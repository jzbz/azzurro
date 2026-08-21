//! The window, and the machinery that keeps it in step with the players.
//!
//! Shape of it: one tokio runtime on its own threads, one long-poll task per
//! player, and a channel of commands going the other way. Nothing touches the
//! UI except through `slint::invoke_from_event_loop`, and nothing in the
//! backend knows what a widget is.
//!
//! The MPRIS bridge in [`mpris`] hangs off the same two things — it reads the
//! statuses the pollers store and writes into the same command channel — so a
//! media key and a click on a button are the same event by the time either
//! reaches a player.

mod mpris;

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bluos::{Client, DeviceId, Discovery, Repeat, Status, discovery::DEFAULT_SWEEP};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::sync::mpsc;

slint::include_modules!();

/// What the window and the desktop can ask the backend to do.
#[derive(Debug)]
enum Command {
    /// Broadcast for players again.
    Rescan,
    /// Do something to one player.
    Player(DeviceId, Action),
}

/// Deliberately coarse: a caller names an intent, and the backend owns every
/// decision about how to reach the player.
#[derive(Debug, Clone, Copy)]
enum Action {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    /// Absolute position, in seconds.
    Seek(u32),
    /// 0 to 100.
    Volume(i32),
    Shuffle(bool),
    Repeat(Repeat),
}

/// One player as the backend tracks it.
struct Entry {
    client: Client,
    /// What the window shows.
    view: Device,
    /// The last status the poller received, which is more than the window
    /// needs but exactly what MPRIS asks for.
    status: Option<Status>,
    /// When that status arrived, so a position can be extrapolated from it.
    status_at: Option<Instant>,
}

type Registry = Arc<Mutex<BTreeMap<DeviceId, Entry>>>;

/// Distinguishes the several MPRIS bus names one process claims.
static NEXT_MPRIS_INDEX: AtomicUsize = AtomicUsize::new(0);

pub fn run_app() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "azzurro_gui=info,bluos=info".into()),
        )
        .without_time()
        .init();

    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("azzurro: {e}");
            ExitCode::FAILURE
        }
    }
}

fn start() -> Result<(), Box<dyn std::error::Error>> {
    let ui = AppWindow::new()?;

    // Held for the lifetime of the window: dropping it would abort every task.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let (commands, command_rx) = mpsc::unbounded_channel();
    runtime.spawn(backend(ui.as_weak(), command_rx, commands.clone()));

    wire(&ui, commands);
    ui.run()?;
    Ok(())
}

/// Point every callback at the command channel.
///
/// A send that fails means the backend is gone, which only happens as the app
/// is shutting down; there is nothing useful to do about it from a click
/// handler, so it is dropped.
fn wire(ui: &AppWindow, commands: mpsc::UnboundedSender<Command>) {
    fn dispatch(
        commands: &mpsc::UnboundedSender<Command>,
        id: &slint::SharedString,
        action: Action,
    ) {
        if let Ok(id) = id.parse() {
            let _ = commands.send(Command::Player(id, action));
        }
    }

    let tx = commands.clone();
    ui.on_toggle(move |id| dispatch(&tx, &id, Action::Toggle));

    let tx = commands.clone();
    ui.on_skip(move |id| dispatch(&tx, &id, Action::Next));

    let tx = commands.clone();
    ui.on_back(move |id| dispatch(&tx, &id, Action::Previous));

    let tx = commands.clone();
    ui.on_set_volume(move |id, level| dispatch(&tx, &id, Action::Volume(level)));

    ui.on_rescan(move || {
        let _ = commands.send(Command::Rescan);
    });
}

