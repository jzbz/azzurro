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

mod artwork;
mod glyphs;
mod known;
mod mpris;
mod order;

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bluos::screen::{ItemKind, SectionKind};
use bluos::settings::{Entry as SettingEntry, Kind, Settings as SettingsPage};
use bluos::{
    ActionKind, Client, DeviceId, Discovery, Queue, Repeat, Screen, Status, SyncStatus,
    discovery::DEFAULT_SWEEP,
};

use crate::artwork::{Artwork, Pixels};
use crate::glyphs::Glyph;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::sync::mpsc;

slint::include_modules!();

/// The application id, which has to agree with
/// `desktop/blue.azzurro.Azzurro.desktop`.
///
/// On Wayland an application cannot set its own taskbar icon. The compositor
/// matches this id against an installed .desktop file and takes the `Icon=`
/// from there, so a mismatch between the two shows a generic placeholder
/// rather than the icon beside it in this directory.
const APP_ID: &str = "blue.azzurro.Azzurro";

/// How much of a queue to pull for the window.
///
/// Long enough that no ordinary queue is truncated, short enough that pointing
/// the app at a player holding somebody's entire library does not drag a
/// megabyte of XML across the network on every selection. The header says when
/// the view is a window rather than the whole thing.
const QUEUE_WINDOW: u32 = 500;

/// Cover art is drawn into a 180px box, doubled so it stays sharp on a HiDPI
/// screen. Fetched once at this size rather than per scale factor.
const COVER_SIZE: u32 = 360;

/// Queue thumbnails are drawn at 34px, likewise doubled. Browse rows use the
/// same size, so the two panes share cache entries for the same art.
const THUMB_SIZE: u32 = 72;

/// A cover on a shelf is drawn at 116px, and at 72 it would be a smear. The
/// cache is keyed by size, so a screen with both shapes on it fetches both.
const TILE_SIZE: u32 = 232;

/// How long the typing has to stop before a search goes out. Short enough that
/// results feel like they follow the keys, long enough that a typed word is one
/// request rather than eight.
const SEARCH_SETTLE: Duration = Duration::from_millis(280);

/// How many past searches to keep. The player keeps none — this is the app's
/// own list, and it lasts as long as the app is running.
const RECENT_SEARCHES: usize = 8;

/// How many players discovery may adopt on its own.
///
/// Not a limit anyone can reach by owning speakers: a large BluOS install is a
/// few dozen zones, and this is an order of magnitude above that. It bounds
/// what a forged announcement can cost, since each adopted player is a
/// permanent poller and another O(n) rebuild of the list the window draws.
/// Adding a player by hand ignores it — that is a decision, not a broadcast.
const MAX_TRACKED: usize = 256;

/// The longest label that shares a line with the others under the queue.
///
/// Which buttons the player sends varies — Edit only appears once there is
/// something to edit — so counting them is not the way to decide. Save, Edit
/// and Clear are one short word each and fit three across a 330px column;
/// "Queue builder mode" is not, and goes on a line of its own.
const QUEUE_BUTTON_SHORT: usize = 8;

/// What the window and the desktop can ask the backend to do.
#[derive(Debug)]
enum Command {
    /// Broadcast for players again.
    Rescan,
    /// Track a player at an address typed in by hand.
    AddPlayer(String),
    /// Show this player's queue. The window's selection is the backend's cue
    /// to fetch one, since fetching every player's queue would be waste.
    Select(DeviceId),
    /// Do something to one player.
    Player(DeviceId, Action),
    /// Put this player into the selected player's group, or take it out.
    ToggleGroup(DeviceId),
    /// Flip shuffle, reading the current state from the last status rather
    /// than tracking it separately.
    ToggleShuffle(DeviceId),
    /// Flip mute, likewise.
    ToggleMute(DeviceId),
    /// All -> one -> off -> all, the order the official controller cycles in.
    CycleRepeat(DeviceId),
    /// Show the browser's starting screen.
    BrowseHome,
    /// Follow row `n` of the screen currently shown.
    BrowseActivate(usize),
    /// Open row `n`'s context menu.
    BrowseMenu(usize),
    /// Open the context menu for a queue position.
    QueueMenu(u32),
    /// Show the player's settings, or one page of them.
    OpenSettings(Option<String>, Step),
    /// Show the Help menu.
    OpenHelp,
    /// Follow entry `n` of the Help menu.
    HelpAction(usize),
    /// Act on row `n` of the settings page currently shown.
    SettingAction(usize),
    /// Change the value of the setting at row `n`.
    SettingEdit(usize, Edit),
    /// Follow a sidebar entry: `(kind, index)`, where kind 0 is a screen and
    /// kind 1 is an item on the Sources screen.
    Sidebar(i32, i32),
    /// Search the current screen for some text.
    BrowseSearch(String),
    BrowseSearchDone(String),
    /// The button on a section's heading, by the section's place on the screen.
    BrowseSection(usize),
    /// A button on the block at the top of a screen: "Play all", "Shuffle".
    BrowseHeader(usize),
    /// Show the record large, or stop showing it.
    ToggleNowPlaying,
    /// Fetch the next page of the list on show and add it to the end.
    BrowseMore,
    /// Forget every search made this session.
    ClearRecent,
    /// Run one line of the context menu open against a queue row.
    QueueMenuAction(usize),
    /// Show what the playing track actually is: format, rate, bit depth.
    NowPlayingInfo,
    /// Move a section from one place in the Customise list to another.
    CustomiseMove(usize, usize),
    /// Keep the arrangement and go back to the screen it describes.
    CustomiseSave,
    /// Pull every other player into the selected one's group.
    GroupAll,
    /// Stop every player, grouped or not.
    PauseAll,
    /// Read one of the player's web configuration pages and draw it.
    OpenServices,
    OpenShares,
    /// A row of whichever of those is showing.
    WebAction(usize),
    /// Read a form off one of the player's pages and show it.
    OpenForm {
        title: String,
        path: String,
    },
    /// A row of the form on show: a field filled in, or a button pressed.
    FormEdit(usize, Edit),
    FormPress(usize),
    /// Move a track from one place in the queue to another, both as positions
    /// in the queue rather than rows on screen.
    QueueReorder(u32, u32),
    QueueRemove(u32),
    QueueSave(String),
    /// Run whichever of the queue document's own buttons this is.
    QueueButton(usize),
    /// Back one screen.
    BrowseBack,
}

/// Where the browser is, and how it got there.
#[derive(Default)]
struct Browsing {
    /// Which player's screens are being read. Browsing follows the selection,
    /// because a screen is only meaningful against the player that served it.
    device: Option<DeviceId>,
    /// One entry per screen fetched, so Back is a pop.
    trail: Vec<Crumb>,
    /// The screens this player offers: `(label, uri)`, in the order it listed
    /// them. Read once from `/ui/Configuration`.
    screens: Vec<(String, String)>,
    /// Where to ask for a queue item's context menu, also from the player.
    queue_menu_uri: Option<String>,
    /// And the menu for whatever is playing, which is where the technical
    /// details of the current file are offered.
    now_playing_menu: Option<String>,
    /// The player's own Sources screen, kept because the sidebar is drawn from
    /// it: its two rows are the inputs and the music services.
    sources: Option<Screen>,
    /// The play queue's own document, kept for the row of buttons under it.
    ///
    /// The rows come from `/Playlist`, which is smaller and pages cleanly. The
    /// buttons only exist on `/ui/Queue`, and which of them the player offers
    /// is its business — Queue Builder Mode appears there only for a client
    /// that declares a new enough schema.
    queue_screen: Option<Screen>,
    /// The context menu open against a queue row, if one is. Kept whole
    /// because activating a line means running the action the player attached
    /// to it, and that lives on the parsed document.
    queue_menu: Option<Screen>,
    /// The queue screen's own uri, from `/ui/Configuration`.
    queue_uri: Option<String>,
    /// Whether a page of a long list is already on its way, so that scrolling
    /// past the trigger a second time does not ask for it twice.
    fetching_more: bool,
    /// What has been searched for this session, most recent first.
    ///
    /// The player does not keep this — the official controller keeps its own,
    /// which is why the list is empty on a player you have used for years.
    /// Recorded when a search is committed rather than on every keystroke, or
    /// every prefix of every word would be on it.
    recent: Vec<String>,
    /// What the middle pane is showing.
    ///
    /// One field rather than four flags, because it is one fact. As four they
    /// could all be true at once, and a stale one routed a press to a pane that
    /// was not on screen: with the services list left set, every row of the
    /// settings page opened a music service's sign-in page instead.
    pane: Pane,
    /// Which sidebar entry is lit, as `(kind, index)`. Recorded on activation
    /// rather than inferred, because a screen can be reached several ways.
    highlighted: Option<(i32, i32)>,
}

impl Pane {
    /// The settings page on show, if that is what is.
    fn settings(&self) -> Option<&SettingsPage> {
        match self {
            Pane::Settings(trail) => trail.last(),
            _ => None,
        }
    }

    /// The web configuration page on show, if that is what is.
    fn web(&self) -> Option<&WebPage> {
        match self {
            Pane::Web(page) => Some(page),
            _ => None,
        }
    }

    /// The form being filled in, if one is.
    fn form(&self) -> Option<&FormPage> {
        match self {
            Pane::Form(page) => Some(page),
            _ => None,
        }
    }
}

/// What the middle pane is showing. These are alternatives rather than layers:
/// opening any of them is a change of mode, and Back leaves it.
#[derive(Default)]
enum Pane {
    /// The player's own screens, which is where the app spends its life.
    #[default]
    Browse,
    /// The settings pages open, deepest last. A trail rather than one page,
    /// so that Back out of Audio lands on Settings instead of leaving
    /// altogether — which is what "back one level" has to mean here too.
    Settings(Vec<SettingsPage>),
    Help,
    /// A page reached from the Help menu: its title and the facts on it.
    HelpDetail(String, Vec<(String, String)>),
    /// One of the player's web configuration pages, drawn rather than opened.
    Web(WebPage),
    /// A form off one of those pages, filled in here rather than in a browser.
    Form(Box<FormPage>),
    /// The record, large. Reached by pressing the artwork on the transport bar,
    /// which is where the official controller puts it too.
    NowPlaying,
    /// Rearranging the sections of a screen — "Customise Home".
    Customise(CustomisePage),
}

/// A screen being rearranged.
struct CustomisePage {
    /// Whose order is being edited, which is the key the preference is filed
    /// under.
    screen: String,
    title: String,
    /// The movable sections, `(id, title)`, in the order they would be saved
    /// in. Sections the player pinned are not here at all: they cannot move,
    /// so offering to move them would be a lie.
    rows: Vec<(String, String)>,
}

/// A form being filled in.
struct FormPage {
    title: String,
    form: bluos::forms::Form,
    /// What has been typed or chosen, by field name. Seeded from what the page
    /// arrived with, so a field left alone goes back exactly as it came.
    values: BTreeMap<String, String>,
    /// Whatever the player said in reply to the last attempt — a wrong password
    /// comes back as the same form with a sentence on it.
    note: String,
    /// The page this was opened from, kept so Back can put it back.
    ///
    /// Signing into a music service is reached by picking one out of the
    /// player's own list of them, and a form that forgot where it came from
    /// sent Back to the browse screen instead — one service configured, and
    /// the list you were working through is gone.
    from: Option<WebPage>,
}

struct Crumb {
    uri: String,
    screen: Screen,
    /// What was typed to reach this screen, when it is a set of search results.
    ///
    /// Typing runs a search on every keystroke, and each one would otherwise be
    /// a step on the trail: Back out of "van halen" and you would land in "van
    /// hale". Results replace results instead, so Back returns to where the
    /// searching started — and the query is kept because that is what goes on
    /// the recent list once the search turns out to have been worth making.
    query: Option<String>,
}

/// How a settings page joins the pane's own trail.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    /// Start again here: opening Settings, or arriving from a Customise button
    /// on a screen somewhere else.
    Root,
    /// Opened from the page showing, which Back undoes.
    Deeper,
    /// The same page again. A write can change more than the value written —
    /// turning tone controls on brings treble and bass to life — so the page is
    /// re-read rather than assumed.
    Reload,
}

/// How a newly fetched screen joins the trail.
#[derive(Debug, Clone, PartialEq)]
enum Arrive {
    /// A top-level screen off the sidebar: everything before it is gone.
    Root,
    /// A step deeper, which Back undoes.
    Deeper,
    /// A set of search results: pushed the first time, and thereafter in place
    /// of the last set.
    Found(String),
    /// In place of the screen it came from, which is what a service picker
    /// marked `replaceScreen` asks for.
    Replace(Option<String>),
}

impl Browsing {
    fn current(&self) -> Option<&Screen> {
        self.trail.last().map(|c| &c.screen)
    }
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
    /// Jump to a position in the play queue.
    PlayQueueIndex(u32),
    /// 0 to 100.
    Volume(i32),
    Shuffle(bool),
    Repeat(Repeat),
    Mute(bool),
    /// Advance the sleep timer one step along the player's own ladder.
    Sleep,
}

/// One player as the backend tracks it.
struct Entry {
    client: Client,
    /// What the window shows in the player list.
    view: Device,
    /// The last status the poller received, which is more than the list needs
    /// but exactly what MPRIS and the queue view ask for.
    status: Option<Status>,
    /// When that status arrived, so a position can be extrapolated from it.
    status_at: Option<Instant>,
    /// Fetched on demand, and dropped when the player says the queue was
    /// replaced.
    queue: Option<Queue>,
    /// The player as it describes itself, including who it is grouped with.
    /// Re-read whenever `/Status` reports a new `syncStat`, which is the
    /// player's way of saying its grouping changed.
    sync: Option<SyncStatus>,
    /// The absolute URL of the art for whatever is playing, resolved against
    /// this player. Kept so that a status carrying the same art twice does not
    /// start the fetch again, and so a fetch that lands late can tell whether
    /// it is still wanted.
    cover_url: Option<String>,
}

type Registry = Arc<Mutex<BTreeMap<DeviceId, Entry>>>;

/// Everything the background tasks share. Cheap to clone; all of it is behind
/// an `Arc` already.
///
/// **Locking rule: never hold two of these at once.** Each of `registry`,
/// `selected` and `browsing` is taken, used, and released before the next is
/// touched — usually by copying the one value needed out first, which is why
/// several functions here read a `DeviceId` in its own statement before doing
/// anything else. Held singly there is no order to get wrong and nothing to
/// deadlock; the moment two are nested, every future caller has to know which
/// way round, and one that gets it backwards hangs the whole app.
#[derive(Clone)]
struct Backend {
    registry: Registry,
    /// Whose queue the window is showing.
    selected: Arc<Mutex<Option<DeviceId>>>,
    commands: mpsc::UnboundedSender<Command>,
    ui: slint::Weak<AppWindow>,
    artwork: Arc<Artwork>,
    browsing: Arc<Mutex<Browsing>>,
    /// Addresses that have answered, remembered between runs.
    known: Arc<Mutex<BTreeSet<DeviceId>>>,
    /// How many searches have been asked for. Typing asks for one per
    /// keystroke and only the last of them should reach the player, so each
    /// takes a number on the way in and checks it is still the highest on the
    /// way out.
    searches: Arc<AtomicU64>,
    /// What the transport looked like when it was last sent.
    sent_transport: Arc<AtomicU64>,
    /// What the queue and the player list looked like when they were last sent
    /// to the window.
    ///
    /// Both are rebuilt on every status change, and a playing track changes its
    /// status once a second. Replacing a model re-creates every row in it and
    /// restarts the transitions on them, so sending an identical one is a
    /// second of repainting for nothing — measured at eleven frames a second
    /// with the app sitting idle.
    sent_queue: Arc<AtomicU64>,
    sent_players: Arc<AtomicU64>,
    /// Section order per screen, as set by Customise Home and kept on disk.
    orders: Arc<Mutex<order::Orders>>,
}

/// What to print on a format badge.
///
/// The player says `hd` for anything above CD, and the official controller
/// shows that as HR — high resolution — which is what the word means on a
/// hi-fi. Everything else is already the abbreviation it should be: `cd`,
/// `mqa`.
fn quality_label(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => String::new(),
        "hd" => "HR".to_owned(),
        other => other.to_ascii_uppercase(),
    }
}

/// Whether this is the same thing the window already has.
///
/// Hashed rather than compared, because the rows carry decoded artwork and
/// comparing that is more expensive than the redraw it would save. Every field
/// that is drawn goes in; the artwork only as whether it has arrived, which is
/// the only thing about it that can change once it has.
fn already_sent(seen: &AtomicU64, fingerprint: u64) -> bool {
    // Zero is the "nothing sent yet" value, so a fingerprint that happens to
    // be zero simply sends once more than it needs to.
    fingerprint != 0 && seen.swap(fingerprint, Ordering::Relaxed) == fingerprint
}

/// One browse row on its way to the window, for the same reason as
/// [`TrackData`]: the pixels are `Send` and the `Image` is not.
struct BrowseData {
    index: i32,
    title: String,
    subtitle: String,
    /// Its place on the record, and what the player says it is encoded as.
    /// Both empty except on a screen about one album.
    track: String,
    quality: String,
    /// The caption on a section heading's button — "Customise" on the Inputs
    /// row, "Manage" on Music Services. Empty everywhere else, which is most
    /// places: only the sidebar draws these, and only on headings.
    action: String,
    cover: Option<Pixels>,
    /// Drawn instead of `cover` where the player's own picture is interface
    /// furniture rather than content. See [`glyphs`].
    glyph: Option<Glyph>,
    heading: bool,
    actionable: bool,
    playing: bool,
    /// Which service a selector menu is currently showing. Not the same thing
    /// as `playing`: you can be looking at TuneIn's favourites while the
    /// library is what is coming out of the speakers.
    selected: bool,
    has_menu: bool,
}

/// A page of the player's web UI that is worth drawing rather than opening.
///
/// Both are lists. What sits behind a row of either is not: signing into a
/// music service asks for a password and sometimes a captcha, and adding a
/// share asks for a server and credentials, so those stay pages to open.
enum WebPage {
    Services(Vec<bluos::reports::Service>),
    Shares {
        /// Where a removal is posted, as the page's own form gives it.
        action: Option<String>,
        shares: Vec<bluos::reports::Share>,
    },
}

/// What a screen about one thing says about it, on its way to the window.
struct HeaderData {
    cover: Option<Pixels>,
    title: String,
    subtitle: String,
    detail: String,
    buttons: Vec<(i32, String, Option<Glyph>)>,
}

/// One section of a screen, ready for the window.
struct BlockData {
    /// 0 = a plain list, 1 = a shelf of tiles, 2 = a strip of service chips.
    kind: i32,
    title: String,
    /// The caption on this section's own button, empty where it has none.
    action: String,
    /// Which section of the screen this came from. Not the block's position:
    /// empty sections and the service picker never become blocks.
    section: i32,
    rows: Vec<BrowseData>,
}

