//! `bluosctl` — the protocol crate with a command line bolted on.
//!
//! This exists mostly to be the fast way to check something against real
//! hardware without opening a window, and it is what the GUI's behavior gets
//! compared against when the two disagree.

use std::time::Duration;

use anyhow::{Context, Result};
use bluos::{Client, DeviceId, Discovery, Repeat, discovery::DEFAULT_SWEEP};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "bluosctl",
    version,
    about = "Control BluOS players from the shell"
)]
struct Cli {
    /// Log at debug level. Discovery in particular is quiet without it.
    #[arg(long, short, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Broadcast for players and list what answers.
    Discover {
        /// How long to keep listening.
        #[arg(long, default_value = "12")]
        seconds: u64,
    },
    /// What a player is: name, model, firmware, grouping.
    Info {
        device: DeviceId,
    },
    /// What a player is doing right now.
    Status {
        device: DeviceId,
    },
    /// Show a page of the player's settings.
    ///
    /// With no page, shows the top level; the ids printed there are what to
    /// pass back in.
    Settings {
        device: DeviceId,
        page: Option<String>,
    },
    /// List the screens this player offers.
    Screens {
        device: DeviceId,
    },
    /// Render one server-driven screen, and show what each row leads to.
    ///
    /// With no path, starts at the Sources screen. Feed a printed `->` URI
    /// back in to walk down the tree.
    Browse {
        device: DeviceId,
        path: Option<String>,
    },
    /// The play queue, with the playing track marked.
    Queue {
        device: DeviceId,
        /// Show only the first N tracks.
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Print a line every time a player changes, until interrupted.
    Watch {
        device: DeviceId,
        /// How long each poll is held open.
        #[arg(long, default_value = "100")]
        poll: u64,
    },
    /// Read the volume, or set it to a level from 0 to 100.
    Volume {
        device: DeviceId,
        level: Option<i32>,
    },
    /// Mute or unmute.
    Mute {
        device: DeviceId,
        #[arg(value_parser = clap::value_parser!(bool))]
        on: bool,
    },
    Play {
        device: DeviceId,
    },
    Pause {
        device: DeviceId,
    },
    /// Play or pause, whichever it is not doing.
    Toggle {
        device: DeviceId,
    },
    Stop {
        device: DeviceId,
    },
    Skip {
        device: DeviceId,
    },
    Back {
        device: DeviceId,
    },
    /// Jump to a position in the current track, in seconds.
    Seek {
        device: DeviceId,
        secs: u32,
    },
    /// Load a stored preset by its number.
    Preset {
        device: DeviceId,
        id: u32,
    },
    Shuffle {
        device: DeviceId,
        // Spelled out because a bare `bool` is a flag to clap, and a flag
        // cannot be positional.
        #[arg(value_parser = clap::value_parser!(bool))]
        on: bool,
    },
    /// all, one or off.
    Repeat {
        device: DeviceId,
        mode: String,
    },
    /// Put a player under another one's control.
    Group {
        master: DeviceId,
        slave: DeviceId,
    },
    /// Break a player out of a group.
    Ungroup {
        master: DeviceId,
        slave: DeviceId,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // reqwest is built with `rustls-no-provider` for the sake of the GUI's
    // cover art, and panics on the first client unless something has chosen a
    // backend. Nothing here speaks https, but the choice is per-process.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| if cli.verbose { "debug" } else { "warn" }.into()),
        )
        .without_time()
        .init();

    match cli.command {
        Command::Discover { seconds } => discover(seconds).await,
        Command::Info { device } => info(device).await,
        Command::Status { device } => status(device).await,
        Command::Queue { device, limit } => queue(device, limit).await,
        Command::Screens { device } => screens(device).await,
        Command::Settings { device, page } => settings(device, page).await,
        Command::Browse { device, path } => browse(device, path).await,
        Command::Watch { device, poll } => watch(device, poll).await,
        Command::Volume { device, level } => volume(device, level).await,
        Command::Mute { device, on } => client(device)?.set_mute(on).await.map_err(Into::into),
        Command::Play { device } => client(device)?.play().await.map_err(Into::into),
        Command::Pause { device } => client(device)?.pause().await.map_err(Into::into),
        Command::Toggle { device } => client(device)?.toggle().await.map_err(Into::into),
        Command::Stop { device } => client(device)?.stop().await.map_err(Into::into),
        Command::Skip { device } => client(device)?.skip().await.map_err(Into::into),
        Command::Back { device } => client(device)?.back().await.map_err(Into::into),
        Command::Seek { device, secs } => client(device)?.seek(secs).await.map_err(Into::into),
        Command::Preset { device, id } => client(device)?.load_preset(id).await.map_err(Into::into),
        Command::Shuffle { device, on } => {
            client(device)?.set_shuffle(on).await.map_err(Into::into)
        }
        Command::Repeat { device, mode } => {
            let mode = match mode.as_str() {
                "all" => Repeat::All,
                "one" => Repeat::One,
                "off" => Repeat::Off,
                other => anyhow::bail!("repeat takes all, one or off, not {other:?}"),
            };
            client(device)?.set_repeat(mode).await.map_err(Into::into)
        }
        Command::Group { master, slave } => {
            client(master)?.add_slave(slave).await.map_err(Into::into)
        }
        Command::Ungroup { master, slave } => client(master)?
            .remove_slave(slave)
            .await
            .map_err(Into::into),
    }
}

fn client(device: DeviceId) -> Result<Client> {
    Client::new(device).with_context(|| format!("building an HTTP client for {device}"))
}

async fn discover(seconds: u64) -> Result<()> {
    let discovery = Discovery::bind().context(
        "binding UDP 11430 — another controller may already hold it without SO_REUSEPORT",
    )?;
    eprintln!(
        "broadcasting to {} for {seconds}s",
        discovery
            .targets()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );

    let window = if seconds == 0 {
        DEFAULT_SWEEP
    } else {
        Duration::from_secs(seconds)
    };
    let found = discovery.sweep(window).await?;

    if found.is_empty() {
        eprintln!("nothing answered. Players asleep, on another subnet, or broadcast filtered.");
        return Ok(());
    }

    for announce in found {
        let Some(player) = announce.player() else {
            continue;
        };
        let id = DeviceId::new(announce.address, player.port());
        println!(
            "{id}\t{}\t{}\t{}",
            player.get("name").unwrap_or("?"),
            player.get("model").unwrap_or("?"),
            player.get("version").unwrap_or("?"),
        );
    }
    Ok(())
}

async fn info(device: DeviceId) -> Result<()> {
    let s = client(device)?.sync_status().await?;
    println!("name      {}", s.name);
    println!("model     {} ({})", s.display_model(), s.model);
    if let Some(brand) = &s.brand {
        println!("brand     {brand}");
    }
    if let Some(version) = &s.version {
        println!("firmware  {version}");
    }
    if let Some(volume) = s.volume {
        println!("volume    {volume}");
    }
    println!("grouped   {}", if s.is_grouped() { "yes" } else { "no" });
    if let Some(zones) = &s.zone_options {
        let positions: Vec<&str> = zones.options.iter().map(|o| o.position.as_str()).collect();
        println!("zones     {}", positions.join(", "));
    }
    Ok(())
}

async fn status(device: DeviceId) -> Result<()> {
    print_status(&client(device)?.status().await?);
    Ok(())
}

async fn watch(device: DeviceId, poll: u64) -> Result<()> {
    let client = client(device)?;
    let mut watch = client.watch().with_poll(Duration::from_secs(poll));
    // Deliberately not retried: a controller backs off and reconnects here, and
    // seeing the raw failure is the point of this subcommand.
    loop {
        let status = watch.next().await?;
        print_status(&status);
        println!("--");
    }
}

fn print_status(s: &bluos::Status) {
    println!("state     {}", s.state.as_deref().unwrap_or("?"));
    if let Some(now) = s.now_playing() {
        println!("playing   {now}");
    }
    if let Some(service) = &s.service {
        println!("service   {service}");
    }
    if let Some(volume) = s.volume {
        println!(
            "volume    {volume}{}",
            if s.is_muted() { " (muted)" } else { "" }
        );
    }
    if let (Some(secs), Some(total)) = (s.secs, s.totlen) {
        println!("position  {secs}s / {total:.0}s");
    }
    if let Some(art) = s.artwork() {
        println!("artwork   {art}");
    }
    println!(
        "can       {}{}",
        if s.can_go_back() { "back " } else { "" },
        if s.can_skip() { "skip" } else { "" }
    );
    println!("etag      {}", s.etag);
}

async fn settings(device: DeviceId, page: Option<String>) -> Result<()> {
    use bluos::settings::{Entry, Kind};

    let page = client(device)?.settings(page.as_deref()).await?;
    println!("served from {}", page.base);
    if let Some(id) = &page.page_id {
        println!("page      {id}");
    }
    println!();

    fn show(entries: &[Entry], page: &bluos::settings::Settings, depth: usize) {
        let pad = "  ".repeat(depth);
        for entry in entries {
            match entry {
                Entry::Group(group) => {
                    println!(
                        "{pad}[{}]{}",
                        group.display_name.as_deref().unwrap_or(&group.id),
                        if group.is_page_link() {
                            format!("  -> settings {}", group.id)
                        } else {
                            String::new()
                        }
                    );
                    show(&group.entries, page, depth + 1);
                }
                Entry::Setting(setting) => {
                    let state = match setting.kind {
                        Kind::Boolean => {
                            format!("[{}]", if setting.is_on() { "on" } else { "off" })
                        }
                        Kind::Button => "(press)".to_owned(),
                        _ => setting
                            .value
                            .clone()
                            .map(|v| format!("= {v}"))
                            .unwrap_or_default(),
                    };
                    let bounds = setting
                        .range
                        .as_ref()
                        .map(|r| {
                            format!("  {}..{}{}", r.min, r.max, r.units.as_deref().unwrap_or(""))
                        })
                        .unwrap_or_default();
                    let gated = if page.is_available(setting) {
                        String::new()
                    } else {
                        let (name, value) = setting.depends_on.clone().unwrap_or_default();
                        format!("  (needs {name}={value})")
                    };
                    println!(
                        "{pad}{:<28} {:<12}{}{}{}",
                        setting.label(),
                        state,
                        bounds,
                        gated,
                        setting
                            .webview
                            .as_deref()
                            .map(|u| format!("  (web page {u})"))
                            .unwrap_or_default(),
                    );
                }
            }
        }
    }

    show(&page.entries, &page, 0);
    Ok(())
}

async fn screens(device: DeviceId) -> Result<()> {
    let config = client(device)?.ui_configuration().await?;
    for item in &config.items {
        println!(
            "{:<22} {}{}",
            item.id,
            item.uri,
            item.result_type
                .as_deref()
                .map(|t| format!("  [{t}]"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

async fn browse(device: DeviceId, path: Option<String>) -> Result<()> {
    let client = client(device)?;

    let path = match path {
        Some(path) => path,
        None => {
            let config = client.ui_configuration().await?;
            config
                .uri("sources")
                .or_else(|| config.uri("home"))
                .unwrap_or("/ui/Sources")
                .to_owned()
        }
    };

    let screen = client.screen(&path).await?;
    let status = client.status().await.ok();

    println!("{}", screen.heading().unwrap_or("(untitled)"));
    if let Some(service) = &screen.service {
        println!("service: {service}");
    }
    for (key, value) in &screen.refresh_on {
        println!("redraw when {key} = {value}");
    }
    for menu in &screen.menu_actions {
        println!(
            "menu: {} -> {}",
            menu.text.as_deref().or(menu.kind.as_deref()).unwrap_or("?"),
            target(menu.action.as_ref())
        );
    }

    for section in &screen.sections {
        println!();
        println!(
            "[{}{}]",
            section.title.as_deref().unwrap_or("—"),
            match section.kind {
                bluos::screen::SectionKind::Row => " (row)",
                bluos::screen::SectionKind::SelectorMenu => " (picker)",
                bluos::screen::SectionKind::List => "",
            }
        );

        for item in &section.items {
            let playing = status.as_ref().is_some_and(|s| item.is_playing(s));
            println!(
                "  {} {:<38} {:<22} {}",
                if playing { "\u{25b6}" } else { " " },
                truncate(item.label().unwrap_or("—"), 38),
                truncate(item.subtitle.as_deref().unwrap_or(""), 22),
                target(item.action.as_ref()),
            );
        }
    }
    Ok(())
}

/// One line describing where an action leads, for the listing above.
fn target(action: Option<&bluos::Action>) -> String {
    use bluos::ActionKind::*;

    let Some(action) = action else {
        return String::new();
    };
    let where_to = action
        .uri
        .as_deref()
        .or(action.href.as_deref())
        .unwrap_or("");

    match action.kind {
        Browse | ContextBrowse => format!("-> {where_to}"),
        PlayerLink => format!("play {where_to}"),
        DeepLink => format!("(client route {where_to})"),
        Webpage | Setting => format!("(browser {where_to})"),
        Add => format!("add {where_to}"),
        Reorder => "reorder".to_owned(),
        Confirmation => format!("play {where_to} (after asking)"),
        Unknown => format!("(unknown action {where_to})"),
    }
}

async fn queue(device: DeviceId, limit: Option<u32>) -> Result<()> {
    let client = client(device)?;
    // Both in one go: the status is what says which row is playing, and
    // whether this queue is the thing playing at all.
    let (queue, status) = tokio::try_join!(
        async {
            match limit {
                Some(n) if n > 0 => client.queue_range(0, n - 1).await,
                _ => client.queue().await,
            }
        },
        client.status()
    )?;

    let cursor = queue.cursor(&status);
    let live = queue.is_live(&status);
    println!("{} tracks in queue {}", queue.length, queue.id.unwrap_or(0));
    if cursor.is_some() && !live {
        println!(
            "(playing {}, not the queue \u{2014} \u{25c7} marks where it would resume)",
            status.service.as_deref().unwrap_or("something else")
        );
    }

    for song in &queue.songs {
        println!(
            "{} {:>4}  {:<44}  {:<26}  {:>7}  {}",
            match (Some(song.id) == cursor, live) {
                (true, true) => "\u{25b6}",
                (true, false) => "\u{25c7}",
                _ => " ",
            },
            song.id,
            truncate(song.title.as_deref().unwrap_or("—"), 44),
            truncate(song.artist.as_deref().unwrap_or(""), 26),
            song.duration().unwrap_or_default(),
            song.quality.as_deref().unwrap_or(""),
        );
    }

    if queue.songs.len() as u32 != queue.length {
        println!("... {} not shown", queue.length - queue.songs.len() as u32);
    }
    Ok(())
}

/// Cut on a character boundary, not a byte one.
fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_owned();
    }
    s.chars().take(width.saturating_sub(1)).collect::<String>() + "\u{2026}"
}

async fn volume(device: DeviceId, level: Option<i32>) -> Result<()> {
    let client = client(device)?;
    match level {
        Some(level) => client.set_volume(level).await?,
        None => {
            let status = client.status().await?;
            println!("{}", status.volume.unwrap_or(-1));
        }
    }
    Ok(())
}