async fn backend(
    ui: slint::Weak<AppWindow>,
    command_rx: mpsc::UnboundedReceiver<Command>,
    commands: mpsc::UnboundedSender<Command>,
) {
    // One HTTP client for every player: the connection pool and the resolver
    // are per client, and a controller holds a poll open to each of them.
    let http = match reqwest::Client::builder().build() {
        Ok(http) => http,
        Err(e) => return say(&ui, format!("could not start HTTP: {e}")),
    };

    let discovery = match Discovery::bind() {
        Ok(d) => Arc::new(d),
        Err(e) => {
            return say(
                &ui,
                format!("could not bind the discovery port: {e}. Add players by address instead."),
            );
        }
    };

    let registry: Registry = Arc::new(Mutex::new(BTreeMap::new()));

    tokio::spawn(run_commands(
        command_rx,
        registry.clone(),
        discovery.clone(),
        ui.clone(),
    ));

    say(
        &ui,
        format!(
            "looking for players on {}",
            discovery
                .targets()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );

    // The sweep is only the cold start. Players announce themselves unprompted
    // when they wake, so the same socket keeps listening afterwards and a
    // player switched on an hour later still appears.
    if let Ok(found) = discovery.sweep(DEFAULT_SWEEP).await {
        for announce in found {
            adopt(&announce, &http, &registry, &ui, &commands);
        }
    }

    loop {
        match discovery.recv().await {
            Ok(announces) => {
                for announce in announces {
                    adopt(&announce, &http, &registry, &ui, &commands);
                }
            }
            Err(e) => {
                tracing::warn!("discovery stopped: {e}");
                return;
            }
        }
    }
}

/// Start tracking a player, unless it is already tracked.
fn adopt(
    announce: &bluos::Announce,
    http: &reqwest::Client,
    registry: &Registry,
    ui: &slint::Weak<AppWindow>,
    commands: &mpsc::UnboundedSender<Command>,
) {
    let Some(player) = announce.player() else {
        return;
    };
    let id = DeviceId::new(announce.address, player.port());

    {
        let mut guard = registry.lock().unwrap();
        if guard.contains_key(&id) {
            return;
        }
        // Seed the row from the announcement so the player appears immediately,
        // named, before the first HTTP round trip has finished.
        guard.insert(
            id,
            Entry {
                client: Client::with_http(id, http.clone()),
                view: Device {
                    id: id.to_string().into(),
                    name: player.get("name").unwrap_or("BluOS player").into(),
                    model: player.get("model").unwrap_or_default().into(),
                    reachable: true,
                    ..Default::default()
                },
                status: None,
                status_at: None,
            },
        );
    }

    tracing::info!(%id, "adopted a player");
    publish(registry, ui);
    tokio::spawn(follow(
        id,
        registry.clone(),
        ui.clone(),
        commands.clone(),
        NEXT_MPRIS_INDEX.fetch_add(1, Ordering::Relaxed),
    ));
}

/// Keep one player's row, and its MPRIS object, current for as long as the app
/// runs.
async fn follow(
    id: DeviceId,
    registry: Registry,
    ui: slint::Weak<AppWindow>,
    commands: mpsc::UnboundedSender<Command>,
    mpris_index: usize,
) {
    let Some(client) = with_entry(&registry, id, |e| e.client.clone()) else {
        return;
    };

    // The announcement already gave a name; /SyncStatus gives the authoritative
    // one, and MPRIS wants it before the bus name is claimed so the desktop
    // never shows a placeholder.
    let mut name = with_entry(&registry, id, |e| e.view.name.to_string()).unwrap_or_default();
    if let Ok(sync) = client.sync_status().await {
        name = sync.name.clone();
        let model = sync.display_model().to_owned();
        update(&registry, id, &ui, |view| {
            view.name = sync.name.as_str().into();
            view.model = model.as_str().into();
        });
    }

    let mpris = mpris::Bridge::attach(mpris_index, id, name, registry.clone(), commands).await;

    let mut watch = client.watch();
    let mut backoff = Duration::from_secs(1);

    loop {
        match watch.next().await {
            Ok(status) => {
                backoff = Duration::from_secs(1);

                {
                    let mut guard = registry.lock().unwrap();
                    if let Some(entry) = guard.get_mut(&id) {
                        entry.status = Some(status.clone());
                        entry.status_at = Some(Instant::now());
                    }
                }

                update(&registry, id, &ui, |view| {
                    view.reachable = true;
                    view.playing = status.is_playing();
                    view.muted = status.is_muted();
                    view.volume = status.volume.unwrap_or(0);
                    view.now_playing = status.now_playing().unwrap_or_default().into();
                    view.service = status.service.clone().unwrap_or_default().into();
                });

                if let Some(bridge) = &mpris {
                    bridge.publish(&status).await;
                }
            }
            Err(e) => {
                tracing::debug!(%id, "poll failed, retrying in {backoff:?}: {e}");
                update(&registry, id, &ui, |view| view.reachable = false);

                // The last known track stays on the bus — a blip should not
                // wipe the desktop's media widget — but it stops claiming to
                // be playing something it can no longer see.
                if let Some(bridge) = &mpris {
                    bridge.publish_offline().await;
                }

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));

                // The etag names a state the player may no longer remember
                // after an outage; drop it so the next poll returns at once
                // with whatever is true now.
                watch.forget_etag();
            }
        }
    }
}