/// The Lucide glyph for a [`Glyph`], out of the set the .slint file holds.
fn glyph_image(icons: &Icons<'_>, glyph: Glyph) -> slint::Image {
    match glyph {
        Glyph::Bluetooth => icons.get_bluetooth(),
        Glyph::Tv => icons.get_tv(),
        Glyph::Cable => icons.get_cable(),
        Glyph::Usb => icons.get_usb(),
        Glyph::Playlist => icons.get_playlist(),
        Glyph::Library => icons.get_library(),
        Glyph::Radio => icons.get_radio(),
        Glyph::Station => icons.get_station(),
        Glyph::Favourite => icons.get_favourite(),
        Glyph::Preset => icons.get_preset(),
        Glyph::Search => icons.get_search(),
        Glyph::Album => icons.get_album(),
        Glyph::Artist => icons.get_artist(),
        Glyph::Track => icons.get_track(),
        Glyph::Genre => icons.get_genre(),
        Glyph::Folder => icons.get_folder(),
        Glyph::Recent => icons.get_recent(),
        Glyph::Add => icons.get_add(),
        Glyph::Home => icons.get_home(),
        Glyph::News => icons.get_news(),
        Glyph::Sources => icons.get_sources(),
        Glyph::Play => icons.get_play(),
        Glyph::Shuffle => icons.get_shuffle(),
        Glyph::Info => icons.get_info(),
        Glyph::Details => icons.get_details(),
        Glyph::Enqueue => icons.get_enqueue(),
        Glyph::Unfavourite => icons.get_unfavourite(),
        Glyph::Clear => icons.get_clear(),
        Glyph::Save => icons.get_save(),
        Glyph::Settings => icons.get_settings(),
        Glyph::Tweak => icons.get_tweak(),
        Glyph::Alarm => icons.get_alarm(),
        Glyph::Sleep => icons.get_sleep(),
        Glyph::Speaker => icons.get_player(),
        Glyph::Volume => icons.get_volume(),
        Glyph::Wifi => icons.get_wifi(),
        Glyph::Network => icons.get_network(),
        Glyph::Artwork => icons.get_artwork(),
        Glyph::Power => icons.get_power(),
        Glyph::Brightness => icons.get_brightness(),
        Glyph::Reset => icons.get_reset(),
        Glyph::Server => icons.get_server(),
        Glyph::Tone => icons.get_tone(),
        Glyph::Gauge => icons.get_gauge(),
        Glyph::Service => icons.get_service(),
        Glyph::Edit => icons.get_edit(),
        Glyph::Secret => icons.get_secret(),
        Glyph::ForYou => icons.get_for_you(),
        Glyph::Sport => icons.get_sport(),
        Glyph::Podcast => icons.get_podcast(),
        Glyph::Trending => icons.get_trending(),
        Glyph::Language => icons.get_language(),
        Glyph::Local => icons.get_local(),
        Glyph::Place => icons.get_place(),
        Glyph::Rescan => icons.get_rescan(),
    }
}

/// One queue row on its way to the window.
///
/// This exists because [`slint::Image`] is not `Send` and the rows are built on
/// a worker thread: the pixels travel, and the `Image` is assembled inside the
/// event loop.
struct TrackData {
    id: i32,
    title: String,
    artist: String,
    duration: String,
    quality: String,
    cursor: bool,
    live: bool,
    cover: Option<Pixels>,
}

/// One of the buttons the player puts under the play queue, on its way to the
/// window.
struct QueueButtonData {
    index: i32,
    label: String,
    glyph: Option<Glyph>,
    highlight: bool,
    mode: i32,
    question: String,
}

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

    // Selecting the backend has to come first. Slint initialises its platform
    // lazily on first use, and `set_xdg_app_id` needs one already there —
    // called before anything else it fails with "No default Slint platform was
    // selected", silently leaving the window without an id. Then the id, which
    // has to be set before the window is built because that is when it is read.
    if let Err(e) = slint::BackendSelector::new().select() {
        eprintln!("azzurro: could not select a rendering backend: {e}");
    }
    if let Err(e) = slint::set_xdg_app_id(APP_ID) {
        eprintln!("azzurro: could not set the application id: {e}");
    }

    // reqwest is built with `rustls-no-provider`, which leaves choosing the
    // crypto backend to the application. ring rather than aws-lc-rs: it is what
    // the rest of this author's Rust already uses, and it does not want cmake.
    // An error here means something else installed one first, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();

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
    runtime.spawn(run(ui.as_weak(), command_rx, commands.clone()));

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

    let tx = commands.clone();
    ui.on_play_track(move |id, index| {
        dispatch(&tx, &id, Action::PlayQueueIndex(index.max(0) as u32))
    });

    let tx = commands.clone();
    ui.on_select(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::Select(id));
        }
    });

    let tx = commands.clone();
    ui.on_browse_home(move || {
        let _ = tx.send(Command::BrowseHome);
    });

    let tx = commands.clone();
    ui.on_browse_back(move || {
        let _ = tx.send(Command::BrowseBack);
    });

    let tx = commands.clone();
    ui.on_browse_activate(move |index| {
        let _ = tx.send(Command::BrowseActivate(index.max(0) as usize));
    });

    let tx = commands.clone();
    ui.on_browse_menu(move |index| {
        let _ = tx.send(Command::BrowseMenu(index.max(0) as usize));
    });

    let tx = commands.clone();
    // The row's position is the window's business, not the backend's: it has
    // already been written to `queue-menu-y` by the time this runs.
    ui.on_queue_menu(move |song, _at| {
        let _ = tx.send(Command::QueueMenu(song.max(0) as u32));
    });

    let tx = commands.clone();
    ui.on_queue_menu_activate(move |at| {
        let _ = tx.send(Command::QueueMenuAction(at.max(0) as usize));
    });

    let tx = commands.clone();
    ui.on_setting_toggle(move |index| {
        let _ = tx.send(Command::SettingEdit(index.max(0) as usize, Edit::Toggle));
    });

    let tx = commands.clone();
    ui.on_setting_choose(move |index, option| {
        let _ = tx.send(Command::SettingEdit(
            index.max(0) as usize,
            Edit::Choose(option.max(0) as usize),
        ));
    });

    let tx = commands.clone();
    ui.on_setting_number(move |index, value| {
        let _ = tx.send(Command::SettingEdit(
            index.max(0) as usize,
            Edit::Number(value),
        ));
    });

    let tx = commands.clone();
    ui.on_setting_text(move |index, text| {
        let _ = tx.send(Command::SettingEdit(
            index.max(0) as usize,
            Edit::Text(text.to_string()),
        ));
    });

    let tx = commands.clone();
    ui.on_setting_open(move |index| {
        let _ = tx.send(Command::SettingAction(index.max(0) as usize));
    });

    let tx = commands.clone();
    ui.on_open_settings(move || {
        let _ = tx.send(Command::OpenSettings(None, Step::Root));
    });

    let tx = commands.clone();
    ui.on_open_help(move || {
        let _ = tx.send(Command::OpenHelp);
    });

    let tx = commands.clone();
    ui.on_sidebar_activate(move |kind, index| {
        let _ = tx.send(Command::Sidebar(kind, index));
    });

    let tx = commands.clone();
    ui.on_browse_search(move |query| {
        let _ = tx.send(Command::BrowseSearch(query.to_string()));
    });

    let tx = commands.clone();
    ui.on_queue_reorder(move |from, to| {
        if from >= 0 && to >= 0 {
            let _ = tx.send(Command::QueueReorder(from as u32, to as u32));
        }
    });

    let tx = commands.clone();
    ui.on_queue_remove(move |id| {
        if id >= 0 {
            let _ = tx.send(Command::QueueRemove(id as u32));
        }
    });

    let tx = commands.clone();
    ui.on_queue_save(move |name| {
        let _ = tx.send(Command::QueueSave(name.to_string()));
    });

    let tx = commands.clone();
    ui.on_queue_button(move |index| {
        if index >= 0 {
            let _ = tx.send(Command::QueueButton(index as usize));
        }
    });

    let tx = commands.clone();
    ui.on_customise_move(move |from, to| {
        let _ = tx.send(Command::CustomiseMove(
            from.max(0) as usize,
            to.max(0) as usize,
        ));
    });

    let tx = commands.clone();
    ui.on_customise_save(move || {
        let _ = tx.send(Command::CustomiseSave);
    });

    let tx = commands.clone();
    ui.on_group_all(move || {
        let _ = tx.send(Command::GroupAll);
    });

    let tx = commands.clone();
    ui.on_pause_all(move || {
        let _ = tx.send(Command::PauseAll);
    });

    let tx = commands.clone();
    ui.on_now_playing_info(move || {
        let _ = tx.send(Command::NowPlayingInfo);
    });

    let tx = commands.clone();
    ui.on_clear_recent(move || {
        let _ = tx.send(Command::ClearRecent);
    });

    let tx = commands.clone();
    ui.on_browse_more(move || {
        let _ = tx.send(Command::BrowseMore);
    });

    let tx = commands.clone();
    ui.on_toggle_now_playing(move || {
        let _ = tx.send(Command::ToggleNowPlaying);
    });

    let tx = commands.clone();
    ui.on_browse_header(move |at| {
        if at >= 0 {
            let _ = tx.send(Command::BrowseHeader(at as usize));
        }
    });

    let tx = commands.clone();
    ui.on_browse_section(move |section| {
        if section >= 0 {
            let _ = tx.send(Command::BrowseSection(section as usize));
        }
    });

    let tx = commands.clone();
    ui.on_browse_search_done(move |query| {
        let _ = tx.send(Command::BrowseSearchDone(query.to_string()));
    });

    let tx = commands.clone();
    ui.on_seek(move |id, secs| dispatch(&tx, &id, Action::Seek(secs.max(0) as u32)));

    let tx = commands.clone();
    ui.on_toggle_shuffle(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::ToggleShuffle(id));
        }
    });

    let tx = commands.clone();
    ui.on_toggle_mute(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::ToggleMute(id));
        }
    });

    let tx = commands.clone();
    ui.on_cycle_repeat(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::CycleRepeat(id));
        }
    });

    let tx = commands.clone();
    ui.on_toggle_group(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::ToggleGroup(id));
        }
    });

    let tx = commands.clone();
    ui.on_add_player(move |text| {
        let _ = tx.send(Command::AddPlayer(text.to_string()));
    });

    ui.on_rescan(move || {
        let _ = commands.send(Command::Rescan);
    });
}

async fn run(
    ui: slint::Weak<AppWindow>,
    command_rx: mpsc::UnboundedReceiver<Command>,
    commands: mpsc::UnboundedSender<Command>,
) {
    // One HTTP client for every player: the connection pool and the resolver
    // are per client, and a controller holds a poll open to each of them.
    // Two clients on purpose. `http` talks to players and will not follow a
    // redirect off one; `art` fetches covers, which legitimately come from a
    // streaming service's CDN on another host. reqwest fixes the redirect
    // policy per client rather than per request, so telling the two apart
    // means having two. The cost is one extra TCP connection per host the
    // second one talks to, and those are hosts the first never touches.
    let http = match bluos::client::http_client() {
        Ok(http) => http,
        Err(e) => return say(&ui, format!("could not start HTTP: {e}")),
    };
    let art = match reqwest::Client::builder().build() {
        Ok(art) => art,
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

    let backend = Backend {
        registry: Arc::new(Mutex::new(BTreeMap::new())),
        selected: Arc::new(Mutex::new(None)),
        commands,
        ui: ui.clone(),
        artwork: Arc::new(Artwork::new(art)),
        browsing: Arc::new(Mutex::new(Browsing::default())),
        known: Arc::new(Mutex::new(BTreeSet::new())),
        searches: Arc::new(AtomicU64::new(0)),
        sent_transport: Arc::new(AtomicU64::new(0)),
        sent_queue: Arc::new(AtomicU64::new(0)),
        sent_players: Arc::new(AtomicU64::new(0)),
        orders: Arc::new(Mutex::new(order::load())),
    };

    tokio::spawn(run_commands(
        command_rx,
        backend.clone(),
        discovery.clone(),
        http.clone(),
    ));
    tokio::spawn(tick_position(backend.clone()));

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

    // Remembered players first: one that was asleep when the app opened will
    // never announce itself, and a broadcast does not always arrive. Addresses
    // from another network are skipped rather than quietly timing out.
    let remembered = tokio::task::spawn_blocking(known::load)
        .await
        .unwrap_or_default();

    // Seeded with everything read, not only what answers: an address pinned by
    // hand — for a player on the far side of a router — has to survive the next
    // save, and a player that is merely switched off today should still be
    // there tomorrow.
    *backend.known.lock().unwrap() = remembered.clone();

    let (here, elsewhere): (Vec<_>, Vec<_>) = remembered
        .into_iter()
        .partition(|id| bluos::discovery::is_local(id.host));
    if !elsewhere.is_empty() {
        tracing::debug!(
            "{} remembered players are on another network",
            elsewhere.len()
        );
    }
    for id in here {
        backend.track(id, &http, None, None);
    }

    // The sweep is only the cold start. Players announce themselves unprompted
    // when they wake, so the same socket keeps listening afterwards and a
    // player switched on an hour later still appears.
    sweep(backend.clone(), discovery.clone(), http.clone()).await;

    loop {
        match discovery.recv().await {
            Ok(announces) => {
                for announce in announces {
                    backend.adopt(&announce, &http);
                }
            }
            Err(e) => {
                tracing::warn!("discovery stopped: {e}");
                return;
            }
        }
    }
}

impl Backend {
    /// Start tracking a player found by discovery.
    fn adopt(&self, announce: &bluos::Announce, http: &reqwest::Client) {
        let Some(player) = announce.player() else {
            return;
        };

        // The address is what the packet *says*, not where it came from: the
        // datagram's source is discarded before this, and LSDP is an
        // unauthenticated broadcast anyone on the network can send. So the
        // announcement is treated as a claim rather than a fact.
        //
        // The same test the remembered players get at startup, for the same
        // reason and with the same helper. No genuine player can fail it — its
        // announcement reached this machine over its own broadcast domain, so
        // it is on one of these interfaces by construction — while a forged
        // one naming 127.0.0.1, a metadata endpoint, or a host across the
        // internet is refused. An address typed in by hand is not gated: that
        // is someone deciding, which is a different thing entirely.
        if !bluos::discovery::is_local(announce.address.into()) {
            tracing::debug!(
                address = %announce.address,
                "ignoring an announcement for an address on no local network"
            );
            return;
        }

        // A cap on top, because a subnet is not itself small: one datagram can
        // carry dozens of identities and a wide netmask leaves room for
        // thousands. Every one of them would be a permanent poller and another
        // O(n) rebuild of the player list. Far above any real system — the
        // largest BluOS install is a few dozen zones — so the only thing this
        // can turn away is a flood.
        let id = DeviceId::new(announce.address, player.port());
        {
            let registry = self.registry.lock().unwrap();
            if registry.len() >= MAX_TRACKED && !registry.contains_key(&id) {
                tracing::warn!("ignoring an announcement: already tracking {MAX_TRACKED} players");
                return;
            }
        }

        self.track(id, http, player.get("name"), player.get("model"));
    }

    /// Start tracking a player, unless it is already tracked.
    ///
    /// The name and model are only hints — from an announcement, or absent
    /// entirely for an address typed in by hand. `/SyncStatus` replaces them
    /// with the truth a moment later.
    fn track(&self, id: DeviceId, http: &reqwest::Client, name: Option<&str>, model: Option<&str>) {
        {
            let mut guard = self.registry.lock().unwrap();
            if guard.contains_key(&id) {
                return;
            }
            // Seed the row from the announcement so the player appears
            // immediately, named, before the first HTTP round trip finishes.
            guard.insert(
                id,
                Entry {
                    client: Client::with_http(id, http.clone()),
                    view: Device {
                        id: id.to_string().into(),
                        name: name.unwrap_or("BluOS player").into(),
                        model: model.unwrap_or_default().into(),
                        reachable: true,
                        ..Default::default()
                    },
                    status: None,
                    status_at: None,
                    queue: None,
                    sync: None,
                    cover_url: None,
                },
            );
        }

        // The window starts with the first row selected, so the backend has to
        // agree with it or the queue panel stays empty until something is
        // clicked.
        let first = {
            let mut selected = self.selected.lock().unwrap();
            if selected.is_none() {
                *selected = Some(id);
                true
            } else {
                false
            }
        };

        tracing::info!(%id, "adopted a player");
        self.publish();
        if first {
            tokio::spawn(fetch_queue(self.clone(), id));
            // The window selects the first row for us, so the backend has to
            // do everything selecting normally would.
            let _ = self.commands.send(Command::BrowseHome);
        }
        tokio::spawn(follow(
            self.clone(),
            id,
            NEXT_MPRIS_INDEX.fetch_add(1, Ordering::Relaxed),
        ));
    }

    fn with_entry<T>(&self, id: DeviceId, f: impl FnOnce(&Entry) -> T) -> Option<T> {
        self.registry.lock().unwrap().get(&id).map(f)
    }

    fn is_selected(&self, id: DeviceId) -> bool {
        *self.selected.lock().unwrap() == Some(id)
    }

    /// Edit one player's row and push the result to the window.
    fn update(&self, id: DeviceId, f: impl FnOnce(&mut Device)) {
        {
            let mut guard = self.registry.lock().unwrap();
            let Some(entry) = guard.get_mut(&id) else {
                return;
            };
            let before = entry.view.clone();
            f(&mut entry.view);
            if entry.view == before {
                return;
            }
        }
        self.publish();
    }

    /// Replace the window's device model wholesale.
    ///
    /// Fine while a row is a dozen scalars and a household has a handful of
    /// players. Once rows carry decoded artwork this wants to become a
    /// `VecModel` held across calls with `row_changed` on the one row that
    /// moved, so that a volume nudge does not re-upload every cover on screen.
    fn publish(&self) {
        // Selection first, then the registry: two locks, always in that order,
        // everywhere.
        let selected = *self.selected.lock().unwrap();

        let rows: Vec<Device> = {
            let guard = self.registry.lock().unwrap();

            // A follower's row names its leader, so every row needs the others.
            let names: BTreeMap<DeviceId, String> = guard
                .iter()
                .map(|(id, entry)| (*id, entry.view.name.to_string()))
                .collect();

            guard
                .iter()
                .map(|(id, entry)| {
                    let mut view = entry.view.clone();
                    let sync = entry.sync.as_ref();

                    view.role = match sync {
                        Some(sync) if sync.is_master() => {
                            let n = sync.slaves.len();
                            format!("Leading {n} player{}", if n == 1 { "" } else { "s" }).into()
                        }
                        Some(sync) if sync.is_slave() => {
                            // A follower that has lost its leader is not
                            // playing anything, and should not look as if it is.
                            if sync.master.as_ref().is_some_and(|m| m.is_reconnecting()) {
                                "Reconnecting…".into()
                            } else {
                                let master = sync.master_id();
                                let who = master
                                    .and_then(|m| names.get(&m).cloned())
                                    .or_else(|| master.map(|m| m.host.to_string()))
                                    .unwrap_or_default();
                                format!("Following {who}").into()
                            }
                        }
                        _ => Default::default(),
                    };

                    view.in_group =
                        selected.is_some() && sync.and_then(|sync| sync.master_id()) == selected;
                    // Grouping is something you do *to another* player, so the
                    // selected row does not offer it against itself.
                    view.groupable = selected.is_some_and(|sel| sel != *id);
                    view
                })
                .collect()
        };

        let line = match rows.len() {
            0 => "no players found yet".to_owned(),
            1 => "1 player".to_owned(),
            n => format!("{n} players"),
        };

        if already_sent(&self.sent_players, {
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            line.hash(&mut hash);
            for row in &rows {
                row.id.hash(&mut hash);
                row.name.hash(&mut hash);
                row.now_playing.hash(&mut hash);
                row.title.hash(&mut hash);
                row.artist.hash(&mut hash);
                row.album.hash(&mut hash);
                row.volume.hash(&mut hash);
                row.muted.hash(&mut hash);
                row.playing.hash(&mut hash);
                row.reachable.hash(&mut hash);
                row.role.hash(&mut hash);
                row.in_group.hash(&mut hash);
                row.groupable.hash(&mut hash);
            }
            hash.finish()
        }) {
            return;
        }

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };

            // Keep the selection on the same player rather than the same
            // index: a player appearing above the selected one would otherwise
            // silently move the selection to its neighbour.
            let selected_id = ui
                .get_devices()
                .row_data(ui.get_selected() as usize)
                .map(|d| d.id);
            let restored = selected_id
                .and_then(|id| rows.iter().position(|d| d.id == id))
                .unwrap_or(0);

            ui.set_devices(ModelRc::new(VecModel::from(rows)));
            ui.set_selected(restored as i32);
            // Not the toast. This is a standing fact about the system rather
            // than something that just happened, and it was being republished
            // on every status change — which, once the toast was drawn, meant
            // "1 player" appearing over the window at every new song.
            ui.set_players_line(line.into());
        });
    }

    /// Push the selected player's queue to the window.
    /// Send the open queue-row menu to the window, and open it.
    ///
    /// The artwork is asked for at thumbnail size and only from what is
    /// already decoded — the same cover is in the row underneath, so a menu
    /// that waited on a fetch would open blank for no reason.
    fn publish_queue_menu(&self) {
        let (title, subtitle, cover, rows) = {
            let browsing = self.browsing.lock().unwrap();
            let Some(menu) = &browsing.queue_menu else {
                return;
            };

            // `(index, label, glyph)` rather than the Slint struct: a
            // `slint::Image` cannot be made off the UI thread, so the glyph
            // stays an enum until the closure below.
            let rows: Vec<(i32, String, Glyph)> = menu
                .items()
                .enumerate()
                .filter_map(|(at, item)| {
                    let label = item.title.clone()?;
                    let glyph = glyphs::menu_glyph(&label);
                    Some((at as i32, label, glyph))
                })
                .collect();

            (
                menu.heading().unwrap_or_default().to_owned(),
                menu.subtitle.clone().unwrap_or_default(),
                menu.image.clone(),
                rows,
            )
        };

        if rows.is_empty() {
            say(&self.ui, "The player offers nothing for that track");
            return;
        }

        let art = self.artwork.clone();
        let client = self
            .browsing
            .lock()
            .unwrap()
            .device
            .and_then(|id| self.with_entry(id, |e| e.client.clone()));
        let cover = cover
            .as_deref()
            .zip(client.as_ref())
            .and_then(|(src, client)| art.cached(&client.image_url(src), THUMB_SIZE));

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            let icons = Icons::get(&ui);
            let rows: Vec<ActionButton> = rows
                .into_iter()
                .map(|(index, label, glyph)| ActionButton {
                    index,
                    label: label.into(),
                    glyph: glyph_image(&icons, glyph),
                    primary: false,
                })
                .collect();

            ui.set_queue_menu_title(title.into());
            ui.set_queue_menu_subtitle(subtitle.into());
            ui.set_queue_menu_cover(cover.map(slint::Image::from_rgba8).unwrap_or_default());
            ui.set_queue_menu_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
            ui.set_queue_menu_open(true);
        });
    }

    fn publish_queue(&self) {
        // Read the selection out before taking the registry lock, so the two
        // are never held at once and there is no order to get wrong.
        let selected = *self.selected.lock().unwrap();

        // What the player offers to do with the queue. Recognised by what each
        // action is rather than by what it says: matching on the label is what
        // the official controller does and it would break on the first player
        // set to another language.
        let buttons: Vec<QueueButtonData> = {
            let browsing = self.browsing.lock().unwrap();
            browsing
                .queue_screen
                .as_ref()
                .map(|screen| {
                    screen
                        .buttons
                        .iter()
                        .enumerate()
                        .filter_map(|(at, button)| {
                            let action = button.action.as_ref()?;
                            let label = button.text.clone()?;
                            let uri = action.uri.as_deref().unwrap_or_default();

                            let mode = match action.kind {
                                // A route into the client, not a request: the
                                // official controller intercepts it before its
                                // own router and turns it into a local flag.
                                ActionKind::DeepLink if uri.starts_with("/edit-queue") => 1,
                                // The player asking to be asked.
                                ActionKind::Confirmation => 2,
                                // Older firmware sends Clear without the ask.
                                // Emptying a queue cannot be undone, so it is
                                // asked for anyway.
                                ActionKind::PlayerLink if uri.starts_with("/Clear") => 2,
                                ActionKind::Browse
                                    if action.result_type.as_deref()
                                        == Some("SaveQueueOptions") =>
                                {
                                    3
                                }
                                _ => 0,
                            };

                            Some(QueueButtonData {
                                index: at as i32,
                                glyph: glyphs::glyph_for(&label, None),
                                highlight: button.highlight,
                                question: action
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| format!("{label}?")),
                                label,
                                mode,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        let (rows, line, cursor_row) = {
            let guard = self.registry.lock().unwrap();
            match selected.and_then(|id| guard.get(&id)).and_then(|entry| {
                entry
                    .queue
                    .as_ref()
                    .map(|queue| (queue, entry.status.clone()))
            }) {
                Some((queue, status)) => {
                    let status = status.unwrap_or_default();
                    let cursor = queue.cursor(&status);
                    // The cursor is where playback would resume; it is only a
                    // now-playing marker if the queue is what is playing.
                    let live = queue.is_playing_from(&status) && status.is_playing();

                    let client = selected
                        .and_then(|id| guard.get(&id))
                        .map(|e| e.client.clone());

                    let rows: Vec<TrackData> = queue
                        .songs
                        .iter()
                        .map(|song| {
                            let at_cursor = Some(song.id) == cursor;
                            // Only what is already decoded. Anything missing
                            // is being fetched, and lands on a later republish.
                            let cover = song
                                .image
                                .as_deref()
                                .filter(|src| !src.is_empty())
                                .zip(client.as_ref())
                                .and_then(|(src, client)| {
                                    self.artwork.cached(&client.image_url(src), THUMB_SIZE)
                                });

                            TrackData {
                                id: song.id as i32,
                                title: song.title.clone().unwrap_or_default(),
                                artist: song.artist.clone().unwrap_or_default(),
                                duration: song.duration().unwrap_or_default(),
                                quality: quality_label(song.quality.as_deref().unwrap_or_default()),
                                cursor: at_cursor,
                                live: at_cursor && live,
                                cover,
                            }
                        })
                        .collect();

                    // Which row the player is on, which is not the same number
                    // as the queue position once a window starts anywhere but
                    // the beginning.
                    let cursor_row = cursor
                        .and_then(|at| queue.songs.iter().position(|song| song.id == at))
                        .map(|row| row as i32)
                        .unwrap_or(-1);

                    let shown = rows.len() as u32;
                    let plural = if queue.length == 1 { "track" } else { "tracks" };
                    let line = if shown == queue.length {
                        format!("Queue · {shown} {plural}")
                    } else {
                        format!("Queue · {shown} of {} {plural}", queue.length)
                    };
                    (rows, line, cursor_row)
                }
                None => (Vec::new(), "Queue".to_owned(), -1),
            }
        };

        if already_sent(&self.sent_queue, {
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            line.hash(&mut hash);
            cursor_row.hash(&mut hash);
            for row in &rows {
                row.id.hash(&mut hash);
                row.title.hash(&mut hash);
                row.artist.hash(&mut hash);
                row.duration.hash(&mut hash);
                row.quality.hash(&mut hash);
                row.cursor.hash(&mut hash);
                row.live.hash(&mut hash);
                row.cover.is_some().hash(&mut hash);
            }
            for button in &buttons {
                button.index.hash(&mut hash);
                button.label.hash(&mut hash);
                button.highlight.hash(&mut hash);
                button.mode.hash(&mut hash);
            }
            hash.finish()
        }) {
            return;
        }

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            let icons = Icons::get(&ui);

            let rows: Vec<Track> = rows
                .into_iter()
                .map(|row| Track {
                    id: row.id,
                    title: row.title.into(),
                    artist: row.artist.into(),
                    duration: row.duration.into(),
                    quality: row.quality.into(),
                    cursor: row.cursor,
                    live: row.live,
                    cover: row.cover.map(slint::Image::from_rgba8).unwrap_or_default(),
                })
                .collect();

            ui.set_queue(ModelRc::new(VecModel::from(rows)));
            ui.set_queue_line(line.into());
            ui.set_queue_cursor(cursor_row);
            // Split here rather than in the window. Drawing every button in
            // both rows and giving the wrong ones no width still left the
            // spacing around them, which is what pushed the last button off
            // the side of the pane.
            let dressed = |button: QueueButtonData| QueueButton {
                index: button.index,
                label: button.label.into(),
                glyph: match button.glyph {
                    Some(glyph) => glyph_image(&icons, glyph),
                    None => Default::default(),
                },
                highlight: button.highlight,
                mode: button.mode,
                question: button.question.into(),
            };
            let (across, below): (Vec<_>, Vec<_>) = buttons
                .into_iter()
                .partition(|button| button.label.chars().count() <= QUEUE_BUTTON_SHORT);

            ui.set_queue_buttons(ModelRc::new(VecModel::from(
                across.into_iter().map(dressed).collect::<Vec<_>>(),
            )));
            ui.set_queue_buttons_below(ModelRc::new(VecModel::from(
                below.into_iter().map(dressed).collect::<Vec<_>>(),
            )));
        });
    }

    /// Push the navigation sidebar to the window.
    ///
    /// Everything in it comes from the player: the screens it reports having,
    /// then the inputs and services off its own Sources screen, under that
    /// screen's own headings.
    fn publish_sidebar(&self) {
        let rows = {
            let browsing = self.browsing.lock().unwrap();
            let lit = browsing.highlighted;
            let mut rows: Vec<BrowseData> = Vec::new();

            for (index, (label, _)) in browsing.screens.iter().enumerate() {
                rows.push(BrowseData {
                    track: String::new(),
                    quality: String::new(),
                    index: index as i32,
                    title: label.clone(),
                    subtitle: String::new(),
                    action: String::new(),
                    cover: None,
                    glyph: glyphs::glyph_for(label, None),
                    heading: false,
                    actionable: true,
                    playing: lit == Some((0, index as i32)),
                    selected: false,
                    has_menu: false,
                });
            }

            if let Some(sources) = &browsing.sources {
                let mut index = 0i32;
                for (ordinal, section) in sources.sections.iter().enumerate() {
                    if let Some(title) = &section.title {
                        // The button beside the heading is the section's own
                        // menu action: "Customise" opens the inputs settings
                        // page, "Manage" opens the music-services page. The
                        // player supplies both the wording and the target.
                        rows.push(BrowseData {
                            track: String::new(),
                            quality: String::new(),
                            index: ordinal as i32,
                            title: title.clone(),
                            subtitle: String::new(),
                            action: section
                                .menu_actions
                                .first()
                                .and_then(|menu| menu.text.clone())
                                .unwrap_or_default(),
                            cover: None,
                            glyph: None,
                            heading: true,
                            actionable: false,
                            playing: false,
                            selected: false,
                            has_menu: false,
                        });
                    }
                    for item in &section.items {
                        let label = item.label().unwrap_or_default().to_owned();
                        rows.push(BrowseData {
                            track: String::new(),
                            quality: String::new(),
                            index,
                            title: label.clone(),
                            subtitle: String::new(),
                            action: String::new(),
                            cover: None,
                            // A service in the sidebar always gets a glyph;
                            // an input keeps the usual rule, which already
                            // replaces the player's own chrome.
                            glyph: Some(match item.kind {
                                ItemKind::Service => glyphs::service_glyph(&label),
                                _ => glyphs::glyph_for(
                                    &label,
                                    item.icon.as_deref().or(item.image.as_deref()),
                                )
                                .unwrap_or(Glyph::Service),
                            }),
                            heading: false,
                            actionable: item.is_actionable(),
                            playing: lit == Some((1, index)),
                            selected: false,
                            has_menu: false,
                        });
                        index += 1;
                    }
                }
            }
            rows
        };

        // The kind has to survive into the model, and BrowseData has no room
        // for it, so it is recovered from whether a heading precedes: simpler
        // to carry it explicitly.
        let screens = self.browsing.lock().unwrap().screens.len();

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            let icons = Icons::get(&ui);

            let mut seen = 0usize;
            let rows: Vec<SidebarRow> = rows
                .into_iter()
                .map(|row| {
                    let kind = if row.heading {
                        2
                    } else if seen < screens {
                        seen += 1;
                        0
                    } else {
                        1
                    };
                    SidebarRow {
                        label: row.title.into(),
                        action: row.action.into(),
                        cover: match row.glyph {
                            Some(glyph) => glyph_image(&icons, glyph),
                            None => Default::default(),
                        },
                        is_glyph: row.glyph.is_some(),
                        header: row.heading,
                        selected: row.playing,
                        kind,
                        index: row.index,
                    }
                })
                .collect();

            ui.set_sidebar(ModelRc::new(VecModel::from(rows)));
        });
    }

    /// Push the current browse screen to the window.
    ///
    /// Sections become headings in a single flat list. The alternative — real
    /// nested rows and shelves the way the official app draws them — is a
    /// bigger UI job than the parsing was, and a list is legible in a 340px
    /// column where a horizontal shelf is not.
    /// Put the Help menu, or a page reached from it, in the middle pane.
    fn publish_help(&self) {
        if let Pane::HelpDetail(title, facts) = &self.browsing.lock().unwrap().pane {
            let (title, facts) = (title.clone(), facts.clone());
            let rows = facts
                .into_iter()
                .map(|(label, value)| SettingData {
                    index: -1,
                    label,
                    detail: value,
                    glyph: Some(Glyph::Info),
                    control: "none",
                    available: true,
                    ..SettingData::blank()
                })
                .collect();
            return self.send_settings(rows, title);
        }

        // About is filled in from what the selected player has told us, so it
        // says something true rather than a version number on its own.
        let about = {
            let selected = *self.selected.lock().unwrap();
            let guard = self.registry.lock().unwrap();
            match selected
                .and_then(|id| guard.get(&id))
                .and_then(|e| e.sync.as_ref())
            {
                Some(sync) => format!(
                    "Azzurro {} · {} on BluOS {}",
                    env!("CARGO_PKG_VERSION"),
                    sync.display_model(),
                    sync.version.as_deref().unwrap_or("?")
                ),
                None => format!("Azzurro {}", env!("CARGO_PKG_VERSION")),
            }
        };

        let mut rows: Vec<SettingData> = HELP_ENTRIES
            .iter()
            .enumerate()
            .map(|(index, (label, _kind, detail, glyph))| SettingData {
                index: index as i32,
                label: (*label).to_owned(),
                detail: (*detail).to_owned(),
                glyph: Some(*glyph),
                control: "link",
                available: true,
                ..SettingData::blank()
            })
            .collect();

        rows.push(SettingData {
            index: HELP_ENTRIES.len() as i32,
            label: "About".to_owned(),
            detail: about,
            glyph: Some(Glyph::Info),
            control: "none",
            available: true,
            ..SettingData::blank()
        });

        // Two lines rather than one: who holds the copyright and what the
        // licence is are the app's own, and the trademark note is about
        // somebody else's — running them together reads as one claim.
        rows.push(SettingData {
            index: -1,
            label: "© 2026 Jonathan Zeppettini · MIT licence".to_owned(),
            detail: "Not affiliated with, endorsed by, or supported by Lenbrook \
                     Industries. BluOS, Bluesound and NAD are their trademarks."
                .to_owned(),
            glyph: Some(Glyph::Details),
            control: "none",
            available: true,
            ..SettingData::blank()
        });

        self.send_settings(rows, "Help".to_owned());
    }

    /// Put a form from the player's web UI in the middle pane.
    ///
    /// Drawn with the settings pane's rows, which already have a control for
    /// every shape these forms use: a line of text, a switch, a list to choose
    /// from. Only the masked one is new, and only because nothing in the
    /// player's own settings ever asks for a password.
    fn publish_form(&self) {
        let Some(page) = self.browsing.lock().unwrap().pane.form().map(|page| {
            (
                page.title.clone(),
                page.form.clone(),
                page.values.clone(),
                page.note.clone(),
            )
        }) else {
            return;
        };
        let (title, form, values, note) = page;

        let mut rows: Vec<SettingData> = Vec::new();
        if !note.is_empty() {
            rows.push(SettingData {
                index: -1,
                glyph: Some(Glyph::Info),
                label: note,
                control: "none",
                available: true,
                ..SettingData::blank()
            });
        }

        for field in &form.fields {
            let held = values.get(&field.name).cloned().unwrap_or_default();
            rows.push(SettingData {
                index: -1,
                glyph: Some(match field.kind {
                    bluos::forms::Kind::Password => Glyph::Secret,
                    bluos::forms::Kind::Choice => Glyph::Tweak,
                    _ => Glyph::Details,
                }),
                label: if field.label.is_empty() {
                    field.name.clone()
                } else {
                    field.label.clone()
                },
                control: match field.kind {
                    bluos::forms::Kind::Text => "text",
                    bluos::forms::Kind::Password => "password",
                    bluos::forms::Kind::Choice => "list",
                    bluos::forms::Kind::Switch => "boolean",
                },
                on: !held.is_empty(),
                // A password is never drawn back, not even as its own length.
                value: match field.kind {
                    bluos::forms::Kind::Password => String::new(),
                    _ => held.clone(),
                },
                options: field.choices.iter().map(|c| c.label.clone()).collect(),
                option_index: field
                    .choices
                    .iter()
                    .position(|c| c.value == held)
                    .unwrap_or(0) as i32,
                available: true,
                ..SettingData::blank()
            });
        }

        for submit in &form.submits {
            rows.push(SettingData {
                index: -1,
                glyph: None,
                // The button says what it does; a row saying the same thing
                // beside it is the word twice.
                label: String::new(),
                value: submit.label.clone(),
                control: "button",
                available: true,
                ..SettingData::blank()
            });
        }

        for (at, row) in rows.iter_mut().enumerate() {
            row.index = at as i32;
        }
        self.send_settings(rows, title);
    }

    /// Put one of the player's web configuration pages in the middle pane.
    ///
    /// Drawn with the settings pane's own rows, because that is what these are:
    /// a label, something to say about it, and one thing to do.
    fn publish_web(&self) {
        let page = self
            .browsing
            .lock()
            .unwrap()
            .pane
            .web()
            .map(|page| match page {
                WebPage::Services(services) => (
                    "Music Services".to_owned(),
                    services
                        .iter()
                        .map(|service| SettingData {
                            index: -1,
                            glyph: Some(glyphs::service_glyph(&service.name)),
                            label: service.name.clone(),
                            // Signing in is a form with a password on it. Naming
                            // where it goes is the honest way to say that pressing
                            // this leaves the app.
                            detail: "Sign in on the player".to_owned(),
                            control: "link",
                            available: true,
                            ..SettingData::blank()
                        })
                        .collect::<Vec<_>>(),
                ),
                WebPage::Shares { shares, .. } => (
                    "Network shares".to_owned(),
                    shares
                        .iter()
                        .map(|share| SettingData {
                            index: -1,
                            glyph: Some(Glyph::Network),
                            label: share.label.clone(),
                            detail: String::new(),
                            control: "button",
                            // The pill says what pressing it does; the row says
                            // which share it would do it to.
                            value: "Remove".to_owned(),
                            available: true,
                            ..SettingData::blank()
                        })
                        .chain(std::iter::once(SettingData {
                            index: -1,
                            glyph: Some(Glyph::Add),
                            label: "Add a share".to_owned(),
                            detail: String::new(),
                            control: "link",
                            available: true,
                            ..SettingData::blank()
                        }))
                        .collect::<Vec<_>>(),
                ),
            });

        let Some((title, mut rows)) = page else {
            return;
        };
        // Numbered in the order they are drawn, so a press finds its way back
        // to the row it came from.
        for (at, row) in rows.iter_mut().enumerate() {
            row.index = at as i32;
        }
        self.send_settings(rows, title);
    }

    /// Draw whichever pane is showing.
    ///
    /// One entry point rather than four, because a caller that has just changed
    /// panes does not know which one it landed on — after Back it depends on
    /// where Back went.
    fn publish_pane(&self) {
        enum Showing {
            Browse,
            Settings,
            Help,
            Web,
            Form,
            NowPlaying,
            Customise,
        }
        let showing = match self.browsing.lock().unwrap().pane {
            Pane::Browse => Showing::Browse,
            Pane::Settings(_) => Showing::Settings,
            Pane::Help | Pane::HelpDetail(..) => Showing::Help,
            Pane::Web(_) => Showing::Web,
            Pane::Form(_) => Showing::Form,
            Pane::NowPlaying => Showing::NowPlaying,
            Pane::Customise(_) => Showing::Customise,
        };

        let large = matches!(showing, Showing::NowPlaying);
        let customising = matches!(showing, Showing::Customise);
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_now_playing(large);
                ui.set_customising(customising);
            }
        });

        match showing {
            // Settings first: it is what takes the pane back off the browse
            // screen when nothing else is showing.
            Showing::Browse => {
                self.publish_settings();
                self.publish_browse();
            }
            Showing::Settings => self.publish_settings(),
            Showing::Help => self.publish_help(),
            Showing::Web => self.publish_web(),
            Showing::Form => self.publish_form(),
            // Nothing to send: it is drawn from what the transport already
            // publishes. Only the settings rows have to be taken down.
            Showing::Customise => self.publish_customise(),
            Showing::NowPlaying => self.publish_settings(),
        }
    }

    /// Turn a settings page into rows for the middle pane.
    fn publish_settings(&self) {
        let Some(page) = self.browsing.lock().unwrap().pane.settings().cloned() else {
            let ui = self.ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade() {
                    ui.set_in_settings(false);
                }
            });
            return;
        };

        let sleep = self
            .selected
            .lock()
            .unwrap()
            .and_then(|id| self.with_entry(id, |e| e.status.clone()))
            .flatten()
            .and_then(|status| status.sleep_minutes());

        let mut rows: Vec<SettingData> = Vec::new();
        let mut index = 0i32;
        walk_settings(&page.entries, &page, sleep, &mut rows, &mut index);

        // A page fetched by id has one group on it and that group names
        // itself — "Customize sources", "Music library". Its own name beats
        // the id it was fetched by, which is a word like `capture` that means
        // nothing to anyone outside the firmware.
        let named = match page.entries.as_slice() {
            [SettingEntry::Group(group)] => group.display_name.clone(),
            _ => None,
        };
        let title = match (named, &page.page_id) {
            (Some(name), _) => name,
            (None, Some(id)) => format!("Settings · {id}"),
            (None, None) => "Settings".to_owned(),
        };

        self.send_settings(rows, title);
    }

    /// Hand a list of setting-shaped rows to the middle pane.
    fn send_settings(&self, rows: Vec<SettingData>, title: String) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            let icons = Icons::get(&ui);

            let items: Vec<SettingItem> = rows
                .into_iter()
                .map(|row| SettingItem {
                    index: row.index,
                    label: row.label.into(),
                    detail: row.detail.into(),
                    cover: match row.glyph {
                        Some(glyph) => glyph_image(&icons, glyph),
                        None => Default::default(),
                    },
                    is_glyph: row.glyph.is_some(),
                    heading: row.heading,
                    control: row.control.into(),
                    on: row.on,
                    value: row.value.into(),
                    number: row.number,
                    minimum: row.minimum,
                    maximum: row.maximum,
                    step: row.step,
                    units: row.units.into(),
                    options: ModelRc::new(VecModel::from(
                        row.options
                            .into_iter()
                            .map(slint::SharedString::from)
                            .collect::<Vec<_>>(),
                    )),
                    option_index: row.option_index,
                    available: row.available,
                })
                .collect();

            ui.set_settings(ModelRc::new(VecModel::from(items)));
            ui.set_in_settings(true);
            ui.set_browse_title(title.into());
            ui.set_browse_can_go_back(true);
        });
    }

    /// The order to draw a screen's sections in.
    ///
    /// The identity permutation unless Customise Home has been used on this
    /// screen, so a screen nobody has rearranged costs one map lookup.
    fn arrangement(&self, screen: &Screen) -> Vec<usize> {
        let plain = || (0..screen.sections.len()).collect::<Vec<_>>();
        let Some(id) = screen.id.as_deref() else {
            return plain();
        };
        let orders = self.orders.lock().unwrap();
        let Some(wanted) = orders.get(id) else {
            return plain();
        };

        let ids: Vec<Option<String>> = screen.sections.iter().map(|s| s.id.clone()).collect();
        let pinned: Vec<bool> = screen.sections.iter().map(|s| s.no_reorder).collect();
        order::arrange(&ids, &pinned, wanted)
    }

    /// Send the list of sections being rearranged to the window.
    fn publish_customise(&self) {
        let (title, rows) = {
            let browsing = self.browsing.lock().unwrap();
            let Pane::Customise(page) = &browsing.pane else {
                return;
            };
            (
                page.title.clone(),
                page.rows
                    .iter()
                    .map(|(_, title)| slint::SharedString::from(title.as_str()))
                    .collect::<Vec<_>>(),
            )
        };

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            ui.set_customise_title(title.into());
            ui.set_customise_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
        });
    }

    fn publish_browse(&self) {
        // Which player's screens these are, then everything about that player,
        // then the screens themselves — three steps rather than two, so that no
        // two of these locks are ever held at once. See the note on `Backend`.
        let device = self.browsing.lock().unwrap().device;
        let status = device
            .and_then(|id| self.with_entry(id, |e| e.status.clone()))
            .flatten();
        let client = device.and_then(|id| self.with_entry(id, |e| e.client.clone()));

        // Settings and Help have the pane while either is open; publishing
        // here would fight them for the title.
        {
            let browsing = self.browsing.lock().unwrap();
            if !matches!(browsing.pane, Pane::Browse) {
                return;
            }
        }

        let (blocks, selector, recent, empty, header, title, can_go_back, search) = {
            let browsing = self.browsing.lock().unwrap();

            let Some(screen) = browsing.current() else {
                return self.send_browse(
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    "Browse".into(),
                    false,
                    None,
                );
            };

            // The block at the top of an album's page. Its artwork is the one
            // picture on the screen worth showing large, so it is asked for at
            // the size a cover is drawn rather than a row's.
            let header = screen.header.as_ref().map(|header| HeaderData {
                cover: header
                    .image
                    .as_deref()
                    .filter(|src| !src.is_empty())
                    .zip(client.as_ref())
                    .and_then(|(src, client)| {
                        self.artwork.cached(&client.image_url(src), COVER_SIZE)
                    }),
                title: header.title.clone().unwrap_or_default(),
                subtitle: header.subtitle.clone().unwrap_or_default(),
                detail: header.subsubtitle.clone().unwrap_or_default(),
                buttons: header
                    .buttons
                    .iter()
                    .enumerate()
                    .filter_map(|(at, button)| {
                        let label = button.text.clone()?;
                        let glyph = glyphs::glyph_for(&label, None);
                        Some((at as i32, label, glyph))
                    })
                    .collect(),
            });

            let mut blocks: Vec<BlockData> = Vec::new();
            let mut selector: Vec<BrowseData> = Vec::new();
            let mut empty: Option<(String, String, Option<Glyph>)> = None;
            let mut index = 0i32;
            // Only worth a heading when there is more than one to tell apart.
            let listed = screen
                .sections
                .iter()
                .filter(|s| s.kind == SectionKind::List && s.title.is_some())
                .count();

            // Whatever order Customise Home last put these in. `ordinal` stays
            // the section's real index throughout, because every action
            // published from this loop refers back into the screen the player
            // sent — reordering the drawing must not reorder the addressing.
            for ordinal in self.arrangement(screen) {
                let section = &screen.sections[ordinal];
                let kind = match section.kind {
                    SectionKind::Row => 1,
                    SectionKind::SelectorMenu => 2,
                    SectionKind::List => 0,
                };
                // A shelf always names itself — that is the only way to tell
                // "Presets" from "Recently Played" when both are a line of
                // covers. Chips need no heading; they are self-evident.
                let heading = match (kind, &section.title) {
                    (1, Some(title)) => title.clone(),
                    (0, Some(title)) if listed > 1 => title.clone(),
                    _ => String::new(),
                };
                let size = if kind == 1 { TILE_SIZE } else { THUMB_SIZE };

                let mut rows = Vec::new();
                for item in &section.items {
                    // The index counts every item the screen holds, including
                    // the two below that never become rows: it is what an
                    // activation is looked up by, and `Screen::items` counts
                    // them too.
                    let at = index;
                    index += 1;

                    match item.kind {
                        // It has its own field above the list.
                        ItemKind::Search => continue,
                        // Not something to put on the screen — something to
                        // draw instead of the screen.
                        ItemKind::InfoPanel => {
                            empty = Some((
                                item.label().unwrap_or_default().to_owned(),
                                item.extra.get("subText").cloned().unwrap_or_default(),
                                glyphs::glyph_for(screen.heading().unwrap_or_default(), None),
                            ));
                            continue;
                        }
                        _ => {}
                    }

                    let source = item
                        .image
                        .as_deref()
                        .or(item.icon.as_deref())
                        .filter(|src| !src.is_empty());

                    // A service picker is a row of names in the app's own
                    // chrome, so it gets the app's own icons; everywhere else
                    // the player's picture wins when it is content.
                    let glyph = if kind == 2 {
                        Some(glyphs::service_glyph(item.label().unwrap_or_default()))
                    } else if item
                        .action
                        .as_ref()
                        .is_some_and(bluos::Action::is_browse_menu)
                    {
                        Some(glyphs::menu_glyph(item.label().unwrap_or_default()))
                    } else {
                        glyphs::glyph_for(item.label().unwrap_or_default(), source)
                    };
                    // A glyph makes the picture beside it redundant, and not
                    // fetching it saves a request the player would have served.
                    let cover = glyph
                        .is_none()
                        .then_some(source)
                        .flatten()
                        .zip(client.as_ref())
                        .and_then(|(src, client)| {
                            self.artwork.cached(&client.image_url(src), size)
                        });

                    rows.push(BrowseData {
                        track: item.extra.get("track").cloned().unwrap_or_default(),
                        quality: quality_label(item.quality.as_deref().unwrap_or_default()),
                        index: at,
                        action: String::new(),
                        // Some rows are an icon and nothing else — the "add a
                        // preset" tile is one — so fall back to what the
                        // action calls itself, and then to nothing rather than
                        // to a placeholder dash.
                        title: item
                            .label()
                            .or_else(|| item.action.as_ref().and_then(|a| a.title.as_deref()))
                            .unwrap_or_default()
                            .to_owned(),
                        subtitle: item
                            .subtitle
                            .clone()
                            .or_else(|| item.body.clone())
                            .unwrap_or_default(),
                        cover,
                        glyph,
                        heading: false,
                        actionable: item.is_actionable(),
                        playing: status.as_ref().is_some_and(|s| item.is_playing(s)),
                        selected: item.selected,
                        has_menu: item.context_menu.is_some(),
                    });
                }

                // A section with a title, an action and nothing inside it is a
                // heading that is also a way in: a library's front page is nine
                // of them — Artists, Albums, Songs — and dropping them for
                // being empty left only the one shelf that had content.
                if rows.is_empty() {
                    if let (Some(title), true) = (&section.title, section.action.is_some()) {
                        blocks.push(BlockData {
                            kind: 3,
                            title: title.clone(),
                            action: String::new(),
                            section: ordinal as i32,
                            rows: Vec::new(),
                        });
                    }
                    continue;
                }
                // The service picker belongs beside the title, where the
                // official controller keeps it, rather than as a row of chips
                // above content it is not part of.
                if kind == 2 {
                    selector = rows;
                } else {
                    blocks.push(BlockData {
                        kind,
                        title: heading,
                        // "View All" beside a shelf showing ten of a hundred,
                        // "Clear" beside the recents. The player writes both
                        // the wording and what they do.
                        action: section
                            .menu_actions
                            .first()
                            .and_then(|menu| menu.text.clone())
                            .unwrap_or_default(),
                        section: ordinal as i32,
                        rows,
                    });
                }
            }

            let title = match (screen.heading(), screen.subtitle.as_deref()) {
                (Some(heading), Some(sub)) => format!("{heading} — {sub}"),
                (Some(heading), None) => heading.to_owned(),
                _ => "Browse".to_owned(),
            };
            let search = screen
                .items()
                .find(|i| i.search_parameter().is_some())
                .map(|i| i.prompt().or(i.label()).unwrap_or("Search").to_owned());

            // The panel is what to draw when there is nothing else; a screen
            // that has both is showing content, so the content wins.
            let empty = empty.filter(|_| blocks.is_empty());
            let recent = browsing.recent.clone();

            (
                blocks,
                selector,
                recent,
                empty,
                header,
                title,
                browsing.trail.len() > 1,
                search,
            )
        };

        self.send_browse(
            blocks,
            selector,
            recent,
            empty,
            header,
            title,
            can_go_back,
            search,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn send_browse(
        &self,
        blocks: Vec<BlockData>,
        selector: Vec<BrowseData>,
        recent: Vec<String>,
        empty: Option<(String, String, Option<Glyph>)>,
        header: Option<HeaderData>,
        title: String,
        can_go_back: bool,
        search: Option<String>,
    ) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            let icons = Icons::get(&ui);

            let blocks: Vec<BrowseBlock> = blocks
                .into_iter()
                .map(|block| BrowseBlock {
                    kind: block.kind,
                    title: block.title.into(),
                    action: block.action.into(),
                    section: block.section,
                    rows: ModelRc::new(VecModel::from(
                        block
                            .rows
                            .into_iter()
                            .map(|row| BrowseRow {
                                index: row.index,
                                title: row.title.into(),
                                subtitle: row.subtitle.into(),
                                track: row.track.into(),
                                quality: row.quality.into(),
                                // The glyphs live in the .slint file, so they
                                // can only be reached from inside the event
                                // loop — which is also the only place a
                                // slint::Image may be built.
                                cover: match row.glyph {
                                    Some(glyph) => glyph_image(&icons, glyph),
                                    None => {
                                        row.cover.map(slint::Image::from_rgba8).unwrap_or_default()
                                    }
                                },
                                is_glyph: row.glyph.is_some(),
                                heading: row.heading,
                                actionable: row.actionable,
                                playing: row.playing,
                                selected: row.selected,
                                has_menu: row.has_menu,
                            })
                            .collect::<Vec<_>>(),
                    )),
                })
                .collect();

            // Which service the picker is showing, said once here rather
            // than searched for in the .slint file every time it draws.
            let chosen = selector.iter().find(|row| row.selected);
            ui.set_browse_service(
                chosen
                    .map(|row| row.title.clone())
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_browse_service_icon(match chosen.and_then(|row| row.glyph) {
                Some(glyph) => glyph_image(&icons, glyph),
                None => Default::default(),
            });
            ui.set_browse_selector(ModelRc::new(VecModel::from(
                selector
                    .into_iter()
                    .map(|row| BrowseRow {
                        index: row.index,
                        title: row.title.into(),
                        subtitle: Default::default(),
                        track: Default::default(),
                        quality: Default::default(),
                        cover: match row.glyph {
                            Some(glyph) => glyph_image(&icons, glyph),
                            None => Default::default(),
                        },
                        is_glyph: row.glyph.is_some(),
                        heading: false,
                        actionable: true,
                        playing: false,
                        selected: row.selected,
                        has_menu: false,
                    })
                    .collect::<Vec<_>>(),
            )));
            ui.set_browse_recent(ModelRc::new(VecModel::from(
                recent
                    .into_iter()
                    .map(slint::SharedString::from)
                    .collect::<Vec<_>>(),
            )));
            ui.set_browse_header_title(
                header
                    .as_ref()
                    .map(|h| h.title.clone())
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_browse_header_subtitle(
                header
                    .as_ref()
                    .map(|h| h.subtitle.clone())
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_browse_header_detail(
                header
                    .as_ref()
                    .map(|h| h.detail.clone())
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_browse_header_cover(
                header
                    .as_ref()
                    .and_then(|h| h.cover.clone())
                    .map(slint::Image::from_rgba8)
                    .unwrap_or_default(),
            );
            ui.set_browse_header_buttons(ModelRc::new(VecModel::from(
                header
                    .map(|h| h.buttons)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(index, label, glyph)| ActionButton {
                        index,
                        label: label.into(),
                        glyph: match glyph {
                            Some(glyph) => glyph_image(&icons, glyph),
                            None => Default::default(),
                        },
                        // The player puts the one it means first and colours it
                        // its own accent. The colour is dropped, the order is
                        // not.
                        primary: index == 0,
                    })
                    .collect::<Vec<_>>(),
            )));
            ui.set_browse_blocks(ModelRc::new(VecModel::from(blocks)));
            ui.set_browse_empty(
                empty
                    .as_ref()
                    .map(|(t, _, _)| t.clone())
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_browse_empty_detail(
                empty
                    .as_ref()
                    .map(|(_, d, _)| d.clone())
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_browse_empty_icon(match empty.and_then(|(_, _, glyph)| glyph) {
                Some(glyph) => glyph_image(&icons, glyph),
                None => icons.get_info(),
            });
            ui.set_browse_title(title.into());
            ui.set_browse_can_go_back(can_go_back);
            ui.set_browse_has_search(search.is_some());
            if let Some(prompt) = search {
                ui.set_browse_search_prompt(prompt.into());
            }
        });
    }

    /// Push the selected player's transport state to the window.
    ///
    /// Kept out of the device model on purpose: the position advances once a
    /// second, and rebuilding every row for it would re-upload every cover on
    /// screen along the way.
    fn publish_transport(&self) {
        let selected = *self.selected.lock().unwrap();
        let snapshot = selected
            .and_then(|id| self.with_entry(id, |e| (e.status.clone(), e.status_at)))
            .and_then(|(status, at)| Some((status?, at)));

        // Reindexing a library takes minutes and the player counts as it goes.
        // It never says how many there are in total, so there is no percentage
        // to report — the count and a bar that says "still working" is the
        // whole of what the player knows.
        // "cd", "hd", "mqa" — the player's own word for what it is decoding,
        // shown the way the official controller shows it.
        let quality = quality_label(
            snapshot
                .as_ref()
                .and_then(|(status, _)| status.quality.as_deref())
                .unwrap_or_default(),
        );

        let indexing = match snapshot.as_ref().and_then(|(status, _)| status.indexing) {
            Some(songs) if songs > 0 => {
                format!("Indexing the music library — {songs} songs so far")
            }
            _ => String::new(),
        };

        let (position, duration, seekable, shuffle, repeat) = match &snapshot {
            Some((status, at)) => {
                let reported = status.secs.unwrap_or(0) as i64;
                let total = status.totlen.unwrap_or(0.0).max(0.0) as i64;
                // The player only says where it is when it sends a status, so
                // between polls the clock has to be carried forward here or the
                // bar would sit still and then jump.
                let elapsed = match (status.is_playing(), at) {
                    (true, Some(at)) => at.elapsed().as_secs() as i64,
                    _ => 0,
                };
                let position = if total > 0 {
                    (reported + elapsed).clamp(0, total)
                } else {
                    reported + elapsed
                };
                (
                    position,
                    total,
                    status.seekable(),
                    status.shuffle_on(),
                    status.repeat.unwrap_or(2) as i32,
                )
            }
            None => (0, 0, false, false, 2),
        };

        // A paused player's position does not move, and neither does anything
        // else here, yet this ran once a second regardless — and each run
        // repainted the progress line, which spans the whole window. Nothing to
        // say means nothing to draw.
        if already_sent(&self.sent_transport, {
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            position.hash(&mut hash);
            duration.hash(&mut hash);
            seekable.hash(&mut hash);
            shuffle.hash(&mut hash);
            repeat.hash(&mut hash);
            indexing.hash(&mut hash);
            quality.hash(&mut hash);
            hash.finish()
        }) {
            return;
        }

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            ui.set_position(position as i32);
            ui.set_duration(duration as i32);
            ui.set_position_text(bluos::clock(position).into());
            ui.set_duration_text(bluos::clock(duration).into());
            ui.set_seekable(seekable);
            ui.set_shuffle(shuffle);
            ui.set_repeat(repeat);
            ui.set_indexing(indexing.as_str().into());
            ui.set_quality(quality.as_str().into());
        });
    }

    /// Put cover art in the now-playing panel, or clear it.
    ///
    /// The tint travels with it: a colour taken from the artwork, which the
    /// panel washes behind everything at low opacity so the room the music is
    /// in picks up the colour of the record. Without artwork there is no
    /// colour, and the panel is its ordinary self.
    fn set_cover(&self, pixels: Option<Pixels>, tint: Option<[u8; 3]>) {
        let ui = self.ui.clone();
        // Blurred here, off the UI thread, rather than in the event loop: it
        // is a decode-sized job and the cover changes on every track.
        let blurred = pixels.as_ref().and_then(artwork::frosted);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            ui.set_cover(pixels.map(slint::Image::from_rgba8).unwrap_or_default());
            ui.set_cover_blur(blurred.map(slint::Image::from_rgba8).unwrap_or_default());
            ui.set_has_tint(tint.is_some());
            if let Some([r, g, b]) = tint {
                ui.set_cover_tint(slint::Color::from_rgb_u8(r, g, b));
            }
        });
    }
}