async fn run_commands(
    mut commands: mpsc::UnboundedReceiver<Command>,
    registry: Registry,
    discovery: Arc<Discovery>,
    ui: slint::Weak<AppWindow>,
) {
    while let Some(command) = commands.recv().await {
        let (id, action) = match command {
            Command::Rescan => {
                say(&ui, "rescanning");
                let _ = discovery.query().await;
                continue;
            }
            Command::Player(id, action) => (id, action),
        };

        let Some(client) = with_entry(&registry, id, |e| e.client.clone()) else {
            continue;
        };

        // Fire and forget. The player's own long poll is what tells the UI
        // whether it worked, so waiting here would only add latency to the
        // next click.
        tokio::spawn(async move {
            let result = match action {
                Action::Play => client.play().await,
                Action::Pause => client.pause().await,
                Action::Toggle => client.toggle().await,
                Action::Stop => client.stop().await,
                Action::Next => client.skip().await,
                Action::Previous => client.back().await,
                Action::Seek(secs) => client.seek(secs).await,
                Action::Volume(level) => client.set_volume(level).await,
                Action::Shuffle(on) => client.set_shuffle(on).await,
                Action::Repeat(mode) => client.set_repeat(mode).await,
            };
            if let Err(e) = result {
                tracing::warn!(%id, ?action, "command failed: {e}");
            }
        });
    }
}

fn with_entry<T>(registry: &Registry, id: DeviceId, f: impl FnOnce(&Entry) -> T) -> Option<T> {
    registry.lock().unwrap().get(&id).map(f)
}

/// Edit one player's row and push the result to the window.
fn update(
    registry: &Registry,
    id: DeviceId,
    ui: &slint::Weak<AppWindow>,
    f: impl FnOnce(&mut Device),
) {
    {
        let mut guard = registry.lock().unwrap();
        let Some(entry) = guard.get_mut(&id) else {
            return;
        };
        let before = entry.view.clone();
        f(&mut entry.view);
        if entry.view == before {
            return;
        }
    }
    publish(registry, ui);
}

/// Replace the window's device model wholesale.
///
/// Fine while a row is a dozen scalars and a household has a handful of
/// players. Once rows carry decoded artwork this wants to become a `VecModel`
/// held across calls with `row_changed` on the one row that moved, so that a
/// volume nudge does not re-upload every cover on screen.
fn publish(registry: &Registry, ui: &slint::Weak<AppWindow>) {
    let rows: Vec<Device> = registry
        .lock()
        .unwrap()
        .values()
        .map(|e| e.view.clone())
        .collect();

    let line = match rows.len() {
        0 => "no players found yet".to_owned(),
        1 => "1 player".to_owned(),
        n => format!("{n} players"),
    };

    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = ui.upgrade() else { return };

        // Keep the selection on the same player rather than the same index:
        // a player appearing above the selected one would otherwise silently
        // move the selection to its neighbour.
        let selected_id = ui
            .get_devices()
            .row_data(ui.get_selected() as usize)
            .map(|d| d.id);
        let restored = selected_id
            .and_then(|id| rows.iter().position(|d| d.id == id))
            .unwrap_or(0);

        ui.set_devices(ModelRc::new(VecModel::from(rows)));
        ui.set_selected(restored as i32);
        ui.set_status_line(line.into());
    });
}

fn say(ui: &slint::Weak<AppWindow>, message: impl Into<String>) {
    let message = message.into();
    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            ui.set_status_line(message.into());
        }
    });
}