/// How a settings row was changed.
#[derive(Debug, Clone)]
enum Edit {
    /// Flip a boolean, whichever pair of words it uses for its two states.
    Toggle,
    /// Pick option `n` of a list.
    Choose(usize),
    Number(f32),
    Text(String),
}

/// The Help menu.
///
/// Built here rather than fetched, because the official controller builds its
/// own too — this is the list it hardcodes, minus two entries that mean
/// nothing here: "Shortcuts", since this app has no keyboard shortcuts to
/// list, and "Upgrade Check - Controller", which updates the controller
/// itself and is the package manager's job on Linux.
///
/// Lenbrook's own support site and its log-submission page used to head this
/// list. Both are removed: neither is about this app, and the second sends a
/// player's logs to a company that did not write it and cannot support it.
/// Anyone who wants either can reach them from the player's own web UI.
///
/// About and the copyright line are added after these, in `publish_help` —
/// they are built from what the selected player has said rather than being
/// fixed strings, so they cannot live in a const.
const HELP_ENTRIES: &[(&str, HelpKind, &str, Glyph)] = &[
    (
        "Azzurro on the web",
        HelpKind::Web("https://azzurro.blue/"),
        "azzurro.blue",
        Glyph::Place,
    ),
    (
        "Upgrade Check",
        HelpKind::Upgrade,
        "Check the player for new firmware",
        Glyph::Rescan,
    ),
    (
        "Diagnostics",
        HelpKind::Diagnostics,
        "Addresses, uptime and library size",
        Glyph::Info,
    ),
];

/// What a Help entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpKind {
    /// An interactive page — signing in, submitting logs — that has to be a
    /// browser. Absolute for Lenbrook's site, relative for the player's own.
    Web(&'static str),
    /// Read-only, so it is read and shown here instead.
    Upgrade,
    Diagnostics,
}

/// One settings row on its way to the window.
struct SettingData {
    index: i32,
    label: String,
    detail: String,
    glyph: Option<Glyph>,
    heading: bool,
    control: &'static str,
    on: bool,
    value: String,
    number: f32,
    minimum: f32,
    maximum: f32,
    step: f32,
    units: String,
    options: Vec<String>,
    option_index: i32,
    available: bool,
}

/// Flatten a settings page into rows, in the same order [`pick`] counts them.
fn walk_settings(
    entries: &[SettingEntry],
    page: &SettingsPage,
    // Minutes left on the sleep timer. It is not in the settings document —
    // the player reports it in `/Status` — so the one row that needs it is
    // handed it from outside.
    sleep: Option<u32>,
    rows: &mut Vec<SettingData>,
    index: &mut i32,
) {
    for entry in entries {
        match entry {
            SettingEntry::Group(group) if group.is_page_link() => {
                let label = group
                    .display_name
                    .clone()
                    .unwrap_or_else(|| group.id.clone());
                rows.push(SettingData {
                    index: *index,
                    glyph: glyphs::glyph_for(&label, None).or(Some(Glyph::Tweak)),
                    label,
                    detail: group.description.clone().unwrap_or_default(),
                    heading: false,
                    control: "link",
                    available: true,
                    ..SettingData::blank()
                });
                *index += 1;
            }
            SettingEntry::Group(group) => {
                if let Some(title) = &group.display_name {
                    rows.push(SettingData {
                        index: -1,
                        label: title.clone(),
                        heading: true,
                        control: "none",
                        available: true,
                        ..SettingData::blank()
                    });
                }
                walk_settings(&group.entries, page, sleep, rows, index);
            }
            SettingEntry::Setting(setting) => {
                let available = page.is_available(setting);
                let bounds = setting.range.clone().unwrap_or_default();

                // A setting with a webview is one the player declines to
                // describe, so it opens rather than being drawn.
                let control = if setting.webview.is_some() {
                    "link"
                } else {
                    match setting.kind {
                        Kind::Boolean => "boolean",
                        Kind::Range => "range",
                        Kind::List => "list",
                        Kind::Text => "text",
                        Kind::Button => "button",
                        Kind::Sleep => "cycle",
                        // Not a link: the alarms editor is the official
                        // controller's own page, and there is nothing here for
                        // one to open. The row says how many there are instead
                        // of pretending to lead somewhere.
                        Kind::Alarms => "none",
                        // Including dual-range, which needs a control of its
                        // own and gets its value shown instead.
                        _ => "none",
                    }
                };

                let option_index = setting
                    .options
                    .iter()
                    .position(|o| Some(o.name.as_str()) == setting.value.as_deref())
                    .unwrap_or(0) as i32;

                rows.push(SettingData {
                    index: *index,
                    // Falls back rather than leaving a hole; see Icons.tweak.
                    glyph: glyphs::glyph_for(setting.label(), None).or(Some(Glyph::Tweak)),
                    label: setting.label().to_owned(),
                    detail: if !available {
                        setting
                            .depends_on
                            .as_ref()
                            .map(|(n, v)| format!("Needs {n} set to {v}"))
                            .unwrap_or_default()
                    } else if matches!(setting.kind, Kind::Alarms) {
                        match setting.count.unwrap_or(0) {
                            0 => "None set".to_owned(),
                            1 => "1 alarm".to_owned(),
                            n => format!("{n} alarms"),
                        }
                    } else {
                        setting.description.clone().unwrap_or_default()
                    },
                    heading: false,
                    control,
                    // The sleep row bends both of these. Its state is not in
                    // the document it is drawn from — the player reports the
                    // timer in `/Status` — so it is filled in from there.
                    on: if matches!(setting.kind, Kind::Sleep) {
                        sleep.is_some()
                    } else {
                        setting.is_on()
                    },
                    value: match setting.kind {
                        Kind::Sleep => match sleep {
                            Some(1) => "1 min".to_owned(),
                            Some(minutes) => format!("{minutes} min"),
                            None => "Off".to_owned(),
                        },
                        _ => setting.value.clone().unwrap_or_default(),
                    },
                    number: setting.number().unwrap_or(0.0),
                    minimum: bounds.min,
                    maximum: bounds.max,
                    step: bounds.step.unwrap_or(1.0),
                    units: bounds.units.unwrap_or_default(),
                    options: setting
                        .options
                        .iter()
                        .map(|o| o.label().to_owned())
                        .collect(),
                    option_index,
                    available,
                });
                *index += 1;
            }
        }
    }
}

impl SettingData {
    fn blank() -> Self {
        Self {
            index: -1,
            label: String::new(),
            detail: String::new(),
            glyph: None,
            heading: false,
            control: "none",
            on: false,
            value: String::new(),
            number: 0.0,
            minimum: 0.0,
            maximum: 1.0,
            step: 1.0,
            units: String::new(),
            options: Vec::new(),
            option_index: 0,
            available: true,
        }
    }
}

/// The setting at row `index`, for the writes.
fn setting_at(page: &SettingsPage, index: usize) -> Option<bluos::settings::Setting> {
    fn walk(
        entries: &[SettingEntry],
        at: &mut usize,
        want: usize,
    ) -> Option<bluos::settings::Setting> {
        for entry in entries {
            match entry {
                SettingEntry::Group(group) if group.is_page_link() => *at += 1,
                SettingEntry::Group(group) => {
                    if let Some(found) = walk(&group.entries, at, want) {
                        return Some(found);
                    }
                }
                SettingEntry::Setting(setting) => {
                    if *at == want {
                        return Some((**setting).clone());
                    }
                    *at += 1;
                }
            }
        }
        None
    }

    let mut at = 0;
    walk(&page.entries, &mut at, index)
}

/// What activating a settings row means.
enum Chosen {
    /// Another page, by id.
    Page(String),
    /// A page the player will not describe; hand it to a browser.
    Web(String),
    /// A value to write.
    Write(Box<bluos::settings::Setting>, String),
    /// One more step along the sleep timer's ladder. Not a write: the setting
    /// carries no value and no options, because the player owns the ladder and
    /// hands out the next rung on request.
    Sleep,
}

/// Find what row `index` of a settings page refers to.
///
/// Walks the page the same way `settings_rows` draws it, so the two agree
/// about what the numbers mean.
fn pick(page: &SettingsPage, index: usize) -> Option<Chosen> {
    fn walk(entries: &[SettingEntry], at: &mut usize, want: usize) -> Option<Chosen> {
        for entry in entries {
            match entry {
                SettingEntry::Group(group) if group.is_page_link() => {
                    if *at == want {
                        return Some(Chosen::Page(group.id.clone()));
                    }
                    *at += 1;
                }
                SettingEntry::Group(group) => {
                    if let Some(found) = walk(&group.entries, at, want) {
                        return Some(found);
                    }
                }
                SettingEntry::Setting(setting) => {
                    if *at == want {
                        if let Some(url) = &setting.webview {
                            return Some(Chosen::Web(url.clone()));
                        }
                        return match setting.kind {
                            Kind::Boolean => setting
                                .toggled()
                                .map(|value| Chosen::Write(setting.clone(), value)),
                            Kind::Sleep => Some(Chosen::Sleep),
                            // A button has no value; the player wants its name
                            // sent back at it.
                            Kind::Button => Some(Chosen::Write(
                                setting.clone(),
                                setting.name.clone().unwrap_or_default(),
                            )),
                            _ => None,
                        };
                    }
                    *at += 1;
                }
            }
        }
        None
    }

    let mut at = 0;
    walk(&page.entries, &mut at, index)
}

/// The screens worth offering, in the order the player listed them.
///
/// The two context-menu entries are addressed directly when a row is
/// right-clicked rather than browsed to, the URL resolver is internal, and the
/// queue has a pane of its own. Everything else goes in the picker — including
/// anything a future firmware adds, which is why the fallback keeps the raw
/// name rather than dropping it.
fn user_screens(config: &bluos::Configuration) -> Vec<(String, String)> {
    // What the official controller actually puts in its sidebar, which is
    // three of these and not the rest: Recently Played and Presets are rows on
    // the Home screen, Sources is the two sections drawn underneath, and News
    // is an empty screen on every player seen.
    const IN_SIDEBAR: &[&str] = &["home", "favourites", "search"];

    config
        .items
        .iter()
        .filter(|item| IN_SIDEBAR.contains(&item.id.as_str()))
        .map(|item| (screen_label(&item.id), item.uri.clone()))
        .collect()
}

fn screen_label(id: &str) -> String {
    match id {
        "home" => "Home".to_owned(),
        "recentlyPlayed" => "Recently Played".to_owned(),
        "news" => "News".to_owned(),
        "favourites" => "Favourites".to_owned(),
        "sources" => "Sources".to_owned(),
        "search" => "Search".to_owned(),
        "presets" => "Presets".to_owned(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// The settings page an action points at, if that is what it points at.
///
/// `/Settings?id=capture` names one page; a bare `/Settings` means the top of
/// the settings menu, which is `None` rather than "not a settings page" — hence
/// the two layers of `Option`.
fn settings_page(uri: &str) -> Option<Option<String>> {
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    if !path.eq_ignore_ascii_case("/Settings") {
        return None;
    }
    Some(query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "id").then(|| value.to_owned())
    }))
}

/// Broadcast for players and adopt whatever answers.
///
/// A sweep is a schedule of broadcasts spread over twelve seconds rather than
/// one query and an answer, because a single UDP broadcast is dropped often
/// enough to matter and a player that was asleep takes a moment to reply at
/// all.
async fn sweep(backend: Backend, discovery: Arc<Discovery>, http: reqwest::Client) {
    for announce in discovery.sweep(DEFAULT_SWEEP).await.unwrap_or_default() {
        backend.adopt(&announce, &http);
    }
}

/// Fetch a screen and show it.
///
/// `push` distinguishes going deeper from starting over: Back pops the trail,
/// so every screen that is arrived at by following a row has to be on it.
async fn open_screen(backend: Backend, id: DeviceId, uri: String, arrive: Arrive) {
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return;
    };

    match client.screen(&uri).await {
        Ok(screen) => {
            {
                let mut browsing = backend.browsing.lock().unwrap();
                // Showing a screen means leaving Settings and Help, which
                // otherwise keep the pane and the sidebar navigates
                // underneath them with nothing appearing to happen.
                browsing.pane = Pane::Browse;
                // Browsing follows the selection: a screen only means anything
                // against the player that served it.
                if browsing.device != Some(id) {
                    browsing.trail.clear();
                }
                browsing.device = Some(id);
                let query = match &arrive {
                    Arrive::Root => {
                        browsing.trail.clear();
                        None
                    }
                    Arrive::Deeper => None,
                    Arrive::Found(query) => {
                        // Results stand in for results. Deciding that here,
                        // with the trail locked, is what keeps two keystrokes
                        // landing at once from stacking two screens.
                        if browsing.trail.last().is_some_and(|c| c.query.is_some()) {
                            browsing.trail.pop();
                        }
                        Some(query.clone())
                    }
                    Arrive::Replace(query) => {
                        browsing.trail.pop();
                        query.clone()
                    }
                };
                browsing.trail.push(Crumb { uri, screen, query });
            }
            // The whole pane, not only the browse rows. Opening a screen is
            // also leaving whichever pane was covering them, and the window
            // keeps drawing the settings rows until it is told otherwise —
            // which showed as a screen with the right title and the wrong list
            // under it.
            backend.publish_pane();
            tokio::spawn(load_browse_thumbnails(backend, id));
        }
        Err(e) => tracing::warn!(%id, "could not read {uri}: {e}"),
    }
}

/// Do whatever row `index` of the current screen says to do.
async fn activate(backend: Backend, index: usize) {
    let (id, action, arrive, worth_keeping) = {
        let browsing = backend.browsing.lock().unwrap();
        let Some(id) = browsing.device else { return };
        let Some(crumb) = browsing.trail.last() else {
            return;
        };

        // Which section the row came from decides how its screen arrives. A
        // service picker asks to replace the screen it sits on — switching
        // from Library to TuneIn is the same screen about a different service,
        // not a step into one.
        let mut at = 0usize;
        let mut found = None;
        'sections: for section in &crumb.screen.sections {
            for item in &section.items {
                if at == index {
                    let replaces =
                        section.kind == SectionKind::SelectorMenu && section.replace_screen;
                    found = Some((item, replaces));
                    break 'sections;
                }
                at += 1;
            }
        }
        let Some((item, replaces)) = found else {
            return;
        };

        let arrive = if replaces {
            Arrive::Replace(crumb.query.clone())
        } else {
            Arrive::Deeper
        };
        // Following a result is what makes a search worth remembering: it
        // found something. Switching service is not — the same query is still
        // on screen.
        let worth_keeping = if replaces { None } else { crumb.query.clone() };

        (
            id,
            item.action.clone().or_else(|| item.play_action.clone()),
            arrive,
            worth_keeping,
        )
    };

    if let Some(query) = worth_keeping {
        remember_search(&backend, query);
    }
    let Some(action) = action else { return };
    run_action(backend, id, action, arrive).await;
}

/// Show a form, keeping whatever the page arrived filled in with.
///
/// Passwords are the exception: a page comes back with the field empty, and
/// seeding it with anything would be inventing a value nobody typed.
fn show_form(backend: &Backend, title: String, form: bluos::forms::Form, note: String) {
    let values = form
        .fields
        .iter()
        .filter(|field| field.kind != bluos::forms::Kind::Password)
        .map(|field| {
            let value = if field.value.is_empty() {
                field
                    .choices
                    .iter()
                    .find(|c| c.selected)
                    .map(|c| c.value.clone())
                    .unwrap_or_default()
            } else {
                field.value.clone()
            };
            (field.name.clone(), value)
        })
        .collect();

    {
        let mut browsing = backend.browsing.lock().unwrap();
        // Taken rather than read: the form is replacing it, and putting it in
        // the form is what lets Back give it back.
        let from = match std::mem::replace(&mut browsing.pane, Pane::Browse) {
            Pane::Web(page) => Some(page),
            _ => None,
        };
        browsing.pane = Pane::Form(Box::new(FormPage {
            title,
            form,
            values,
            note,
            from,
        }));
    }
    backend.publish_form();
}

/// Put a query at the top of the recent list.
fn remember_search(backend: &Backend, query: String) {
    {
        let mut browsing = backend.browsing.lock().unwrap();
        browsing.recent.retain(|seen| seen != &query);
        browsing.recent.insert(0, query);
        browsing.recent.truncate(RECENT_SEARCHES);
    }
    backend.publish_browse();
}

/// Search for what has been typed so far.
///
/// Called once the typing has settled rather than on the keystroke itself, and
/// then only if nothing has been typed since.
async fn run_search(backend: Backend, query: String) {
    let query = query.trim().to_owned();

    let plan = {
        let browsing = backend.browsing.lock().unwrap();
        let Some(id) = browsing.device else { return };
        let Some(crumb) = browsing.trail.last() else {
            return;
        };
        // An empty box is not a search for nothing: it is being back where the
        // searching started.
        if query.is_empty() {
            crumb.query.is_some().then_some((id, None))
        } else {
            crumb
                .screen
                .items()
                .find(|item| item.search_parameter().is_some())
                .and_then(|item| item.search_url(&query))
                .map(|uri| (id, Some(uri)))
        }
    };

    match plan {
        Some((id, Some(uri))) => {
            open_screen(backend, id, uri, Arrive::Found(query)).await;
        }
        Some((_, None)) => {
            backend.browsing.lock().unwrap().trail.pop();
            backend.publish_browse();
        }
        None => {}
    }
}

/// Carry out one action from a screen.
///
/// Every branch is the player's instruction rather than this app's decision —
/// which is the whole point of the server-driven screens, and why a music
/// service nobody has heard of still browses and plays.
async fn run_action(backend: Backend, id: DeviceId, action: bluos::Action, arrive: Arrive) {
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return;
    };
    let uri = action.uri.clone().or_else(|| action.href.clone());

    match action.kind {
        ActionKind::Browse | ActionKind::ContextBrowse => {
            let Some(uri) = uri else { return };

            // Two rows of a track's context menu are browses that do not lead
            // to a screen. The player says which by naming the result: a table
            // of five facts about a file, and a redirect out to last.fm.
            match action.result_type.as_deref() {
                Some("BriefInfo") => {
                    let title = action.title.clone().unwrap_or_else(|| "Info".to_owned());
                    match client.technical_info(&uri).await {
                        Ok(facts) if !facts.is_empty() => {
                            backend.browsing.lock().unwrap().pane = Pane::HelpDetail(title, facts);
                            backend.publish_pane();
                        }
                        other => {
                            if let Err(e) = other {
                                tracing::debug!(%id, "no technical info: {e}");
                            }
                            say(&backend.ui, "The player said nothing about this file");
                        }
                    }
                }
                // A page about the music on somebody else's site.
                Some("Info") => {
                    let url = client.image_url(&uri);
                    open_in_browser(&backend.ui, url).await;
                }
                _ => open_screen(backend, id, uri, arrive).await,
            }
        }

        // The player handed over a complete request; sending it is the whole
        // job. Its own long poll reports the result.
        ActionKind::PlayerLink | ActionKind::Add | ActionKind::Confirmation => {
            let Some(uri) = uri else { return };
            match client.follow(&uri).await {
                Ok(()) => {
                    // Even the wording of the confirmation comes from the
                    // player — "Added to favourites" is its phrase, not ours.
                    if let Some(text) = &action.notification {
                        say(&backend.ui, text.clone());
                    }
                    // Favouriting changes what the menu should say next time,
                    // and the player says so.
                    if action.refresh_screen {
                        refresh_current(backend).await;
                    }
                }
                Err(e) => tracing::warn!(%id, "{uri} failed: {e}"),
            }
        }

        // Not a path on the player: these are the official app's own routes.
        // The library is the one that matters, and it has an equivalent that
        // goes through the same browse endpoint as everything else.
        ActionKind::DeepLink => match uri.as_deref() {
            Some(route) if route.starts_with("/music-service/") => {
                let service = route.trim_start_matches("/music-service/");
                let uri =
                    format!("/ui/BrowseObjects?service={service}&type=BrowseMenu&url=%2FBrowse");
                open_screen(backend, id, uri, Arrive::Deeper).await;
            }
            // A route into the official controller's own pages, which this app
            // has not built. Saying so is the whole fix available here: the
            // alternative is a control that swallows the press, which reads as
            // the app being broken rather than unfinished.
            // Rearranging the sections of a screen. The player has no say in
            // this and no endpoint for it — `/customise-screen` is a route into
            // the controller's own interface, and the preference lives on this
            // machine. So the screen showing is read here and turned into a
            // list to drag about.
            Some("/customise-screen") => {
                let page = {
                    let browsing = backend.browsing.lock().unwrap();
                    let screen = browsing.current();
                    let arranged = screen.map(|screen| (screen, backend.arrangement(screen)));

                    arranged.and_then(|(screen, arrangement)| {
                        let rows: Vec<(String, String)> = arrangement
                            .into_iter()
                            .filter_map(|at| {
                                let section = screen.sections.get(at)?;
                                // A pinned section cannot move, so listing it
                                // would only offer something that does not
                                // work. One with no id cannot be named in the
                                // saved order and so cannot be placed either.
                                if section.no_reorder {
                                    return None;
                                }
                                let id = section.id.clone()?;
                                let title = section
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| id.replace('-', " "));
                                Some((id, title))
                            })
                            .collect();

                        Some(CustomisePage {
                            screen: screen.id.clone()?,
                            title: action
                                .title
                                .clone()
                                .unwrap_or_else(|| "Customise".to_owned()),
                            rows,
                        })
                    })
                };

                match page {
                    // Fewer than two and there is nothing to arrange.
                    Some(page) if page.rows.len() > 1 => {
                        backend.browsing.lock().unwrap().pane = Pane::Customise(page);
                        backend.publish_pane();
                    }
                    _ => say(&backend.ui, "Nothing on this screen can be moved"),
                }
            }

            Some(route) => {
                tracing::debug!(%id, "no equivalent for the route {route}");
                say(
                    &backend.ui,
                    match route {
                        "/add-preset" => "Saving presets is not built yet".to_owned(),
                        _ => format!(
                            "{} is not built yet",
                            action.title.as_deref().unwrap_or(route)
                        ),
                    },
                );
            }
            None => {}
        },

        // Signing into a music service and the player's own settings pages are
        // web pages. A controller can only point at them.
        ActionKind::Webpage | ActionKind::Setting => {
            if let Some(uri) = uri {
                // Except this one, which only looks like a web page. The
                // Customise button on the Inputs row leads to
                // `/Settings?id=capture`, and that is a settings document this
                // app already renders — sending it to a browser would be
                // handing back a page it can draw itself.
                if let Some(page) = settings_page(&uri) {
                    let _ = backend
                        .commands
                        .send(Command::OpenSettings(page, Step::Root));
                    return;
                }
                // The Manage button on Music Services. What it opens is a list
                // of the services this player can be signed into, which is
                // worth drawing; the sign-in form behind each one is not.
                if uri.contains("%2Fservices") || uri.contains("/services") {
                    let _ = backend.commands.send(Command::OpenServices);
                    return;
                }
                let url = client.image_url(&uri);
                open_in_browser(&backend.ui, url).await;
            }
        }

        ActionKind::Reorder | ActionKind::Unknown => {
            tracing::debug!(%id, "nothing to do for a {:?} action", action.kind);
        }
    }
}

/// Advance the progress bar between polls.
///
/// Unconditional once a second: setting a Slint property to the value it
/// already holds does not redraw anything, so a paused player costs two lock
/// acquisitions and nothing else.
async fn tick_position(backend: Backend) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        backend.publish_transport();
    }
}

/// Read `/SyncStatus` and fold it into the player's row.
///
/// This is where grouping becomes visible: who is leading, who is following,
/// and whether a follower has lost sight of its leader.
async fn read_sync(backend: &Backend, id: DeviceId, client: &Client) -> Option<SyncStatus> {
    let sync = client.sync_status().await.ok()?;

    if let Some(entry) = backend.registry.lock().unwrap().get_mut(&id) {
        entry.sync = Some(sync.clone());
    }

    // Remembered only now, not when it was adopted: answering /SyncStatus is
    // what makes an address a player, so a mistyped one is never written down.
    let newly_known = {
        let mut known = backend.known.lock().unwrap();
        known.insert(id).then(|| known.clone())
    };
    if let Some(known) = newly_known {
        tokio::task::spawn_blocking(move || known::save(&known));
    }

    let name = sync.name.clone();
    let model = sync.display_model().to_owned();
    backend.update(id, |view| {
        view.name = name.as_str().into();
        view.model = model.as_str().into();
    });

    // Every row's role can change when one player's grouping does — the leader
    // gains a follower at the same moment the follower gains a leader.
    backend.publish();
    Some(sync)
}

/// Re-fetch the screen on show, keeping its place on the trail.
async fn refresh_current(backend: Backend) {
    let Some((id, uri)) = ({
        let mut browsing = backend.browsing.lock().unwrap();
        browsing
            .device
            .zip(browsing.trail.pop().map(|crumb| crumb.uri))
    }) else {
        return;
    };
    open_screen(backend, id, uri, Arrive::Deeper).await;
}

/// Fetch the icons and cover art for the screen on show.
async fn load_browse_thumbnails(backend: Backend, id: DeviceId) {
    // Registry first and released, then the browse state: never both at once.
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return;
    };

    let urls: Vec<(String, u32)> = {
        let browsing = backend.browsing.lock().unwrap();
        let Some(screen) = browsing.current() else {
            return;
        };

        let mut seen = std::collections::BTreeSet::new();
        // The header's cover first: it is the largest thing on the screen and
        // the one the eye lands on, so it should not queue behind forty rows.
        let header = screen
            .header
            .as_ref()
            .and_then(|header| header.image.as_deref())
            .filter(|src| !src.is_empty())
            .map(|src| (client.image_url(src), COVER_SIZE))
            .into_iter();

        header
            .chain(
                screen
                    .sections
                    .iter()
                    // A cover on a shelf is drawn four times the size of one on a row,
                    // so the two want different fetches. Walking sections rather than
                    // items is what makes that distinction available here.
                    .flat_map(|section| {
                        let size = if section.kind == SectionKind::Row {
                            TILE_SIZE
                        } else {
                            THUMB_SIZE
                        };
                        section.items.iter().map(move |item| (item, size))
                    })
                    .filter_map(|(item, size)| {
                        // A menu row is drawn as a glyph whatever picture came with it,
                        // so fetching TuneIn's logo for "Sports" would be a request for
                        // something that never reaches the screen.
                        if item
                            .action
                            .as_ref()
                            .is_some_and(bluos::Action::is_browse_menu)
                        {
                            return None;
                        }
                        let source = item
                            .image
                            .as_deref()
                            .or(item.icon.as_deref())
                            .filter(|src| !src.is_empty())?;
                        // Drawn as a glyph, so there is nothing to fetch.
                        glyphs::glyph_for(item.label().unwrap_or_default(), Some(source))
                            .is_none()
                            .then_some((source, size))
                    })
                    .map(|(src, size)| (client.image_url(src), size)),
            )
            .filter(|entry| seen.insert(entry.clone()))
            .collect()
    };

    if urls.is_empty() {
        return;
    }

    // A few at a time. A library's Albums page is four hundred rows, and one
    // request per row means four hundred at once against a player that is also
    // being long-polled — which stalls the poll, the artwork and the window
    // together. The screen fills in slightly later and stays responsive while
    // it does.
    const AT_ONCE: usize = 6;
    let mut urls = urls.into_iter();
    let mut fetches = tokio::task::JoinSet::new();
    for (url, size) in urls.by_ref().take(AT_ONCE) {
        let artwork = backend.artwork.clone();
        fetches.spawn(async move {
            artwork.get(&url, size).await;
        });
    }

    let mut last_publish = Instant::now();
    while fetches.join_next().await.is_some() {
        if let Some((url, size)) = urls.next() {
            let artwork = backend.artwork.clone();
            fetches.spawn(async move {
                artwork.get(&url, size).await;
            });
        }
        // Redrawing rebuilds every row, so on a long list it costs more than
        // the fetch it is reporting. Often enough to look alive, rarely enough
        // not to be the reason the list stutters.
        if last_publish.elapsed() >= Duration::from_millis(400) {
            backend.publish_browse();
            last_publish = Instant::now();
        }
    }
    backend.publish_browse();
}

/// Fetch the art for whatever the selected player is playing.
///
/// Late arrivals are discarded rather than drawn: by the time a fetch returns,
/// the selection may have moved to another player, or the track may have
/// changed under it. Both are checked before anything reaches the window.
async fn load_cover(backend: Backend, id: DeviceId) {
    let wanted = backend.with_entry(id, |e| e.cover_url.clone()).flatten();

    let pixels = match &wanted {
        Some(url) => backend.artwork.get(url, COVER_SIZE).await,
        None => None,
    };

    if !backend.is_selected(id) {
        return;
    }
    if backend.with_entry(id, |e| e.cover_url.clone()).flatten() != wanted {
        return;
    }
    let tint = wanted.as_deref().and_then(|url| backend.artwork.tint(url));
    backend.set_cover(pixels, tint);
}

/// Fetch thumbnails for the queue on screen.
///
/// Deduplicated by URL first, which is what makes this cheap: a queue is
/// usually a handful of albums however many tracks it holds, and BluOS gives
/// every track from one album the same artwork URL. The republishes are
/// coalesced, so a queue filling in redraws a few times rather than once per
/// image.
async fn load_thumbnails(backend: Backend, id: DeviceId) {
    let urls: Vec<String> = {
        let guard = backend.registry.lock().unwrap();
        let Some(entry) = guard.get(&id) else { return };
        let Some(queue) = &entry.queue else { return };

        let mut seen = std::collections::BTreeSet::new();
        queue
            .songs
            .iter()
            .filter_map(|song| song.image.as_deref().filter(|src| !src.is_empty()))
            .map(|src| entry.client.image_url(src))
            .filter(|url| seen.insert(url.clone()))
            .collect()
    };

    if urls.is_empty() {
        return;
    }
    tracing::debug!(%id, covers = urls.len(), "fetching queue thumbnails");

    let mut fetches = tokio::task::JoinSet::new();
    for url in urls {
        let artwork = backend.artwork.clone();
        fetches.spawn(async move {
            artwork.get(&url, THUMB_SIZE).await;
        });
    }

    let mut last_publish = Instant::now();
    while fetches.join_next().await.is_some() {
        // The user moved on; the rest of these covers are for a queue nobody
        // is looking at.
        if !backend.is_selected(id) {
            fetches.abort_all();
            return;
        }
        if last_publish.elapsed() >= Duration::from_millis(150) {
            backend.publish_queue();
            last_publish = Instant::now();
        }
    }
    backend.publish_queue();
}

/// Read the selected player's queue and show it.
async fn fetch_queue(backend: Backend, id: DeviceId) {
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return;
    };

    match client.queue_range(0, QUEUE_WINDOW - 1).await {
        Ok(queue) => {
            tracing::debug!(
                %id,
                tracks = queue.songs.len(),
                total = queue.length,
                "read the queue"
            );
            if let Some(entry) = backend.registry.lock().unwrap().get_mut(&id) {
                entry.queue = Some(queue);
            }
            backend.publish_queue();
            tokio::spawn(fetch_queue_buttons(backend.clone(), id));
            tokio::spawn(load_thumbnails(backend, id));
        }
        Err(e) => tracing::debug!(%id, "could not read the queue: {e}"),
    }
}

/// Read the queue's own document, for the buttons the player puts under it.
///
/// Separate from the rows, which come from `/Playlist`: that endpoint pages and
/// this one names the actions. Failing is not worth reporting — the queue still
/// draws, just without its buttons.
async fn fetch_queue_buttons(backend: Backend, id: DeviceId) {
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return;
    };
    let uri = backend
        .browsing
        .lock()
        .unwrap()
        .queue_uri
        .clone()
        .unwrap_or_else(|| "/ui/Queue".to_owned());

    match client.screen(&uri).await {
        Ok(screen) => {
            {
                let mut browsing = backend.browsing.lock().unwrap();
                browsing.queue_screen = Some(screen);
            }
            backend.publish_queue();
        }
        Err(e) => tracing::debug!(%id, "no queue document: {e}"),
    }
}

/// Keep one player's row, its queue and its MPRIS object current for as long as
/// the app runs.
async fn follow(backend: Backend, id: DeviceId, mpris_index: usize) {
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return;
    };

    // The announcement already gave a name; /SyncStatus gives the authoritative
    // one, and MPRIS wants it before the bus name is claimed so the desktop
    // never shows a placeholder.
    let mut name = backend
        .with_entry(id, |e| e.view.name.to_string())
        .unwrap_or_default();
    if let Some(sync) = read_sync(&backend, id, &client).await {
        name = sync.name.clone();
    }

    // Deliberately not exported yet. An address that never answers — one typed
    // wrongly, or a player that has since been unplugged — would otherwise sit
    // in the desktop's media controls forever, claiming to be a player nobody
    // can reach. The bus name is claimed on the first real status instead.
    let mut mpris: Option<mpris::Bridge> = None;

    let mut watch = client.watch();
    let mut backoff = Duration::from_secs(1);
    let mut unreadable = 0u32;

    loop {
        match watch.next().await {
            Ok(status) => {
                backoff = Duration::from_secs(1);
                unreadable = 0;

                // `pid` is the queue's identity. When it changes the player has
                // replaced the queue, which is the cue the device itself gives
                // through `refreshOnStatusChange` on its queue screen.
                //
                // The first status of all is not a replacement, however
                // different it looks from the nothing that preceded it —
                // treating it as one would throw away the queue the adoption
                // just fetched and fetch it again.
                let (queue_replaced, indexed) = {
                    let mut guard = backend.registry.lock().unwrap();
                    let Some(entry) = guard.get_mut(&id) else {
                        return;
                    };
                    let replaced = match &entry.status {
                        Some(previous) => previous.pid != status.pid,
                        None => false,
                    };
                    // The banner says nothing once the count stops, so the one
                    // moment worth a word is the moment it stops: the count was
                    // climbing and now it is not.
                    let indexed = match &entry.status {
                        Some(previous) => {
                            previous.indexing.unwrap_or(0) > 0 && status.indexing.unwrap_or(0) == 0
                        }
                        None => false,
                    };
                    if replaced {
                        entry.queue = None;
                    }
                    entry.status = Some(status.clone());
                    entry.status_at = Some(Instant::now());
                    (replaced, indexed)
                };

                if indexed && backend.is_selected(id) {
                    say(&backend.ui, "Music library index complete");
                }

                backend.update(id, |view| {
                    view.reachable = true;
                    view.playing = status.is_playing();
                    view.muted = status.is_muted();
                    view.volume = status.volume.unwrap_or(0);
                    view.now_playing = status.now_playing().unwrap_or_default().into();
                    // Split as well as combined: the player list has one line
                    // to spare and wants them joined, while the now-playing
                    // panel gives the title its own size and the artist a
                    // quieter one under it.
                    view.title = status.title1.clone().unwrap_or_default().into();
                    view.album = status.album.clone().unwrap_or_default().into();
                    view.artist = status
                        .artist
                        .clone()
                        .or_else(|| status.title2.clone())
                        .unwrap_or_default()
                        .into();
                    view.service = status
                        .service_name
                        .clone()
                        .or_else(|| status.service.clone())
                        .unwrap_or_default()
                        .into();
                });

                // `syncStat` mirrors /SyncStatus's own etag, so a change in it
                // means the player's grouping moved — without a second request
                // to find that out.
                let regrouped = {
                    let guard = backend.registry.lock().unwrap();
                    guard.get(&id).is_some_and(|entry| {
                        entry.sync.as_ref().map(|s| s.etag.as_str()) != status.sync_stat.as_deref()
                    })
                };
                if regrouped {
                    let backend = backend.clone();
                    let client = client.clone();
                    tokio::spawn(async move {
                        read_sync(&backend, id, &client).await;
                    });
                }

                // Resolved here rather than at draw time: the path is relative
                // to *this* player unless the service handed back an absolute
                // URL, and only the client knows which.
                let art = status.artwork().map(|src| client.image_url(src));
                let art_changed = {
                    let mut guard = backend.registry.lock().unwrap();
                    match guard.get_mut(&id) {
                        Some(entry) if entry.cover_url != art => {
                            entry.cover_url = art;
                            true
                        }
                        _ => false,
                    }
                };
                if art_changed && backend.is_selected(id) {
                    tokio::spawn(load_cover(backend.clone(), id));
                }

                // A screen carries the status values it was built against; when
                // one of them moves the player is saying the drawing is out of
                // date. Re-fetch in place rather than dropping the trail.
                let stale = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing.device == Some(id)
                        && browsing
                            .trail
                            .last()
                            .is_some_and(|crumb| crumb.screen.is_stale(&status))
                };
                if stale {
                    tokio::spawn(refresh_current(backend.clone()));
                }

                if backend.is_selected(id) {
                    if queue_replaced {
                        tokio::spawn(fetch_queue(backend.clone(), id));
                    } else {
                        // Same queue, but the cursor may have moved to the next
                        // track.
                        backend.publish_queue();
                    }
                }

                // The settings pane can be showing a row whose state is in
                // the status rather than in the settings document — the sleep
                // timer is one — so it is redrawn along with everything else.
                if backend.is_selected(id)
                    && matches!(backend.browsing.lock().unwrap().pane, Pane::Settings(_))
                {
                    backend.publish_settings();
                }

                if backend.is_selected(id) {
                    backend.publish_transport();
                }

                if mpris.is_none() {
                    let name = backend
                        .with_entry(id, |e| e.view.name.to_string())
                        .unwrap_or_else(|| name.clone());
                    mpris = mpris::Bridge::attach(
                        mpris_index,
                        id,
                        name,
                        backend.registry.clone(),
                        backend.commands.clone(),
                    )
                    .await;
                }
                if let Some(bridge) = &mpris {
                    bridge.publish(&status).await;
                }
            }
            Err(e) => {
                // Two different failures wearing one shape. A player that
                // cannot be reached is offline and the app should say so; a
                // player that answered with a document this crate cannot read
                // is perfectly alive, and it does emit one on occasion while
                // it changes input. Taking the whole app offline over that —
                // greying the transport and writing "Not responding" under
                // the name — is what makes switching inputs look like a
                // freeze, and it lasted as long as the backoff.
                let answered = matches!(e, bluos::Error::Xml { .. });
                let wait = if answered {
                    unreadable += 1;
                    // A run of them is a real disagreement about the format
                    // rather than a blip, and then it is worth slowing down
                    // instead of hammering the player.
                    if unreadable > 3 {
                        backoff
                    } else {
                        Duration::from_millis(200)
                    }
                } else {
                    unreadable = 0;
                    backend.update(id, |view| view.reachable = false);

                    // The last known track stays on the bus — a blip should
                    // not wipe the desktop's media widget — but it stops
                    // claiming to be playing something it can no longer see.
                    if let Some(bridge) = &mpris {
                        bridge.publish_offline().await;
                    }
                    backoff
                };

                tracing::debug!(%id, "poll failed, retrying in {wait:?}: {e}");
                tokio::time::sleep(wait).await;
                if wait == backoff {
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }

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
    backend: Backend,
    discovery: Arc<Discovery>,
    http: reqwest::Client,
) {
    while let Some(command) = commands.recv().await {
        let (id, action) = match command {
            Command::Rescan => {
                // Spawned rather than awaited: a sweep runs for twelve seconds
                // and this loop is what carries every other command.
                tokio::spawn(sweep(backend.clone(), discovery.clone(), http.clone()));
                continue;
            }
            Command::Select(id) => {
                let already = {
                    let mut selected = backend.selected.lock().unwrap();
                    let already = *selected == Some(id);
                    *selected = Some(id);
                    already
                };
                // Show whatever is already known straight away, and only go to
                // the network when this is a player whose queue is not held.
                backend.publish_queue();
                backend.publish_transport();
                tokio::spawn(load_cover(backend.clone(), id));
                // Warm the browser for this player, so the tab is not a wait.
                let fresh = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing.device != Some(id) || browsing.trail.is_empty()
                };
                if fresh {
                    let _ = backend.commands.send(Command::BrowseHome);
                }
                if !already
                    || backend
                        .with_entry(id, |e| e.queue.is_none())
                        .unwrap_or(false)
                {
                    tokio::spawn(fetch_queue(backend.clone(), id));
                }
                continue;
            }
            Command::AddPlayer(text) => {
                let text = text.trim();
                match text.parse::<DeviceId>() {
                    Ok(id) => {
                        say(&backend.ui, format!("looking for a player at {id}"));
                        backend.track(id, &http, None, None);
                    }
                    Err(_) => say(&backend.ui, format!("{text:?} is not an address")),
                }
                continue;
            }

            Command::BrowseHome => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                // Already showing this player's screens: leave the trail where
                // the user left it rather than resetting to the root.
                {
                    let browsing = backend.browsing.lock().unwrap();
                    if browsing.device == Some(id) && !browsing.trail.is_empty() {
                        drop(browsing);
                        backend.publish_pane();
                        continue;
                    }
                }

                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                // The player says which screens it has; the app does not have a
                // list of its own.
                let config = client.ui_configuration().await.ok();
                let screens = config.as_ref().map(user_screens).unwrap_or_default();
                let root = screens
                    .first()
                    .map(|(_, uri)| uri.clone())
                    .unwrap_or_else(|| "/ui/Sources".to_owned());

                // The sidebar's lower half is the Sources screen's own rows,
                // so it is read once here rather than every time the sidebar
                // is redrawn.
                let sources_uri = config
                    .as_ref()
                    .and_then(|c| c.uri("sources"))
                    .unwrap_or("/ui/Sources")
                    .to_owned();
                let sources = client.screen(&sources_uri).await.ok();

                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    browsing.queue_uri = config
                        .as_ref()
                        .and_then(|c| c.uri("queue"))
                        .map(str::to_owned);
                    browsing.now_playing_menu = config
                        .as_ref()
                        .and_then(|c| c.uri("nowPlayingContextMenu"))
                        .map(str::to_owned);
                    browsing.queue_menu_uri = config
                        .as_ref()
                        .and_then(|c| c.uri("queueItemContextMenu"))
                        .map(str::to_owned);
                    browsing.screens = screens;
                    browsing.sources = sources;
                    browsing.highlighted = Some((0, 0));
                }
                backend.publish_sidebar();
                tokio::spawn(open_screen(backend.clone(), id, root, Arrive::Root));
                continue;
            }

            Command::BrowseBack => {
                // One level, whatever a level is where you are standing: out of
                // a page reached from Help back to Help, out of a settings
                // sub-page back to the page above it, out of a pane back to
                // browsing, and only then back along the trail of screens.
                let moved = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    match &mut browsing.pane {
                        Pane::HelpDetail(..) => {
                            browsing.pane = Pane::Help;
                            true
                        }
                        Pane::Settings(trail) if trail.len() > 1 => {
                            trail.pop();
                            true
                        }
                        // Back out of a service's sign-in form lands on the
                        // list of services it was chosen from.
                        Pane::Form(page) if page.from.is_some() => {
                            let from = page.from.take();
                            if let Some(page) = from {
                                browsing.pane = Pane::Web(page);
                            }
                            true
                        }
                        Pane::Help
                        | Pane::Settings(_)
                        | Pane::Web(_)
                        | Pane::Form(_)
                        | Pane::Customise(_)
                        | Pane::NowPlaying => {
                            browsing.pane = Pane::Browse;
                            true
                        }
                        // Already browsing, so back means the screen before.
                        Pane::Browse => {
                            let deeper = browsing.trail.len() > 1;
                            if deeper {
                                browsing.trail.pop();
                            }
                            deeper
                        }
                    }
                };
                if moved {
                    backend.publish_pane();
                }
                continue;
            }

            Command::BrowseActivate(index) => {
                // Every pane but Browse draws settings-shaped rows.
                let in_settings = !matches!(backend.browsing.lock().unwrap().pane, Pane::Browse);
                if in_settings {
                    let _ = backend.commands.send(Command::SettingAction(index));
                } else {
                    tokio::spawn(activate(backend.clone(), index));
                }
                continue;
            }

            Command::BrowseMenu(index) => {
                let opened = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing.device.zip(
                        browsing
                            .current()
                            .and_then(|screen| screen.items().nth(index))
                            .and_then(|item| item.context_menu.as_ref())
                            .and_then(|action| action.uri.clone()),
                    )
                };
                if let Some((id, uri)) = opened {
                    tokio::spawn(open_screen(backend.clone(), id, uri, Arrive::Deeper));
                }
                continue;
            }

            Command::QueueMenu(song) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                // The player names the endpoint in /ui/Configuration; only the
                // queue position is ours to add.
                let base = backend
                    .browsing
                    .lock()
                    .unwrap()
                    .queue_menu_uri
                    .clone()
                    .unwrap_or_else(|| "/ui/queueItemCM".to_owned());
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.screen(&format!("{base}?id={song}")).await {
                        Ok(menu) => {
                            backend.browsing.lock().unwrap().queue_menu = Some(menu);
                            backend.publish_queue_menu();
                        }
                        Err(e) => {
                            tracing::debug!(%id, "no menu for queue item {song}: {e}");
                            say(&backend.ui, "The player offers nothing for that track");
                        }
                    }
                });
                continue;
            }

            Command::QueueMenuAction(at) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let action = backend
                    .browsing
                    .lock()
                    .unwrap()
                    .queue_menu
                    .as_ref()
                    .and_then(|menu| menu.items().nth(at))
                    .and_then(|item| item.action.clone());

                if let Some(action) = action {
                    let backend = backend.clone();
                    tokio::spawn(async move {
                        run_action(backend.clone(), id, action, Arrive::Deeper).await;
                        // Favouriting and deleting both change the queue, and
                        // the player announces neither.
                        fetch_queue(backend, id).await;
                    });
                }
                continue;
            }

            Command::OpenSettings(page, step) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                match client.settings(page.as_deref()).await {
                    Ok(page) => {
                        let mut browsing = backend.browsing.lock().unwrap();
                        match (&mut browsing.pane, step) {
                            // Deeper and reload both keep whatever is under
                            // them; only the top of the trail differs.
                            (Pane::Settings(trail), Step::Deeper) => trail.push(page),
                            (Pane::Settings(trail), Step::Reload) if !trail.is_empty() => {
                                let top = trail.len() - 1;
                                trail[top] = page;
                            }
                            _ => browsing.pane = Pane::Settings(vec![page]),
                        }
                        // Settings and Help are rows of their own below the
                        // list, so nothing in the list is where you are any
                        // more. Leaving the last screen lit says you are on a
                        // screen you are not looking at.
                        browsing.highlighted = None;
                        drop(browsing);
                        backend.publish_sidebar();
                        backend.publish_settings();
                    }
                    Err(e) => {
                        tracing::warn!(%id, "could not read settings: {e}");
                        say(&backend.ui, format!("could not read settings: {e}"));
                    }
                }
                continue;
            }

            Command::SettingAction(index) => {
                // Three panes are drawn with these rows, so the press goes to
                // whichever one is actually on screen.
                match backend.browsing.lock().unwrap().pane {
                    Pane::Help | Pane::HelpDetail(..) => {
                        let _ = backend.commands.send(Command::HelpAction(index));
                        continue;
                    }
                    Pane::Web(_) => {
                        let _ = backend.commands.send(Command::WebAction(index));
                        continue;
                    }
                    Pane::Form(_) => {
                        let _ = backend.commands.send(Command::FormPress(index));
                        continue;
                    }
                    _ => {}
                }

                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                // The row index counts groups-that-are-links and settings, in
                // the order settings_rows walks them, so the same walk finds it.
                let chosen = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing.pane.settings().and_then(|page| pick(page, index))
                };

                match chosen {
                    Some(Chosen::Page(id)) => {
                        let _ = backend
                            .commands
                            .send(Command::OpenSettings(Some(id), Step::Deeper));
                    }
                    Some(Chosen::Sleep) => {
                        let _ = backend.commands.send(Command::Player(id, Action::Sleep));
                    }
                    Some(Chosen::Web(url)) => {
                        // Except the share configuration, which is a list of
                        // what is mounted and a button to unmount it. That is
                        // worth drawing; the page that adds one asks for a
                        // password and is not.
                        if url.contains("/sharecfg") {
                            let _ = backend.commands.send(Command::OpenShares);
                            continue;
                        }
                        // Joining a wireless network: a list of what the player
                        // can see and a key to type. All of it is a form, and
                        // the only reason it ever left the app is that nothing
                        // here could draw one.
                        if let Some(path) = url.split_once("://").map(|(_, rest)| rest)
                            && let Some(path) = path.split_once('/').map(|(_, rest)| rest)
                            && path.starts_with("wificfg")
                        {
                            let _ = backend.commands.send(Command::OpenForm {
                                title: "WiFi".to_owned(),
                                path: format!("/{path}"),
                            });
                            continue;
                        }
                        open_in_browser(&backend.ui, url).await;
                    }
                    Some(Chosen::Write(setting, value)) => {
                        let page = backend.browsing.lock().unwrap().pane.settings().cloned();
                        if let Some(page) = page {
                            match client.write_setting(&page, &setting, &value).await {
                                Ok(()) => {
                                    // A button leaves nothing behind on the
                                    // page it was pressed on — no toggle moves,
                                    // no value changes — so without a word it
                                    // is indistinguishable from a dead control.
                                    // Reindexing takes minutes before the
                                    // player admits it started.
                                    if setting.kind == Kind::Button {
                                        say(&backend.ui, format!("{}…", setting.label()));
                                    }
                                    // Re-read rather than guess: a write can
                                    // change more than the one value, and the
                                    // player is the only one who knows.
                                    let _ = backend.commands.send(Command::OpenSettings(
                                        page.page_id.clone(),
                                        Step::Reload,
                                    ));
                                }
                                Err(e) => {
                                    say(&backend.ui, format!("{}: {e}", setting.label()));
                                }
                            }
                        }
                    }
                    None => {}
                }
                continue;
            }

            Command::OpenHelp => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    browsing.pane = Pane::Help;
                    browsing.highlighted = None;
                }
                backend.publish_sidebar();
                backend.publish_help();
                continue;
            }

            Command::HelpAction(index) => {
                let Some((_, kind, _, _)) = HELP_ENTRIES.get(index) else {
                    // The About row, which is text rather than a link.
                    continue;
                };
                let client = backend
                    .selected
                    .lock()
                    .unwrap()
                    .and_then(|id| backend.with_entry(id, |e| e.client.clone()));

                match kind {
                    HelpKind::Web(target) => {
                        // Absolute for Lenbrook's own site, relative for the
                        // pages the player serves; image_url knows which.
                        let url = match &client {
                            Some(client) => client.image_url(target),
                            None => (*target).to_owned(),
                        };
                        open_in_browser(&backend.ui, url).await;
                    }
                    HelpKind::Diagnostics => {
                        let Some(client) = client else { continue };
                        match client.diagnostics().await {
                            Ok(facts) if !facts.is_empty() => {
                                backend.browsing.lock().unwrap().pane =
                                    Pane::HelpDetail("Diagnostics".to_owned(), facts);
                                backend.publish_help();
                            }
                            // The page is the player's own HTML, so it can
                            // change under us; offer it rather than nothing.
                            _ => {
                                let url = client.image_url("/redirectToCp?href=/diagnostics");
                                open_in_browser(&backend.ui, url).await;
                            }
                        }
                    }
                    HelpKind::Upgrade => {
                        let Some(client) = client else { continue };
                        match client.upgrade_check().await {
                            Ok((status, action)) => {
                                let mut facts = vec![(
                                    "Status".to_owned(),
                                    status.unwrap_or_else(|| {
                                        "The player did not answer clearly".to_owned()
                                    }),
                                )];
                                // Only present when the player itself offers
                                // one; see reports::upgrade_action.
                                if let Some((label, href)) = action {
                                    facts.push((label, client.image_url(&href)));
                                }
                                backend.browsing.lock().unwrap().pane =
                                    Pane::HelpDetail("Upgrade Check".to_owned(), facts);
                                backend.publish_help();
                            }
                            Err(e) => say(&backend.ui, format!("upgrade check failed: {e}")),
                        }
                    }
                }
                continue;
            }

            Command::SettingEdit(index, edit)
                if matches!(backend.browsing.lock().unwrap().pane, Pane::Form(_)) =>
            {
                let _ = backend.commands.send(Command::FormEdit(index, edit));
                continue;
            }

            Command::OpenForm { title, path } => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                // Up before the request, not after it. Asking the player for
                // its wireless page makes it scan for access points first, and
                // that is three and a half seconds of a window that looks like
                // it ignored the press.
                show_form(
                    &backend,
                    title.clone(),
                    bluos::forms::Form::default(),
                    "Reading the page…".to_owned(),
                );

                match client.web_form(&path).await {
                    Ok(Some(form)) => {
                        show_form(&backend, title, form, String::new());
                    }
                    // No form on it, or a shape this crate cannot read. The
                    // page itself still works, so offer that rather than
                    // nothing.
                    other => {
                        if let Err(e) = other {
                            tracing::debug!(%id, "could not read {path}: {e}");
                        }
                        // Nothing here to draw, so put the page itself up and
                        // leave the placeholder behind.
                        backend.browsing.lock().unwrap().pane = Pane::Browse;
                        backend.publish_pane();
                        match client.web_url(&path) {
                            Ok(url) => {
                                open_in_browser(&backend.ui, url).await;
                            }
                            // The player named somewhere that is not the
                            // player. Handing that to the desktop's browser is
                            // the one thing this app must not do on its say-so.
                            Err(e) => {
                                tracing::warn!(%id, "refusing to open {path}: {e}");
                                say(&backend.ui, "That page is not on this player");
                            }
                        }
                    }
                }
                continue;
            }

            Command::FormEdit(at, edit) => {
                let mut browsing = backend.browsing.lock().unwrap();
                let Pane::Form(page) = &mut browsing.pane else {
                    continue;
                };
                // The note, when there is one, sits above the fields and is not
                // one of them.
                let at = at.wrapping_sub(usize::from(!page.note.is_empty()));
                let Some(field) = page.form.fields.get(at) else {
                    continue;
                };

                let value = match edit {
                    Edit::Text(text) => Some(text),
                    Edit::Choose(n) => field.choices.get(n).map(|c| c.value.clone()),
                    Edit::Toggle => Some(
                        match page.values.get(&field.name).map(String::as_str) {
                            Some("") | None => "on",
                            _ => "",
                        }
                        .to_owned(),
                    ),
                    Edit::Number(_) => None,
                };
                if let Some(value) = value {
                    page.values.insert(field.name.clone(), value);
                }
                drop(browsing);
                backend.publish_form();
                continue;
            }

            Command::FormPress(at) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };

                let sending = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Some(page) = browsing.pane.form() else {
                        continue;
                    };
                    let first = usize::from(!page.note.is_empty()) + page.form.fields.len();
                    page.form.submits.get(at.wrapping_sub(first)).map(|submit| {
                        (
                            page.title.clone(),
                            page.form.clone(),
                            page.values.clone(),
                            submit.clone(),
                        )
                    })
                };
                let Some((title, form, values, submit)) = sending else {
                    continue;
                };

                match client.submit_form(&form, &values, &submit).await {
                    Ok(body) => {
                        // The answer is another page: the same form with a
                        // message when something was wrong, the next step's
                        // form when it was right. Following it is what lets one
                        // screen lead to another without knowing the route.
                        match bluos::forms::parse(&body).into_iter().next() {
                            Some(next) => {
                                show_form(&backend, title, next, bluos::reports::message(&body))
                            }
                            None => {
                                let said = bluos::reports::message(&body);
                                say(
                                    &backend.ui,
                                    if said.is_empty() {
                                        format!("{} done", submit.label)
                                    } else {
                                        said
                                    },
                                );
                                backend.browsing.lock().unwrap().pane = Pane::Browse;
                                backend.publish_pane();
                            }
                        }
                    }
                    Err(e) => say(&backend.ui, format!("{}: {e}", submit.label)),
                }
                continue;
            }

            Command::SettingEdit(index, edit) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                let Some(page) = backend.browsing.lock().unwrap().pane.settings().cloned() else {
                    continue;
                };
                let Some(setting) = setting_at(&page, index) else {
                    continue;
                };

                let value = match edit {
                    Edit::Toggle => setting.toggled(),
                    Edit::Choose(n) => setting.options.get(n).map(|o| o.name.clone()),
                    // The player wants the number as it writes it: a whole one
                    // where the step is whole, and one decimal where it is not.
                    Edit::Number(v) => Some(match setting.range.as_ref().and_then(|r| r.step) {
                        Some(step) if step.fract() != 0.0 => format!("{v:.1}"),
                        _ => format!("{}", v.round() as i64),
                    }),
                    Edit::Text(text) => Some(text),
                };
                let Some(value) = value else { continue };

                match client.write_setting(&page, &setting, &value).await {
                    Ok(()) => {
                        // Re-read rather than assume: a write can move more
                        // than the one value — turning tone controls on brings
                        // treble and bass to life — and only the player knows.
                        let _ = backend
                            .commands
                            .send(Command::OpenSettings(page.page_id.clone(), Step::Reload));
                    }
                    Err(e) => say(&backend.ui, format!("{}: {e}", setting.label())),
                }
                continue;
            }

            Command::Sidebar(kind, index) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                if kind != 2 {
                    backend.browsing.lock().unwrap().highlighted = Some((kind, index));
                    backend.publish_sidebar();

                    // Pressing anything in the sidebar means leaving whatever
                    // pane is covering the screens. Most entries open one and
                    // leave it that way themselves, but an input does not —
                    // switching to Bluetooth sends a command and shows nothing
                    // — so from Settings it left the settings rows on screen.
                    let covered = {
                        let mut browsing = backend.browsing.lock().unwrap();
                        let covered = !matches!(browsing.pane, Pane::Browse);
                        if covered {
                            browsing.pane = Pane::Browse;
                        }
                        covered
                    };
                    if covered {
                        backend.publish_pane();
                    }
                }

                if kind == 2 {
                    // The button on a section heading. Whatever it does is the
                    // section's own menu action — a settings page for Inputs,
                    // the services page for Music Services.
                    let action = {
                        let browsing = backend.browsing.lock().unwrap();
                        browsing.sources.as_ref().and_then(|screen| {
                            screen
                                .sections
                                .get(index.max(0) as usize)
                                .and_then(|section| section.menu_actions.first())
                                .and_then(|menu| menu.action.clone())
                        })
                    };
                    if let Some(action) = action {
                        tokio::spawn(run_action(backend.clone(), id, action, Arrive::Deeper));
                    }
                } else if kind == 0 {
                    let uri = backend
                        .browsing
                        .lock()
                        .unwrap()
                        .screens
                        .get(index.max(0) as usize)
                        .map(|(_, uri)| uri.clone());
                    if let Some(uri) = uri {
                        tokio::spawn(open_screen(backend.clone(), id, uri, Arrive::Root));
                    }
                } else {
                    // An entry off the Sources screen: run whatever that item
                    // says, which is a browse for a service and a play command
                    // for an input.
                    let action = {
                        let browsing = backend.browsing.lock().unwrap();
                        browsing.sources.as_ref().and_then(|screen| {
                            screen.items().nth(index.max(0) as usize).and_then(|item| {
                                item.action.clone().or_else(|| item.play_action.clone())
                            })
                        })
                    };
                    if let Some(action) = action {
                        tokio::spawn(run_action(backend.clone(), id, action, Arrive::Deeper));
                    }
                }
                continue;
            }

            Command::BrowseSearch(query) => {
                // Typing is the search; there is no Enter to wait for. Held
                // back until the typing settles so a word costs one request,
                // and numbered so that only the last one asked for lands.
                let generation = backend.searches.fetch_add(1, Ordering::Relaxed) + 1;
                let backend = backend.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(SEARCH_SETTLE).await;
                    if backend.searches.load(Ordering::Relaxed) != generation {
                        return;
                    }
                    run_search(backend, query).await;
                });
                continue;
            }

            Command::QueueReorder(from, to) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                match client.move_queue_item(from, to).await {
                    // The player does not announce a reorder — the queue's own
                    // id does not change — so the list is re-read rather than
                    // waited for.
                    Ok(()) => tokio::spawn(fetch_queue(backend.clone(), id)),
                    Err(e) => {
                        say(&backend.ui, format!("could not move the track: {e}"));
                        continue;
                    }
                };
                continue;
            }

            Command::QueueRemove(index) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                match client.delete_queue_item(index).await {
                    Ok(()) => tokio::spawn(fetch_queue(backend.clone(), id)),
                    Err(e) => {
                        say(&backend.ui, format!("could not remove the track: {e}"));
                        continue;
                    }
                };
                continue;
            }

            Command::QueueSave(name) => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    continue;
                }
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                match client.save_queue(&name).await {
                    Ok(()) => {
                        say(&backend.ui, format!("Saved as \"{name}\""));
                        tokio::spawn(fetch_queue(backend.clone(), id));
                    }
                    Err(e) => say(&backend.ui, format!("could not save the queue: {e}")),
                }
                continue;
            }

            Command::QueueButton(at) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let action = backend
                    .browsing
                    .lock()
                    .unwrap()
                    .queue_screen
                    .as_ref()
                    .and_then(|screen| screen.buttons.get(at))
                    .and_then(|button| button.action.clone());
                if let Some(action) = action {
                    let backend = backend.clone();
                    tokio::spawn(async move {
                        run_action(backend.clone(), id, action, Arrive::Deeper).await;
                        // Every one of these changes the queue, and the player
                        // reports none of them: clearing keeps the same queue
                        // id, and turning a mode on changes only what the next
                        // screen looks like.
                        fetch_queue(backend, id).await;
                    });
                }
                continue;
            }

            Command::OpenServices => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                match client.services().await {
                    Ok(services) if !services.is_empty() => {
                        let mut browsing = backend.browsing.lock().unwrap();
                        browsing.pane = Pane::Web(WebPage::Services(services));
                        browsing.highlighted = None;
                        drop(browsing);
                        backend.publish_sidebar();
                        backend.publish_web();
                    }
                    // The page is the player's own HTML and a firmware update
                    // could change its shape. Falling back to opening it beats
                    // showing an empty list and claiming there are no services.
                    other => {
                        if let Err(e) = other {
                            tracing::debug!(%id, "could not read the services page: {e}");
                        }
                        // A constant, so this cannot be off-player.
                        if let Ok(url) = client.web_url("/services?noheader=1") {
                            open_in_browser(&backend.ui, url).await;
                        }
                    }
                }
                continue;
            }

            Command::OpenShares => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                match client.shares().await {
                    Ok((action, shares)) => {
                        let mut browsing = backend.browsing.lock().unwrap();
                        browsing.pane = Pane::Web(WebPage::Shares { action, shares });
                        browsing.highlighted = None;
                        drop(browsing);
                        backend.publish_sidebar();
                        backend.publish_web();
                    }
                    Err(e) => {
                        tracing::debug!(%id, "could not read the shares page: {e}");
                        // A constant, so this cannot be off-player.
                        if let Ok(url) = client.web_url("/sharecfg?noheader=1") {
                            open_in_browser(&backend.ui, url).await;
                        }
                    }
                }
                continue;
            }

            Command::WebAction(at) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };

                enum Press {
                    /// A page with a form on it, filled in here.
                    Form(String, String),
                    Remove(String, String),
                }

                let press = {
                    let browsing = backend.browsing.lock().unwrap();
                    match browsing.pane.web() {
                        Some(WebPage::Services(services)) => services
                            .get(at)
                            .map(|service| Press::Form(service.name.clone(), service.href.clone())),
                        Some(WebPage::Shares { action, shares }) => match shares.get(at) {
                            // A share, and the form that unmounts it.
                            Some(share) => action
                                .clone()
                                .map(|action| Press::Remove(action, share.field.clone())),
                            // Past the last share is the row that adds one,
                            // which asks for a server and a password.
                            None => Some(Press::Form(
                                "Network shares".to_owned(),
                                "/sharecfg?noheader=1".to_owned(),
                            )),
                        },
                        None => None,
                    }
                };

                match press {
                    Some(Press::Form(title, path)) => {
                        let _ = backend.commands.send(Command::OpenForm { title, path });
                    }
                    Some(Press::Remove(action, field)) => {
                        match client
                            .remove_shares(&action, std::slice::from_ref(&field))
                            .await
                        {
                            Ok(()) => {
                                say(&backend.ui, "Share removed");
                                let _ = backend.commands.send(Command::OpenShares);
                            }
                            Err(e) => say(&backend.ui, format!("could not remove {field}: {e}")),
                        }
                    }
                    None => {}
                }
                continue;
            }

            Command::NowPlayingInfo => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                // Through the player's own menu rather than by building a
                // request: it already offers Technical info for whatever is
                // playing, and it knows the file name this app never sees.
                let uri = backend
                    .browsing
                    .lock()
                    .unwrap()
                    .now_playing_menu
                    .clone()
                    .unwrap_or_else(|| "/ui/nowPlayingCM".to_owned());

                match client.screen(&uri).await {
                    Ok(menu) => {
                        let action = menu.items().find_map(|item| {
                            let action = item.action.as_ref()?;
                            (action.result_type.as_deref() == Some("BriefInfo"))
                                .then(|| action.clone())
                        });
                        match action {
                            Some(action) => {
                                run_action(backend.clone(), id, action, Arrive::Deeper).await;
                            }
                            None => say(&backend.ui, "The player offers nothing about this track"),
                        }
                    }
                    Err(e) => tracing::debug!(%id, "no now-playing menu: {e}"),
                }
                continue;
            }

            Command::ClearRecent => {
                backend.browsing.lock().unwrap().recent.clear();
                backend.publish_browse();
                continue;
            }

            Command::BrowseMore => {
                let asking = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    if browsing.fetching_more {
                        continue;
                    }
                    let next = browsing
                        .current()
                        .and_then(|screen| screen.next.clone())
                        .zip(browsing.device);
                    if next.is_some() {
                        browsing.fetching_more = true;
                    }
                    next
                };
                let Some((next, id)) = asking else { continue };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    backend.browsing.lock().unwrap().fetching_more = false;
                    continue;
                };

                match client.screen(&next).await {
                    Ok(more) => {
                        {
                            let mut browsing = backend.browsing.lock().unwrap();
                            browsing.fetching_more = false;
                            if let Some(crumb) = browsing.trail.last_mut() {
                                // Added to the section already on screen rather
                                // than as one of its own: it is the same list,
                                // continued, and a heading between page one and
                                // page two would be inventing a division the
                                // player never made.
                                let arriving: Vec<_> = more
                                    .sections
                                    .into_iter()
                                    .flat_map(|section| section.items)
                                    .collect();
                                if let Some(section) = crumb.screen.sections.last_mut() {
                                    section.items.extend(arriving);
                                }
                                crumb.screen.next = more.next;
                            }
                        }
                        backend.publish_browse();
                        tokio::spawn(load_browse_thumbnails(backend.clone(), id));
                    }
                    Err(e) => {
                        backend.browsing.lock().unwrap().fetching_more = false;
                        tracing::debug!(%id, "could not read {next}: {e}");
                    }
                }
                continue;
            }

            Command::ToggleNowPlaying => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    browsing.pane = match browsing.pane {
                        Pane::NowPlaying => Pane::Browse,
                        _ => Pane::NowPlaying,
                    };
                }
                backend.publish_pane();
                continue;
            }

            Command::BrowseHeader(at) => {
                let found = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing.device.zip(
                        browsing
                            .current()
                            .and_then(|screen| screen.header.as_ref())
                            .and_then(|header| header.buttons.get(at))
                            .and_then(|button| button.action.clone()),
                    )
                };
                if let Some((id, action)) = found {
                    tokio::spawn(run_action(backend.clone(), id, action, Arrive::Deeper));
                }
                continue;
            }

            Command::BrowseSection(at) => {
                let found = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing.device.zip(
                        browsing
                            .current()
                            .and_then(|screen| screen.sections.get(at))
                            .and_then(|section| {
                                // An empty section is its own action; a shelf
                                // with content puts one on its heading instead.
                                if section.items.is_empty() {
                                    section.action.clone()
                                } else {
                                    section
                                        .menu_actions
                                        .first()
                                        .and_then(|menu| menu.action.clone())
                                }
                            }),
                    )
                };
                if let Some((id, action)) = found {
                    tokio::spawn(run_action(backend.clone(), id, action, Arrive::Deeper));
                }
                continue;
            }

            // Enter, which is not needed to search but does say the search was
            // the one that mattered — so this is the only place a query is
            // written to the recent list.
            Command::BrowseSearchDone(query) => {
                let query = query.trim().to_owned();
                if !query.is_empty() {
                    remember_search(&backend, query);
                }
                continue;
            }

            Command::ToggleShuffle(id) => {
                let on = backend
                    .with_entry(id, |e| e.status.as_ref().is_some_and(|s| s.shuffle_on()))
                    .unwrap_or(false);
                let _ = backend
                    .commands
                    .send(Command::Player(id, Action::Shuffle(!on)));
                continue;
            }

            Command::ToggleMute(id) => {
                let muted = backend
                    .with_entry(id, |e| e.status.as_ref().is_some_and(|s| s.is_muted()))
                    .unwrap_or(false);
                let _ = backend
                    .commands
                    .send(Command::Player(id, Action::Mute(!muted)));
                continue;
            }

            Command::CycleRepeat(id) => {
                let current = backend
                    .with_entry(id, |e| e.status.as_ref().and_then(|s| s.repeat))
                    .flatten()
                    .unwrap_or(2);
                // 0 all -> 1 one -> 2 off -> 0 all. Cycling by value order
                // works out to the order the official controller uses.
                let next = Repeat::from_status((current + 1) % 3);
                let _ = backend
                    .commands
                    .send(Command::Player(id, Action::Repeat(next)));
                continue;
            }

            Command::ToggleGroup(target) => {
                let Some(master) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                if master == target {
                    continue;
                }
                let Some(client) = backend.with_entry(master, |e| e.client.clone()) else {
                    continue;
                };

                // Both calls go to the master: it owns the group, and the
                // slave only learns about it afterwards.
                let joined = backend
                    .with_entry(target, |e| {
                        e.sync.as_ref().and_then(|s| s.master_id()) == Some(master)
                    })
                    .unwrap_or(false);

                tokio::spawn(async move {
                    let result = if joined {
                        client.remove_slave(target).await
                    } else {
                        client.add_slave(target).await
                    };
                    if let Err(e) = result {
                        tracing::warn!(%master, %target, "grouping failed: {e}");
                    }
                });
                continue;
            }

            Command::CustomiseMove(from, to) => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let Pane::Customise(page) = &mut browsing.pane else {
                        continue;
                    };
                    if from >= page.rows.len() || to >= page.rows.len() || from == to {
                        continue;
                    }
                    let row = page.rows.remove(from);
                    page.rows.insert(to, row);
                }
                backend.publish_customise();
                continue;
            }

            Command::CustomiseSave => {
                let saved = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Pane::Customise(page) = &browsing.pane else {
                        continue;
                    };
                    let ids: Vec<String> = page.rows.iter().map(|(id, _)| id.clone()).collect();
                    let mut orders = backend.orders.lock().unwrap();
                    orders.insert(page.screen.clone(), ids);
                    orders.clone()
                };
                // Written on the spot rather than at exit: the app is a
                // long-running window and there is no other moment that
                // reliably arrives.
                order::save(&saved);

                backend.browsing.lock().unwrap().pane = Pane::Browse;
                backend.publish_pane();
                say(&backend.ui, "Home rearranged");
                continue;
            }

            Command::GroupAll => {
                let Some(master) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(master, |e| e.client.clone()) else {
                    continue;
                };
                let others: Vec<DeviceId> = backend
                    .registry
                    .lock()
                    .unwrap()
                    .keys()
                    .copied()
                    .filter(|id| *id != master)
                    .collect();

                if others.is_empty() {
                    say(&backend.ui, "No other players to group");
                    continue;
                }

                tokio::spawn(async move {
                    // One at a time: the master rebuilds the group on each
                    // call, and issuing them together has them race over the
                    // same membership list.
                    for target in others {
                        if let Err(e) = client.add_slave(target).await {
                            tracing::warn!(%master, %target, "group all: {e}");
                        }
                    }
                });
                continue;
            }

            Command::PauseAll => {
                let clients: Vec<_> = backend
                    .registry
                    .lock()
                    .unwrap()
                    .values()
                    .map(|e| e.client.clone())
                    .collect();

                tokio::spawn(async move {
                    for client in clients {
                        if let Err(e) = client.pause().await {
                            tracing::debug!("pause all: {e}");
                        }
                    }
                });
                continue;
            }

            Command::Player(id, action) => (id, action),
        };

        let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
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
                Action::PlayQueueIndex(index) => client.play_queue_index(index).await,
                Action::Volume(level) => client.set_volume(level).await,
                Action::Shuffle(on) => client.set_shuffle(on).await,
                Action::Mute(on) => client.set_mute(on).await,
                Action::Repeat(mode) => client.set_repeat(mode).await,
                Action::Sleep => client.cycle_sleep().await,
            };
            if let Err(e) = result {
                tracing::warn!(%id, ?action, "command failed: {e}");
            }
        });
    }
}

/// Hand a URL to the desktop's browser, and tell the window it happened.
///
/// Worth saying out loud. The browser opens behind the app as often as in
/// front of it, and this app does not change at all when it does — so without
/// a word the press reads as having done nothing, which is exactly the
/// complaint the queue menu used to draw.
async fn open_in_browser(ui: &slint::Weak<AppWindow>, url: String) {
    tracing::info!("opening {url} in a browser");
    match tokio::task::spawn_blocking(move || open::that_detached(url)).await {
        Ok(Ok(())) => say(ui, "Opened in your browser"),
        Ok(Err(e)) => {
            tracing::warn!("could not open a browser: {e}");
            say(ui, "Could not open your browser");
        }
        // The blocking pool itself gave up, which is not something the person
        // reading the screen can do anything about beyond knowing.
        Err(e) => {
            tracing::warn!("browser launch did not finish: {e}");
            say(ui, "Could not open your browser");
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hd_is_shown_as_hr_and_the_rest_are_left_alone() {
        // The player says "hd"; the official controller shows HR, for high
        // resolution, which is what the word means on a hi-fi.
        assert_eq!(quality_label("hd"), "HR");
        assert_eq!(quality_label("HD"), "HR");
        assert_eq!(quality_label("cd"), "CD");
        assert_eq!(quality_label("mqa"), "MQA");
        assert_eq!(quality_label(""), "");
        assert_eq!(quality_label("  "), "");
    }
}
