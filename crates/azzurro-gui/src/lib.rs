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
mod custom;
mod glyphs;
mod known;
mod lane;
#[cfg(target_os = "linux")]
mod mpris;
mod order;
mod searches;

use std::collections::BTreeMap;
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
///
/// A page rather than a ceiling: it used to be both, and everything past the
/// five hundredth track was simply unreachable. Scrolling near the end asks
/// for the next window, the same way a long browse list does.
const QUEUE_WINDOW: u32 = 500;

/// How long the artwork has to stop changing before it is drawn.
///
/// Short enough that a cover still appears to arrive with the track, long
/// enough to swallow the burst of values a player emits while it switches
/// between sources. See [`load_cover`].
const COVER_SETTLE: Duration = Duration::from_millis(180);

/// The cover on a player's row in the sidebar, at twice the 40px it is drawn
/// in.
///
/// Its own tier because it is fetched for *every* player rather than only the
/// one being looked at: in a house with speakers in four rooms the sidebar is
/// a list of what each room is playing, and at this size that is a picture
/// rather than a line of elided text. One of these is 25 KiB against the
/// hero's 2 MiB, so the whole list costs less than a single sleeve.
const CARD_SIZE: u32 = 80;

/// The now-playing sleeve, at twice the largest box it is drawn in so it stays
/// sharp on a HiDPI screen. Fetched once at this size rather than per scale
/// factor.
///
/// The box this is doubled from is the hero's ceiling — `root.height - 278px`,
/// capped by the pane's width — which lands around 560px on an ordinary
/// window. The old comment here described a 180px box, and there had not been
/// one anywhere in the window for some time: at 360 the largest thing in the
/// app was being upscaled 1.1x on a plain screen and 2.2x on a HiDPI one, so
/// the one picture the whole app is arranged around was the softest thing in
/// it.
///
/// The cost is real and is worth stating: 720² × 4 = 2 MiB per entry against
/// 506 KiB before, and the table on [`artwork::MEMORY_CACHE`] carries the
/// arithmetic. Only the hero asks for this tier — the 232px shelf tiles and
/// 72px thumbnails are unchanged — so a cache full of them needs 256 different
/// albums opened at full size.
const COVER_SIZE: u32 = 720;

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
/// own list, and it now outlives the run, so the bound belongs with the file
/// that has to stay a sensible size.
use searches::KEEP as RECENT_SEARCHES;

/// How many players discovery may adopt on its own.
///
/// Not a limit anyone can reach by owning speakers: a large BluOS install is a
/// few dozen zones, and this is an order of magnitude above that. It bounds
/// what a forged announcement can cost, since each adopted player is a
/// permanent poller and another O(n) rebuild of the list the window draws.
/// Adding a player by hand ignores it — that is a decision, not a broadcast.
const MAX_TRACKED: usize = 256;

/// How often to ask a player how its upgrade is going.
///
/// The same interval the official controller uses. Frequent enough that the
/// bar moves, slow enough that a player busy writing its own flash is not
/// answering a request every second while it does it.
const UPGRADE_POLL: Duration = Duration::from_secs(5);

/// How long to keep asking before giving up on watching.
///
/// Covers the whole of it — check, download, install, reboot and coming back —
/// which is what the official controller allows for the same span.
const UPGRADE_PATIENCE: Duration = Duration::from_secs(20 * 60);

/// How long a player may be unreachable mid-upgrade before that is worth
/// saying. It reboots in the middle, so silence is expected for a stretch.
const UPGRADE_SILENCE: Duration = Duration::from_secs(90);

/// How long an upgrade must have been running before an ordinary SyncStatus
/// counts as "finished" rather than "the player has not started yet".
const UPGRADE_SETTLE: Duration = Duration::from_secs(20);

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
    /// Offer to show the update page for a player that has firmware waiting.
    ///
    /// Only ever raised once per player per run — see `told_about_update`.
    UpdateNotice(DeviceId),
    /// The answer to that offer. `true` opens the page.
    UpdateNoticeAnswer(bool),
    /// Ask whether to install the firmware the player says it has, naming it.
    ///
    /// Two steps on purpose. This one only puts the question up; nothing is
    /// sent to the player until [`Command::UpgradeAnswer`] says yes.
    ///
    /// The player is the one whose check offered the update, carried because
    /// the page stays up when another player is chosen and its Install used
    /// to be run on whichever player that was.
    UpgradeAsk(DeviceId),
    /// The answer to that question, which applies to the player it was asked
    /// about: see `Browsing::upgrade_asked`.
    UpgradeAnswer(bool),
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
    /// A letter of the jump bar was pressed or typed.
    BrowseJump(String),
    /// Open the one screen the player puts a search box on.
    OpenSearch,
    /// Go to the very start or the very end of a paged list.
    BrowseEnds(bool),
    /// Enough of the queue has been scrolled that the next window is wanted.
    QueueMore,
    /// Run one of the actions the service offers for what is playing, by the
    /// player's own name for it.
    TrackAction(String),
    /// Open the hand-typed stations.
    OpenStations,
    /// Turn the stations pane's editing mode on or off.
    StationsEdit,
    /// A name and an address, as typed. Both are checked here.
    StationAdd(String, String),
    StationPlay(usize),
    StationRename(usize, String),
    StationRemove(usize),
    StationMove(usize, usize),
    /// Forget every search made this session.
    ClearRecent,
    /// A row on the "add to playlist" list: an existing one, or the line that
    /// makes a new one.
    PlaylistPress(usize),
    /// The name typed into that line.
    PlaylistNamed(String),
    /// Send the add, once somewhere has been settled on, from the opening of
    /// the page it was settled on: see `PlaylistPage::opened`.
    PlaylistAdd(u64, Option<String>, bluos::client::PlaylistTarget),
    /// Answer the "switch input?" question: run it, or forget it.
    ConfirmInput(bool),
    /// Run one line of the context menu open against a queue row.
    QueueMenuAction(usize),
    /// Show what the playing track actually is: format, rate, bit depth.
    NowPlayingInfo,
    /// Open the alarms screen of this player: the one whose settings page it
    /// was chosen on.
    OpenAlarms(DeviceId),
    /// Arm or disarm one from the list, on the player whose list it was read
    /// from. The owner travels with the id because the press is sent back
    /// through the channel, and a pane that changed players in between would
    /// otherwise lend this id to the other player's alarm.
    AlarmArm(DeviceId, u32, bool),
    /// Open the editor on one that already exists.
    AlarmOpen(u32),
    /// Start a new one. `true` makes a schedule — an alarm that runs to a
    /// finishing time rather than for a length.
    AlarmNew {
        schedule: bool,
    },
    /// Change a field of the alarm being edited. Nothing reaches the player.
    AlarmEdit(AlarmField),
    /// Open the source picker at the top of the tree.
    AlarmPick,
    /// Follow or choose the picker's row at this position.
    AlarmPickRow(usize),
    /// Send the working copy to the player.
    AlarmSave,
    /// Delete the one being edited.
    AlarmDelete,
    /// Answer the player's question by pressing one of its buttons. The index
    /// is into the dialog's own list, and out of range means dismissed.
    DialogPress(usize),
    /// Move a section from one place in the Customize list to another.
    CustomiseMove(usize, usize),
    /// Keep the arrangement and go back to the screen it describes.
    CustomiseSave,
    /// Pull every other player into the selected one's group.
    GroupAll,
    /// Stop every player, grouped or not.
    PauseAll,
    /// Read one of this player's web configuration pages and draw it.
    ///
    /// The player is named rather than read from the selection when this is
    /// handled, for the reason on `Step::Deeper`: shares are reached from a
    /// settings page, and that page can be another player's than the one
    /// selected.
    ///
    /// `reload` reads again a page that is already up, after a write from it.
    /// Nobody pressed anything for that, so it lands only on the same page of
    /// the same player's still showing and never opens a browser: a re-read
    /// taking the rule for a press put the list back over wherever the user
    /// had gone in the meantime.
    OpenServices {
        device: DeviceId,
        reload: bool,
    },
    OpenShares {
        device: DeviceId,
        reload: bool,
    },
    /// A row of whichever of those is showing.
    WebAction(usize),
    /// Read a form off one of this player's pages and show it.
    OpenForm {
        device: DeviceId,
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
    /// Bumped every time the trail changes.
    ///
    /// The screens are fetched off the loop, so a reply can land after the
    /// screen it was for has gone: a page of a long list appended itself to
    /// whatever crumb happened to be on top, and an abandoned artwork loader
    /// kept fetching for a screen nobody could see. Anything that leaves the
    /// trail and comes back takes this number with it and checks it still
    /// holds — the same shape as `searches`, which has guarded typing this
    /// way from the start.
    era: u64,
    /// Bumped every time a letter is jumped to.
    ///
    /// `era` is not enough. It marks a change of *screen*, and a jump stays on
    /// the screen it is already on — it replaces the rows inside the crumb
    /// rather than pushing a new one. So a page of the old window, asked for by
    /// a scroll and still in flight, passed the `era` check and appended thirty
    /// rows from wherever the list used to be onto the letter just jumped to.
    /// Two jumps in quick succession had the same problem in reverse: the
    /// slower request landing second put the wrong letter on screen.
    jumps: u64,
    /// Letters worked out from the rows on screen, for a list the player sent
    /// no index for: `(key, every pixel offset that letter starts at)`.
    ///
    /// The player indexes its four long alphabetical lists — Artists, Albums,
    /// Songs, Composers — and nothing else. Genres is 175 entries, sorted, and
    /// gets none; measured on a Powernode running 4.16.22. The difference is
    /// that an indexed list is *paged* and this one is not: the whole thing
    /// arrives in one document, so the letters can be read off the rows and the
    /// jump is a scroll rather than a request.
    ///
    /// Rebuilt whenever the rows are, and from the same rows the window is
    /// given, so the offsets cannot drift from what is drawn.
    derived: Vec<(String, Vec<f32>)>,
    /// The bar letter last pressed, which era it was pressed in, and how far
    /// through that letter's runs the list has been taken.
    ///
    /// A letter can start in more than one place and the bar shows one of it.
    /// Pressing again goes to the next run and wraps at the end, which is how a
    /// folded bar reaches everything it folded. The era is carried so that
    /// leaving the screen and coming back starts from the top again.
    cycling: Option<(u64, String, usize)>,
    /// The screens this player offers: `(label, uri)`, in the order it listed
    /// them. Read once from `/ui/Configuration`.
    screens: Vec<(String, String)>,
    /// Queue Builder Mode: pressing a track adds it to the end of the queue
    /// instead of playing it.
    queue_building: bool,
    /// The player the "Update …?" question is up for, until it is answered.
    ///
    /// Kept for the reason `pending_input` keeps its player: the answer comes
    /// back as a bare yes, and reading the selection then is reading it a
    /// second time, after the question named a player.
    upgrade_asked: Option<DeviceId>,
    /// An input waiting to be confirmed, because switching to it stops
    /// whatever is playing, and the player it is an input of.
    ///
    /// The player is kept with it for the reason `queue_menu_owner` is: the
    /// question stays up while the selection can move, and the action carries
    /// one player's input path, which confirming used to send to whichever
    /// player was selected by then.
    pending_input: Option<(DeviceId, bluos::screen::Action)>,
    /// The question the player asked instead of doing what it was told, and
    /// which player asked it. See [`bluos::dialog`]; `None` whenever there is
    /// nothing to answer.
    ///
    /// The buttons carry that player's own requests — "Replace" is an add of
    /// one of its files — so pressing one runs on the player that asked, not
    /// on whichever is selected when the answer comes.
    dialog: Option<(DeviceId, bluos::dialog::Dialog)>,
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
    ///
    /// Kept with the player it was read from. There is one of these for the
    /// whole window and the queue beside it is always the selected player's,
    /// so a document left from the last selection drew that player's buttons
    /// under this one's tracks, and a press ran them here.
    queue_screen: Option<(DeviceId, Screen)>,
    /// The context menu open against a queue row, if one is. Kept whole
    /// because activating a line means running the action the player attached
    /// to it, and that lives on the parsed document.
    queue_menu: Option<Screen>,
    /// Which player wrote the open menu.
    ///
    /// A browse row's menu is fetched from `device` — the player whose screens
    /// are being read — and its lines were run against `selected`. Those are
    /// normally the same player and briefly are not, so a Favorite carrying
    /// one player's local file path could be sent to another. Recorded with
    /// the menu and checked before anything on it is run.
    queue_menu_owner: Option<DeviceId>,
    /// The queue screen's own uri, from `/ui/Configuration`.
    queue_uri: Option<String>,
    /// Which player `queue_uri` and the addresses beside it were read from.
    ///
    /// They are written when a player's configuration arrives, which is some
    /// time after it is chosen and never for one that cannot answer. Until
    /// then they are the last player's, and a queue document fetched from
    /// them could be the wrong route entirely.
    configured: Option<DeviceId>,
    /// The search screen's own uri, from the same place.
    ///
    /// The player puts a search box on exactly one screen — measured on a
    /// Powernode running 4.16.22: not on Artists, Albums, Songs, Genres,
    /// Composers, Playlists, Folders or Favorites, and not on the library's
    /// front page either. So a key that means "search" cannot focus a box on
    /// the screen you are looking at; it has to be able to fetch the one screen
    /// that has one.
    search_uri: Option<String>,
    /// Whether a page of a long list is already on its way, so that scrolling
    /// past the trigger a second time does not ask for it twice.
    fetching_more: bool,
    /// The same, for the queue.
    ///
    /// Its own flag rather than sharing `fetching_more`: the browse list and
    /// the play queue are two lists on screen at once, and one of them paging
    /// must not stop the other from doing it.
    fetching_queue: bool,
    /// What has been searched for, most recent first.
    ///
    /// The player does not keep this — the official controller keeps its own,
    /// which is why the list is empty on a player you have used for years.
    /// Recorded when a search is committed rather than on every keystroke, or
    /// every prefix of every word would be on it.
    ///
    /// Seeded from `searches` at startup and written back on every change, so
    /// a name looked up yesterday does not have to be typed again today.
    recent: Vec<String>,
    /// What the middle pane is showing.
    ///
    /// One field rather than four flags, because it is one fact. As four they
    /// could all be true at once, and a stale one routed a press to a pane that
    /// was not on screen: with the services list left set, every row of the
    /// settings page opened a music service's sign-in page instead.
    pane: Pane,
    /// Which sidebar entry is lit, as `(kind, index)`, and whose sidebar it
    /// was lit on. Recorded on activation rather than inferred, because a
    /// screen can be reached several ways.
    ///
    /// The index is a position in one player's list, and the next player's
    /// list is a different one: kept bare, Favourites lit on one player lit
    /// Search on a player without Favourites, and an input's position lit
    /// whichever service sat there. Read it through [`Browsing::lit`], which
    /// draws it only on the sidebar it belongs to.
    ///
    /// One entry per player rather than one for whoever pressed last. A press
    /// on a player whose screens never came — its Home and its Search both
    /// refused — took the only slot from the player whose screens were still
    /// up, and choosing that player back kept its trail with nothing lit.
    ///
    /// Each entry carries the value of `configurations` when it was lit. Write
    /// it through [`Browsing::light`].
    highlighted: BTreeMap<DeviceId, ((i32, i32), u64)>,
    /// How many times any player's configuration has been asked for.
    ///
    /// A configuration landing takes an entry of its player's off the sidebar
    /// when it is left from before, and must keep one for a press made while
    /// it was on its way. Who is configured cannot tell the two apart — a
    /// player chosen away and back is still the one configured, and so is one
    /// whose screens all failed and was pressed again — but when the entry
    /// was lit can: lit before this configuration was asked for, it is from
    /// before.
    configurations: u64,
}

impl Pane {
    /// The settings page on show, if that is what is, and the player it
    /// belongs to — one question, so a caller cannot read the page and then
    /// act on somebody else.
    fn settings_owned(&self) -> Option<(DeviceId, &SettingsPage)> {
        match self {
            Pane::Settings(owner, trail) => trail.last().map(|page| (*owner, page)),
            _ => None,
        }
    }

    /// The web configuration page on show, if that is what is.
    fn web(&self) -> Option<&WebPage> {
        match self {
            Pane::Web(_, page) => Some(page),
            _ => None,
        }
    }

    /// The player a page read from one player belongs to, for the panes a
    /// further page of that player's is opened from: its settings, its web
    /// pages and their forms, and its alarms.
    ///
    /// Those panes stay up when another player is chosen, so a page opened
    /// from one of them lands on it by this, not by the selection. See
    /// [`Backend::may_open_for`].
    fn owner(&self) -> Option<DeviceId> {
        match self {
            Pane::Settings(owner, _) | Pane::Web(owner, _) => Some(*owner),
            Pane::Form(page) => Some(page.device),
            Pane::Alarms(page) => Some(page.device),
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
    ///
    /// With the player the trail was read from. The pane stays up when the
    /// sidebar moves, so a press that resolved its target from `selected`
    /// wrote one player's setting — its URL and its name — to another; see
    /// `AlarmsPage::device`, which is the same fix.
    Settings(DeviceId, Vec<SettingsPage>),
    Help,
    /// A page of plain facts: its title, what is on it, where Back goes, and
    /// the player it offers to install an update on, if it does.
    ///
    /// The offer is part of the page so that it goes when the page does. Kept
    /// beside it, it outlived Back and was drawn on the next player's
    /// Diagnostics; and with no player to it, Install ran on whichever player
    /// was selected by the time it was pressed.
    HelpDetail(String, Vec<(String, String)>, Whence, Option<DeviceId>),
    /// One of the player's web configuration pages, drawn rather than opened,
    /// and the player it was read from.
    ///
    /// Held for the reason `Settings` holds its player: removing a share posts
    /// that player's own field name, and the page stays up when the sidebar
    /// moves.
    Web(DeviceId, WebPage),
    /// A form off one of those pages, filled in here rather than in a browser.
    Form(Box<FormPage>),
    /// The record, large. Reached by pressing the artwork on the transport bar,
    /// which is where the official controller puts it too.
    NowPlaying,
    /// The player's alarms, and the one being edited if any.
    Alarms(Box<AlarmsPage>),
    /// Rearranging the sections of a screen — "Customize Home".
    Customise(CustomisePage),
    /// Choosing where to file a track.
    Playlists(Box<PlaylistPage>),
    /// Radio stations typed in by hand, which the player knows nothing about.
    Stations(Box<StationsPage>),
    /// Renaming one of the player's presets.
    EditPreset(Box<EditPresetPage>),
}

/// A preset being renamed.
///
/// The url and the image are carried rather than shown: `/SetPreset` replaces
/// the whole slot, so a field left out is a field cleared, and they have to go
/// back exactly as they came. Only the name is anybody's business to change —
/// the stream came out of the player's own catalog, and retyping it by hand
/// would sever the preset from the thing it names.
struct EditPresetPage {
    /// Whose preset this is.
    ///
    /// `/SetPreset` replaces the whole slot, so sending it to a player other
    /// than the one the menu came from does not rename anything — it
    /// overwrites that player's preset in the same slot with this one's
    /// station. Held for the reason `AlarmsPage::device` is.
    device: DeviceId,
    slot: u32,
    preset: bluos::screen::Preset,
}

/// The stations kept on this machine, as a page.
#[derive(Default)]
struct StationsPage {
    stations: Vec<custom::Station>,
    /// Whether the rows are being edited rather than played.
    ///
    /// A mode rather than controls always on show. Reordering needs the whole
    /// row to be a handle and playing needs the whole row to be a target, and
    /// one row cannot be both at once; and a removal cannot be undone, so
    /// putting the bin behind Edit is the difference between deleting a
    /// station and meaning to.
    editing: bool,
}

/// Somewhere to put a track, being chosen.
struct PlaylistPage {
    /// The player the options were read from. They name the track in that
    /// player's terms, so the add goes back to it rather than to whichever
    /// player is selected when a playlist is picked.
    device: DeviceId,
    /// Which opening of the page this is, from `Backend::openings`.
    ///
    /// A press is sent back through the channel before the add is made, and
    /// the add answers a round trip after that. Either can find another page
    /// up by then — another player's playlists, or Settings — and the device
    /// alone does not tell this player's page from its next one.
    opened: u64,
    title: String,
    options: bluos::playlists::AddToPlaylist,
    /// Whether the row for a new playlist is open for typing. Closed, it is a
    /// line to press; open, it is a field — the same two states the queue's
    /// Save button has, and for the same reason.
    naming: bool,
}

/// A screen being rearranged.
/// The alarms screen: the list as the player last gave it, and the alarm being
/// edited when one is open.
///
/// `editing` holds a whole [`bluos::alarms::Alarm`] rather than a handful of
/// fields, so the editor is a working copy: nothing reaches the player until
/// Save, and Back throws the copy away. A new alarm is the same thing with
/// `id` still zero, which is also what tells `save_alarm` to create rather
/// than replace.
#[derive(Debug, Clone)]
struct AlarmsPage {
    /// Whose alarms these are.
    ///
    /// Held rather than read from the selection at press time: the pane stays
    /// up when the sidebar moves, so a save that resolved its target from
    /// `selected` wrote one player's alarm onto another — and Delete did the
    /// same. The queue menu already keeps its owner for this reason; see
    /// `queue_menu_owner`.
    device: DeviceId,
    list: bluos::alarms::Alarms,
    editing: Option<bluos::alarms::Alarm>,
    /// Which opening of the editor this is, from `Backend::openings`. A
    /// picker level asked for with no level open joins only this one; see
    /// [`AlarmsPage::top`]. A save or a delete closes only the opening it was
    /// pressed on for the same reason: numbers are never handed out twice, so
    /// an editor opened while the write was out is never mistaken for it.
    opened: u64,
    /// The source picker, while it is open over the editor. A trail rather
    /// than one level, so Back walks out the way it walked in — the tree is
    /// several deep on a player with services on it.
    picking: Vec<PickerLevel>,
}

/// One level of the source tree, and what it is called.
#[derive(Debug, Clone)]
struct PickerLevel {
    title: String,
    rows: bluos::stations::Stations,
    /// Which opening of a level this is, from `Backend::openings`.
    opened: u64,
}

impl AlarmsPage {
    /// What a level asked for now would be put on top of: the level open, or
    /// the editor when none is.
    ///
    /// Numbers are never handed out twice, so a level that was closed, or an
    /// editor that was, is never the top again. How deep the picker is could
    /// not say that: Back and Plays again put a new top level at the same
    /// depth, and a folder pressed on the old one landed over it.
    fn top(&self) -> u64 {
        self.picking
            .last()
            .map_or(self.opened, |level| level.opened)
    }
}

struct CustomisePage {
    /// Whose order is being edited, which is the key the preference is filed
    /// under.
    screen: String,
    title: String,
    /// The movable sections, `(id, title)`, in the order they would be saved
    /// in. Sections the player pinned are not here at all: they cannot move,
    /// so offering to move them would be a lie.
    rows: Vec<(String, String)>,
    /// Set when what is being dragged is the player's presets rather than this
    /// app's own idea of a screen's order, and carrying whose presets they are
    /// and the list's `prid`.
    ///
    /// The same page for both because it is the same gesture and the same
    /// rows; only where the answer goes differs — a file here, a request to
    /// the player there. The ids in `rows` are slot numbers in that case.
    ///
    /// The player is kept because the order is a permutation of its slots:
    /// sent to another player, it shuffles that player's presets by numbers
    /// read off a different list, and any slot the other list lacks is
    /// deleted.
    presets: Option<(DeviceId, u32)>,
}

/// Where Back goes from a page that can be reached from more than one place.
///
/// Technical info is the case that needs it: the Help menu leads to pages
/// shaped exactly like it, and so does a track's context menu, and so does the
/// format badge on the record. Landing all three back on Help was right for
/// one of them and baffling for the other two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Whence {
    Help,
    Browse,
    NowPlaying,
}

/// A form being filled in.
struct FormPage {
    /// Whose form this is.
    ///
    /// A form is submitted to the player that served it, not to whichever is
    /// selected when the button is pressed. The WiFi form carries a network
    /// key and a service's carries a password, and the pane stays up when the
    /// sidebar moves: sent to the other player, a join could take that one
    /// off the network. Held for the reason `AlarmsPage::device` is.
    device: DeviceId,
    /// Which opening of a form this is, from `Backend::forms`.
    ///
    /// The player is not enough to tell one form from another. Open WiFi, go
    /// Back while it scans, open a service's sign-in, and both placeholders
    /// are that player's: the scan landing second replaced the sign-in with
    /// the WiFi form, and a scan that failed closed it and opened a browser.
    /// A reply is for the form it was asked for, and this is how it knows it.
    /// The steps of one form keep the number, so an answer to a submit lands
    /// on the form that was submitted.
    opened: u64,
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
    ///
    /// A settings page too: the WiFi form is opened from Player settings, and
    /// Back out of its placeholder landed on Home, where the row under the
    /// pointer that had been WiFi was a Recently Played tile that plays. Only
    /// ever `Pane::Web` or `Pane::Settings`, of the form's own player.
    from: Option<Box<Pane>>,
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
    /// How many rows the player sends at a time, as the first page showed.
    ///
    /// The rows held stop saying so once a second page has been grafted on,
    /// and the End key needs the page and not the pile: ninety held of 2062
    /// aimed the last window at 1972 and showed rows that end thirty short.
    page: u32,
}

impl Browsing {
    /// Note that the trail is no longer what it was.
    fn moved_on(&mut self) {
        self.era = self.era.wrapping_add(1);
    }

    /// The sidebar entry to light, if the one recorded was lit on the sidebar
    /// showing.
    ///
    /// Not cleared when another player's configuration lands, only not drawn:
    /// that player's Home may never come, and choosing the last player back
    /// keeps its trail, which the entry it had lit still describes.
    fn lit(&self) -> Option<(i32, i32)> {
        self.configured
            .and_then(|owner| self.highlighted.get(&owner))
            .map(|(entry, _)| *entry)
    }

    /// Light an entry on `id`'s sidebar, noting how many configurations had
    /// been asked for by then; see `configurations`.
    fn light(&mut self, id: DeviceId, entry: (i32, i32)) {
        let at = self.configurations;
        self.highlighted.insert(id, (entry, at));
    }
}

/// How a settings page joins the pane's own trail.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    /// Start again here: opening Settings, or arriving from a Customize button
    /// on a screen somewhere else. Always the selected player's.
    Root,
    /// Opened from the page showing, which Back undoes.
    ///
    /// This and `Reload` name the player whose trail they belong to rather
    /// than reading the selection when they are handled: the pane stays up
    /// when another player is chosen, and a page of one player's settings
    /// pushed onto another's trail would have its rows written to the wrong
    /// speaker.
    Deeper(DeviceId),
    /// The same page again. A write can change more than the value written —
    /// turning tone controls on brings treble and bass to life — so the page is
    /// re-read rather than assumed.
    Reload(DeviceId),
}

/// How a newly fetched screen joins the trail.
#[derive(Debug, Clone, PartialEq)]
enum Arrive {
    /// A top-level screen off the sidebar: everything before it is gone.
    Root,
    /// A player's first screen, arriving when it is chosen: `Root`, and the
    /// sidebar lights its first entry.
    ///
    /// Lit as it lands, with the trail, and not when the configuration does.
    /// Between those the screens showing are still the last player's, and
    /// choosing that one back before this Home arrived dropped the Home and
    /// kept its trail — Search, say — under a sidebar pointing at Home. What
    /// is lit is recorded rather than inferred, so the only way to keep it
    /// true is to write it at the moment the screen it describes goes up.
    ///
    /// Unless the user got there first. The sidebar takes presses as soon as
    /// the configuration is in, which on a waking player is seconds before
    /// Home: Favourites pressed in between stayed on screen under a lit Home.
    /// So a Home that finds something of this player's already lit leaves it
    /// lit, and one that finds this player's screens already up — the press
    /// answered before it did — is not shown at all.
    Home,
    /// A step deeper, which Back undoes.
    Deeper,
    /// A set of search results: pushed the first time, and thereafter in place
    /// of the last set.
    Found(String),
    /// In place of the screen it came from, which is what a service picker
    /// marked `replaceScreen` asks for.
    Replace(Option<String>),
    /// In place of one particular screen, if that is still the one on top.
    ///
    /// A refresh reads the crumb, waits for the player, and comes back to a
    /// trail the user may have moved in the meantime. `Replace` pops whatever
    /// it finds, so a refresh that lost that race deleted the level the user
    /// had just opened and put the old one back in its place.
    Refresh { query: Option<String>, was: String },
}

impl Browsing {
    fn current(&self) -> Option<&Screen> {
        self.trail.last().map(|c| &c.screen)
    }

    /// Which of a letter's runs this press means, advancing on a repeat.
    ///
    /// `None` when the letter is on no list, which is the answer for a
    /// keystroke that is not a letter of this bar at all.
    fn next_stop(&mut self, key: &str, stops: usize) -> Option<usize> {
        if stops == 0 {
            return None;
        }
        let at = match &self.cycling {
            Some((era, last, at)) if *era == self.era && last == key => (at + 1) % stops,
            _ => 0,
        };
        self.cycling = Some((self.era, key.to_owned(), at));
        Some(at)
    }
}

/// Deliberately coarse: a caller names an intent, and the backend owns every
/// decision about how to reach the player.
///
/// `Play`, `Pause` and `Stop` are only ever constructed by the MPRIS bridge —
/// the window's own transport button sends `Toggle`, because it is one button
/// — so off Linux, where that bridge is not built, nothing makes them. The
/// arms that handle them stay either way: they cost nothing, and deleting a
/// third of an intent enum to please a lint on one platform would be the tail
/// wagging the dog.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
    /// Whether this player is rewriting its own firmware.
    ///
    /// While it is, it answers nothing useful and will reboot; the window
    /// takes its transport and volume away rather than offering controls that
    /// quietly do nothing. Cleared when it comes back.
    upgrading: bool,
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
    /// The same art at [`CARD_SIZE`], for this player's row in the sidebar.
    ///
    /// Held decoded rather than fetched at publish time: the player list is
    /// rebuilt on every status tick, and a fetch on that path would be a
    /// request per player per second.
    card_cover: Option<Pixels>,
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
///
/// `orders` is the one exception, and it is not one anybody chose:
/// [`Backend::arrangement`] reads it, and every caller of that is already
/// holding `browsing` because the screen being arranged lives there. So the
/// rule for that pair is an order rather than a ban — **`browsing` first,
/// `orders` second, never the reverse.** `orders` is a leaf: nothing is taken
/// while it is held, which is what keeps the cycle from closing. Anything new
/// that wants both must take them in that order.
#[derive(Clone)]
struct Backend {
    registry: Registry,
    /// Whose queue the window is showing.
    selected: Arc<Mutex<Option<DeviceId>>>,
    /// Bumped, under `selected`'s lock, every time `selected` changes.
    ///
    /// A screen is fetched off the loop, and the player it was fetched from
    /// can stop being the selected one while the reply is on its way. Choosing
    /// a player that was slow to answer and then another that was not let the
    /// slow reply land second: the pane showed the first player's Home, and
    /// took `Browsing::device` with it, while the sidebar, queue and transport
    /// all showed the second — so a row pressed there played on a player that
    /// was not the one selected. Checking the id when the reply lands is not
    /// enough on its own, because choosing away and back again passes it.
    ///
    /// So a request takes this number with it, read with the id under the
    /// same lock by [`Backend::selection`], and a reply that finds it moved is
    /// dropped. It is `browsing.era`'s idea one level up: the era says the
    /// trail changed, this says the player it belongs to did. An atomic rather
    /// than a field of either mutex, so the check can be made with `browsing`
    /// held without nesting two locks.
    selections: Arc<AtomicU64>,
    commands: mpsc::UnboundedSender<Command>,
    ui: slint::Weak<AppWindow>,
    artwork: Arc<Artwork>,
    browsing: Arc<Mutex<Browsing>>,
    /// Addresses that have answered, remembered between runs. Oldest first
    /// and bounded; see [`known`].
    known: Arc<Mutex<Vec<DeviceId>>>,
    /// The line the requests that must keep their order stand in. See
    /// [`lane`]; the arms that use it say why they have to.
    writes: Arc<lane::Lane>,
    /// How many searches have been asked for. Typing asks for one per
    /// keystroke, so each takes a number and checks it is still the highest
    /// before the request goes out — which collapses a typed word into one
    /// search rather than eight.
    ///
    /// Before, and only before. Two searches that both get past that check
    /// still race, and the slower one lands last and takes the screen with the
    /// box reading something else. Correcting that means carrying the number
    /// through the fetch and refusing to apply a stale reply, which `open_screen`
    /// has no way to express today; the next keystroke fixes it, so it has been
    /// left. This comment used to claim the check happened on the way out.
    searches: Arc<AtomicU64>,
    /// Forms opened so far, numbering each one as its placeholder goes up. See
    /// `FormPage::opened`.
    forms: Arc<AtomicU64>,
    /// The same for the other pages a late reply has to find again: alarm
    /// editors, picker levels and the playlist chooser. See
    /// `PlaylistPage::opened` and [`AlarmsPage::top`].
    openings: Arc<AtomicU64>,
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
    /// And the settings rows, for the same reason as the three above: the
    /// status ticks every second and rebuilding a settings model replaces
    /// every row instance, which throws away whatever was being typed into
    /// one and jumps the scroll on a page of mixed heights.
    sent_settings: Arc<AtomicU64>,
    /// And the browse model, which is the largest of them.
    ///
    /// It was the only one published with no memo at all, so every republish
    /// replaced every row — the cost the four above exist to avoid, on the
    /// model with the most rows in it. A screen is republished whenever a
    /// thumbnail lands, on every page of a paged screen, and on every refresh.
    sent_browse: Arc<AtomicU64>,
    /// The letters of the jump bar, so the model is not replaced on every
    /// republish of the same screen.
    sent_index: Arc<AtomicU64>,
    /// Which screen the window was last shown, so a new one can start at the
    /// top. See the note where it is read.
    sent_era: Arc<AtomicU64>,
    /// Whether a stale-screen refresh is already in flight.
    ///
    /// Staleness is decided by comparing the screen's `refreshOnStatusChange`
    /// pairs against the live status, and the status ticks once a second. A
    /// pair the status cannot satisfy — a key `Status::field` does not model,
    /// so the comparison reads `None` forever — is therefore not a screen that
    /// refreshes once, it is a screen that refetches every second for as long
    /// as it is open. One refresh at a time bounds that, and any future
    /// staleness rule that gets it wrong, to a single round trip.
    refreshing: Arc<std::sync::atomic::AtomicBool>,
    /// Players already mentioned as having an update waiting.
    ///
    /// One offer per player per run. Not written down between runs: what
    /// would be remembered is "this firmware was declined", and the check
    /// answers with no version to key that on — so the choice is between
    /// asking once a run and never asking again after the first no. Once a
    /// run is the honest one, and it is what makes dismissing cheap.
    told_about_update: Arc<Mutex<std::collections::HashSet<DeviceId>>>,
    /// Which player the offer on screen is about.
    update_offer: Arc<Mutex<Option<DeviceId>>>,
    /// Section order per screen, as set by Customize Home and kept on disk.
    orders: Arc<Mutex<order::Orders>>,
}

/// The player's own words, in American spelling.
///
/// BluOS writes British: "Favourites", "Customise Home", "Added to
/// favourites". Those are chrome — the app's furniture, which happens to be
/// authored by the speaker — and they read oddly beside this app's own text.
///
/// **Only ever call this on chrome.** Screen and section titles, menu labels,
/// button captions. Never on an item's title, subtitle or artist: those are
/// content, and a pass over them renames *Favourite Worst Nightmare*. The
/// boundary is the whole reason this is a function called at a handful of
/// sites rather than a filter over everything on its way to the window — and
/// why the list below is three words rather than a dictionary.
///
/// Whole words only, and case preserved on the first letter, so "Favourites"
/// and "favourites" both come out right and "Colourbox" is left alone.
fn american(text: &str) -> String {
    // Three words, and only because the player was seen writing all three:
    // "Favourites" and "My Favourites" as screen and section titles,
    // "Favourite" and "Remove favourite" in context menus, "Customise Home"
    // and "Customise" on buttons. Nothing else British has turned up in its
    // chrome.
    //
    // The list stays this short on purpose. A screen's title is chrome when it
    // says "Favourites" and content when it is an album's name, and there is
    // nothing in the document that tells them apart — so the only protection
    // against renaming a record is that the words being replaced are ones no
    // record is likely to be called. "Colour" and "Centre" were in an earlier
    // draft and came out again for exactly that reason.
    const PAIRS: &[(&str, &str)] = &[
        ("favourite", "favorite"),
        ("favourites", "favorites"),
        ("customise", "customize"),
    ];

    // Split on word boundaries so a substring cannot be caught: `is_alphabetic`
    // rather than whitespace, since "Add to playlist…" and "Favourites," both
    // end a word on punctuation.
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();

    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        let lower = word.to_lowercase();
        match PAIRS.iter().find(|(from, _)| *from == lower) {
            Some((_, to)) => {
                // Match the case of what was written. Three shapes and no
                // more, because that is all these labels use: a section
                // heading is upper — "FAVOURITES" — a title is capitalized,
                // and a word inside a sentence is lower.
                let upper = word.chars().filter(|c| c.is_alphabetic()).count() > 1
                    && word
                        .chars()
                        .filter(|c| c.is_alphabetic())
                        .all(char::is_uppercase);
                if upper {
                    out.extend(to.chars().flat_map(char::to_uppercase));
                } else if word.chars().next().is_some_and(char::is_uppercase) {
                    let mut chars = to.chars();
                    if let Some(first) = chars.next() {
                        out.extend(first.to_uppercase());
                        out.push_str(chars.as_str());
                    }
                } else {
                    out.push_str(to);
                }
            }
            None => out.push_str(word),
        }
        word.clear();
    };

    for c in text.chars() {
        if c.is_alphabetic() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// What to print on a format badge.
///
/// The player says `hd` for anything above CD, and the official controller
/// shows that as HR — high resolution — which is what the word means on a
/// hi-fi. Everything else is already the abbreviation it should be: `cd`,
/// `mqa`.
/// One thing the service offers for what is playing, on its way to the window.
#[derive(Debug, PartialEq, Eq)]
struct TrackActionData {
    name: String,
    label: String,
    on: bool,
}

/// Which of a status's actions belong on the Now Playing panel.
///
/// Availability is `url.is_some()` rather than [`Status::can`]. That helper
/// falls back to "anything not a stream can do it", which is right for skip
/// and back and wrong for these: a local file is queue-based, so the fallback
/// would put a Love button on a track no service will ever hear about. An
/// action listed *without* a URL is the player declaring it unavailable, and
/// that is the whole signal here.
///
/// Skip and back are the transport's, and the interval variants of them are
/// the fifteen-second nudges a podcast wants — transport territory too, and no
/// document showing their shape has been seen here, so they are left out
/// rather than guessed at.
fn track_actions(status: &bluos::Status) -> Vec<TrackActionData> {
    status
        .actions()
        .iter()
        .filter(|action| action.url.is_some())
        .filter(|action| action.interval.is_none())
        .filter(|action| !matches!(action.name.as_str(), "skip" | "back"))
        .map(|action| TrackActionData {
            name: action.name.clone(),
            label: action_label(&action.name),
            // Whether a toggle is set — the player says whether this track is
            // already loved. Note a real Powernode sends `state` on skip and
            // back as well, so this is not by itself a sign that an action is
            // a toggle; those two are filtered out above regardless.
            on: action.state.unwrap_or(0) != 0,
        })
        .collect()
}

/// A word for an action the player names.
///
/// The player sends identifiers, not labels — `love`, `ban`, `shop` — and the
/// official controller draws each as its own glyph. Words here instead: the
/// icon set this app uses has no heart, no thumb and no cart, and inventing
/// three that merely resemble the ones it does have would be worse than saying
/// what the button does.
///
/// Anything unrecognized is title-cased and shown regardless. The list is
/// whatever the service offers, so a name nobody here has met is a button that
/// still works rather than one that vanishes.
fn action_label(name: &str) -> String {
    match name {
        "love" => "Love".to_owned(),
        "ban" => "Ban".to_owned(),
        "shop" => "Shop".to_owned(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

fn quality_label(raw: &str) -> String {
    let raw = raw.trim();
    match raw.to_ascii_lowercase().as_str() {
        "" => String::new(),
        "hd" => "HR".to_owned(),
        _ => kilobits(raw).unwrap_or_else(|| raw.to_ascii_uppercase()),
    }
}

/// A bitrate in this field, rendered the way anyone writes one.
///
/// The field carries two different kinds of value. A library track gets a
/// tier — `cd`, `hd`, `mqa` — and an HTTP stream gets its bitrate in bits per
/// second instead, so the badge that reads CD on a file read `128000` on a
/// station.
///
/// Kilobits are a thousand bits and not 1024 of them: the player's own words
/// for the stream that produced `128000` are `MP3 128 kb/s`. Dividing by 1024
/// would render `125k`, which is wrong in a way nobody would ever catch.
///
/// `None` for anything that is not a plain number large enough to be a
/// bitrate, so a value neither this nor the player has seen before falls
/// through to being shown as it arrived rather than as nonsense.
fn kilobits(raw: &str) -> Option<String> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let bits: u64 = raw.parse().ok()?;
    (bits >= 1000).then(|| format!("{}k", bits / 1000))
}

/// Whether this is the same thing the window already has.
///
/// Hashed rather than compared, because the rows carry decoded artwork and
/// comparing that is more expensive than the redraw it would save. The
/// artwork goes in only as whether it has arrived, which is the only thing
/// about it that can change once it has.
///
/// Every field that is drawn has to go in, and that is an obligation on each
/// caller rather than something this can enforce: a field the window draws but
/// the fingerprint omits is a cell that silently stops redrawing when it
/// changes. Adding a field to a row means adding it here too. `Device.model`
/// is the known exception — drawn, deliberately not hashed, because it is read
/// once when a player is adopted and never changes after.
fn already_sent(seen: &AtomicU64, fingerprint: u64) -> bool {
    // Zero is the "nothing sent yet" value, so a fingerprint that happens to
    // be zero simply sends once more than it needs to.
    fingerprint != 0 && seen.swap(fingerprint, Ordering::Relaxed) == fingerprint
}

/// Whether a row is the app's own furniture rather than something on the system.
///
/// [`american`] is kept off item titles on purpose: a record really can be
/// called "Colours" and a band "The Organisers", and rewriting those would be
/// putting words in the player's mouth. These kinds are not the name of
/// anything — they are the affordances the player draws at the end of a
/// screen, and "Customise Home" is chrome in exactly the way the sidebar is.
/// It arrives as an `<item>` only because that is how the document carries it.
fn is_chrome_row(kind: ItemKind) -> bool {
    matches!(kind, ItemKind::Customise | ItemKind::Footer)
}

/// Fetch one level of the source tree and push it onto the picker's trail.
///
/// `owner` is the player whose alarm is being edited, and `top` what
/// [`AlarmsPage::top`] said when this level was asked for. The level joins
/// only that page with that same top. A slow folder answered after Back closed
/// the picker used to open it again, and one answered after the pane had gone
/// to another player's alarms put this player's sources into that player's
/// alarm.
async fn open_picker(
    backend: &Backend,
    client: &Client,
    owner: DeviceId,
    top: u64,
    title: String,
    path: String,
) {
    let listed = client.stations(&path).await;
    let opened = backend.openings.fetch_add(1, Ordering::Relaxed) + 1;
    {
        let mut browsing = backend.browsing.lock().unwrap();
        let Pane::Alarms(page) = &mut browsing.pane else {
            return;
        };
        if page.device != owner || page.editing.is_none() || page.top() != top {
            return;
        }
        // Said only while the picker it was asked from is still up: a list
        // nobody is waiting for any more has not failed anybody.
        match listed {
            Ok(rows) => page.picking.push(PickerLevel {
                title,
                rows,
                opened,
            }),
            Err(e) => {
                drop(browsing);
                say(&backend.ui, format!("could not read that list: {e}"));
                return;
            }
        }
    }
    backend.publish_pane();
}

/// Watch an upgrade through to the end, and tell the window what it is doing.
///
/// Polled bare, every few seconds, rather than through the long poll the rest
/// of the app uses. Two reasons, both learned from the official controller:
/// the etag poll holds a connection open for up to a hundred seconds and the
/// player stops answering partway through to reboot, so a held poll waits for
/// a reply that is never coming; and `/SyncStatus` answers with a different
/// root element while upgrading, which the ordinary reader treats as a
/// malformed player.
///
/// Gives up after [`UPGRADE_PATIENCE`]. That is not a failure of the upgrade —
/// nothing here can stop one — only of this app's willingness to keep asking.
async fn follow_upgrade(backend: Backend, id: DeviceId) {
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return;
    };

    let started = std::time::Instant::now();
    // A player mid-reboot refuses connections for a while; that is the normal
    // middle of an upgrade, not a reason to stop watching.
    let mut quiet_since: Option<std::time::Instant> = None;

    loop {
        if started.elapsed() > UPGRADE_PATIENCE {
            backend.clear_upgrade();
            say(
                &backend.ui,
                "Still updating after twenty minutes — check the player itself",
            );
            return;
        }

        match client.sync_or_upgrade().await {
            Ok(bluos::client::Sync::Upgrading(progress)) => {
                quiet_since = None;
                backend.show_upgrade(id, &progress);
                if progress.error {
                    backend.clear_upgrade();
                    say(&backend.ui, "The update failed — the player will say more");
                    return;
                }
            }
            // Back to answering as itself: the upgrade is over and the player
            // has rebooted into whatever it now runs.
            Ok(bluos::client::Sync::Status(_)) => {
                if quiet_since.is_some() || started.elapsed() > UPGRADE_SETTLE {
                    backend.clear_upgrade();
                    say(&backend.ui, "Update finished");
                    return;
                }
            }
            // Unreachable, or answering with its bootloader's idea of HTML.
            // Expected in the middle; only worth reporting if it lasts.
            Err(_) => {
                let since = *quiet_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > UPGRADE_SILENCE {
                    backend.clear_upgrade();
                    say(
                        &backend.ui,
                        "Lost the player while it was updating — it may still be working",
                    );
                    return;
                }
            }
        }

        tokio::time::sleep(UPGRADE_POLL).await;
    }
}

/// What a press on the alarms screen means.
///
/// The screen borrows the settings pane's rows, so its presses arrive as
/// settings presses. Which alarm command they stand for depends on the row's
/// index and on whether the editor is open — the list numbers its rows by
/// position, the editor by the fixed constants above. The two are kept apart
/// by [`ALARM_ROW_BASE`], which sits far above any list a player can serve;
/// `publish_alarms` refuses to publish a row that would reach it, so a
/// position can never be read as an editor row.
fn alarm_command(backend: &Backend, index: usize, edit: Option<Edit>) -> Option<Command> {
    let browsing = backend.browsing.lock().unwrap();
    let Pane::Alarms(page) = &browsing.pane else {
        return None;
    };
    // The picker owns the presses while it is open, and its rows are
    // positional like the list's.
    if !page.picking.is_empty() {
        return Some(Command::AlarmPickRow(index));
    }

    let Some(alarm) = page.editing.as_ref() else {
        // The list. A toggle arms the alarm at that position; anything else
        // opens it, and the last row makes a new one.
        if index == NEW_ALARM {
            return Some(Command::AlarmNew { schedule: false });
        }
        if index == NEW_SCHEDULE {
            return Some(Command::AlarmNew { schedule: true });
        }
        let found = page.list.alarms.get(index)?;
        return Some(match edit {
            Some(Edit::Toggle) => Command::AlarmArm(page.device, found.id, !found.enabled),
            _ => Command::AlarmOpen(found.id),
        });
    };

    // The editor.
    let field = match (index, edit) {
        (ALARM_SOURCE, _) => return Some(Command::AlarmPick),
        (ALARM_SAVE, _) => return Some(Command::AlarmSave),
        (ALARM_DELETE, _) => return Some(Command::AlarmDelete),
        (ALARM_VOLUME, Some(Edit::Number(v))) => AlarmField::Volume(v.max(0.0) as u32),
        (ALARM_FADE, Some(Edit::Toggle)) => AlarmField::FadeIn(!alarm.fade_in),
        (ALARM_SHUFFLE, Some(Edit::Toggle)) => AlarmField::Shuffle(!alarm.shuffle),
        (ALARM_BACKUP, Some(Edit::Toggle)) => AlarmField::Backup(!alarm.use_backup),
        // The last option on the Stops chooser is "At a set time"; the rest
        // are the durations, in the order they were published.
        (ALARM_KIND, Some(Edit::Choose(n))) => match DURATIONS.get(n) {
            Some(minutes) => AlarmField::Duration(*minutes),
            None => AlarmField::Schedule(true),
        },
        _ => return None,
    };
    Some(Command::AlarmEdit(field))
}

/// Whether pressing this is a switch of input.
///
/// An input is the one thing on a screen that stops the music: a service opens
/// a screen, a track replaces the queue and says so itself, but an input takes
/// the speaker away from whatever it was doing with no way back to where it
/// was. It is a `player-link` whose command plays directly rather than through
/// the player's own `/ui/prf` wrapper — which is how a station is written, and
/// a station is not an input.
fn switches_input(action: &bluos::screen::Action) -> bool {
    action.kind == bluos::screen::ActionKind::PlayerLink
        && action
            .uri
            .as_deref()
            .is_some_and(|uri| uri.starts_with("/Play"))
}

/// Ask before an input stops the music, and say whether the question went up.
///
/// `false` means there was nothing to ask and the caller should get on with it.
/// Shared rather than living where it was first needed: the sidebar is not the
/// only way to reach an input — Home's Sources shelf and its Most Used row
/// carry the same items — and having the question on one path and not the
/// others meant the same press warned or did not depending on where it was
/// made.
fn ask_before_input(
    backend: &Backend,
    id: DeviceId,
    action: &bluos::screen::Action,
    label: &str,
) -> bool {
    if !switches_input(action) {
        return false;
    }
    // Only worth asking while something is playing. Switching a silent
    // speaker costs nothing, and a dialog for it is just one more press.
    let playing = backend
        .with_entry(id, |e| {
            e.status.as_ref().is_some_and(bluos::Status::is_playing)
        })
        .unwrap_or(false);
    if !playing {
        return false;
    }

    backend.browsing.lock().unwrap().pending_input = Some((id, action.clone()));
    let ui = backend.ui.clone();
    let label = label.to_owned();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            ui.set_confirm_input(label.into());
        }
    });
    true
}

/// Put up the question a player asked instead of acting, unless another
/// player has been chosen since the press that led to it.
///
/// The question arrives a round trip after the press. Put up over the player
/// chosen in the meantime, it reads as being about that one — "Replace" looks
/// like it means the queue on screen — while the answer, rightly, goes to the
/// player that asked, whose queue is the one replaced. The owner kept with the
/// dialog decides where an answer goes; this decides whether to ask at all.
fn offer_dialog(backend: &Backend, id: DeviceId, selection: u64, dialog: bluos::dialog::Dialog) {
    {
        let mut browsing = backend.browsing.lock().unwrap();
        // With the lock held, for the reason on `Backend::still_selected`.
        if !backend.still_selected(selection) {
            tracing::debug!(%id, "not asking: another player was chosen");
            return;
        }
        browsing.dialog = Some((id, dialog));
    }
    backend.publish_dialog();
}

/// Open a context menu read from `id`, unless another player has been chosen
/// since the dots were pressed.
///
/// For the reason `offer_dialog` gives. The menu names no player, and it sits
/// over the queue and transport of whichever is selected, so a menu of the
/// last player's track read as being about this one's while Play now and
/// Remove from queue ran, rightly, on the player that served it.
fn put_up_menu(backend: &Backend, id: DeviceId, selection: u64, menu: Screen) {
    {
        let mut browsing = backend.browsing.lock().unwrap();
        // With the lock held, for the reason on `Backend::still_selected`.
        if !backend.still_selected(selection) {
            tracing::debug!(%id, "not opening the menu: another player was chosen");
            return;
        }
        browsing.queue_menu = Some(menu);
        browsing.queue_menu_owner = Some(id);
    }
    backend.publish_queue_menu();
}

/// Put the player's answer back on the alarms page, if it is still up and
/// still that player's.
///
/// Returns whether it was. An alarm is named by a small integer that every
/// player hands out from one, so another player's list on this page would
/// have its toggles arm and its rows open alarms that happen to share an id.
fn replace_alarms(backend: &Backend, id: DeviceId, list: bluos::alarms::Alarms) -> bool {
    match &mut backend.browsing.lock().unwrap().pane {
        Pane::Alarms(page) if page.device == id => {
            page.list = list;
            true
        }
        _ => false,
    }
}

/// The row indices the alarms editor uses.
///
/// Fixed rather than positional. `walk_settings` numbers settings rows by
/// walking them, which is right where the rows are always the same ones; here
/// Stops, Fall back and Delete each come and go, so a positional index would
/// renumber the rows below them and send a volume drag to the wrong field.
/// Above any real row, and positive. Negative would read better — nothing
/// could mistake it for a position — but the window's callbacks clamp a row
/// index at zero on the way in, so a negative constant arrives as row zero and
/// every one of these becomes "the first alarm in the list". Settings rows are
/// numbered by walking a page, so they run to tens; nine thousand is clear.
/// The Upgrade Check page's one pressable row.
///
/// Far above any position a help page reaches, for the same reason the alarm
/// constants below are: these rows share one index space with positional ones.
const UPGRADE_ROW: usize = 9100;

const ALARM_ROW_BASE: usize = 9000;
const ALARM_KIND: usize = ALARM_ROW_BASE;
const ALARM_VOLUME: usize = ALARM_ROW_BASE + 1;
const ALARM_FADE: usize = ALARM_ROW_BASE + 2;
const ALARM_SHUFFLE: usize = ALARM_ROW_BASE + 3;
const ALARM_BACKUP: usize = ALARM_ROW_BASE + 4;
const ALARM_SAVE: usize = ALARM_ROW_BASE + 5;
const ALARM_DELETE: usize = ALARM_ROW_BASE + 6;
const ALARM_SOURCE: usize = ALARM_ROW_BASE + 8;
/// The last row of the list, which is not an alarm.
const NEW_ALARM: usize = ALARM_ROW_BASE + 7;
const NEW_SCHEDULE: usize = ALARM_ROW_BASE + 9;

/// How long an alarm plays for, in minutes.
///
/// The official controller's own ladder. Offering a free number would be
/// truer to the wire — the player takes any — but a ladder is what anyone
/// setting an alarm actually wants, and it is the list the player's users
/// already know.
const DURATIONS: &[u32] = &[15, 30, 45, 60, 90, 120];

/// The letters under the day chips, Sunday first, matching the order the
/// player writes `days` in.
const DAY_LETTERS: [&str; bluos::alarms::DAYS] = ["S", "M", "T", "W", "T", "F", "S"];

/// A time as the list and the header show it.
fn clock(hour: u8, minute: u8) -> String {
    format!("{hour:02}:{minute:02}")
}

/// A schedule's finishing time, split out of the `"HHmm"` the player writes.
///
/// Nine in the morning where there is none, which is only reached on an alarm
/// being turned into a schedule for the first time: the field has to open on
/// something, and the official controller opens it an hour after the default
/// start.
fn schedule_end(alarm: &bluos::alarms::Alarm) -> (u8, u8) {
    let parsed = alarm.end.as_deref().and_then(|end| {
        let (hour, minute) = end.split_at_checked(2)?;
        Some((hour.parse().ok()?, minute.parse().ok()?))
    });
    parsed.unwrap_or((9, 0))
}

/// The days in words, which is what the list row and the editor both show.
///
/// "Once" and not "Never" for no day at all: the player treats an empty `days`
/// as a single firing rather than as an alarm that never goes off, and reading
/// seven dark chips as "once" is not something anyone would guess.
fn repeat_summary(days: &[bool; bluos::alarms::DAYS]) -> String {
    const NAMES: [&str; bluos::alarms::DAYS] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let set: Vec<&str> = days
        .iter()
        .zip(NAMES)
        .filter_map(|(on, name)| on.then_some(name))
        .collect();

    match set.len() {
        0 => "Once".to_owned(),
        7 => "Every day".to_owned(),
        _ if days[1..6].iter().all(|on| *on) && !days[0] && !days[6] => "Weekdays".to_owned(),
        _ if days[0] && days[6] && days[1..6].iter().all(|on| !*on) => "Weekends".to_owned(),
        _ => set.join(", "),
    }
}

/// What one alarm says under its time on the list.
fn alarm_detail(alarm: &bluos::alarms::Alarm) -> String {
    let mut parts = vec![repeat_summary(&alarm.days)];
    parts.push(match alarm.source.as_deref().filter(|s| !s.is_empty()) {
        Some(source) => source.to_owned(),
        // Only reached if the player sends none. It fills this in itself on
        // a save with nothing chosen — observed as "Current play queue or
        // station" — so this is a backstop, not the usual case.
        None => "Whatever the player is set to".to_owned(),
    });
    match &alarm.end {
        Some(_) => {
            let (hour, minute) = schedule_end(alarm);
            parts.push(format!("until {}", clock(hour, minute)));
        }
        None => parts.push(format!("{} min", alarm.duration)),
    }
    parts.join(" · ")
}

/// One browse row on its way to the window, for the same reason as
/// [`TrackData`]: the pixels are `Send` and the `Image` is not.
#[derive(Clone)]
struct BrowseData {
    index: i32,
    /// Whether pressing it starts music rather than opening a screen.
    plays: bool,
    title: String,
    subtitle: String,
    /// Its place on the record, and what the player says it is encoded as.
    /// Both empty except on a screen about one album.
    track: String,
    quality: String,
    /// The caption on a section heading's button — "Customize" on the Inputs
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
    /// as `playing`: you can be looking at TuneIn's favorites while the
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

/// Everything about a browse row that the window draws.
///
/// Split out because the browse memo hashes rows from two places — the
/// selector strip and each block — and a field added to one and not the other
/// is a row that stops redrawing when it changes.
fn browse_row_fingerprint(row: &BrowseData, hash: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    row.index.hash(hash);
    row.plays.hash(hash);
    row.title.hash(hash);
    row.subtitle.hash(hash);
    row.track.hash(hash);
    row.quality.hash(hash);
    row.action.hash(hash);
    row.cover.is_some().hash(hash);
    row.glyph.hash(hash);
    row.heading.hash(hash);
    row.actionable.hash(hash);
    row.playing.hash(hash);
    row.selected.hash(hash);
    row.has_menu.hash(hash);
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
        Glyph::Headphones => icons.get_headphones(),
        Glyph::Broadcast => icons.get_broadcast(),
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

    // Selecting the backend has to come first. Slint initializes its platform
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
    ui.on_answer_input(move |go| {
        let _ = tx.send(Command::ConfirmInput(go));
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
    ui.on_track_action(move |name| {
        let _ = tx.send(Command::TrackAction(name.to_string()));
    });

    let tx = commands.clone();
    ui.on_open_stations(move || {
        let _ = tx.send(Command::OpenStations);
    });

    let tx = commands.clone();
    ui.on_stations_edit(move || {
        let _ = tx.send(Command::StationsEdit);
    });

    let tx = commands.clone();
    ui.on_station_add(move |name, url| {
        let _ = tx.send(Command::StationAdd(name.to_string(), url.to_string()));
    });

    let tx = commands.clone();
    ui.on_station_play(move |at| {
        let _ = tx.send(Command::StationPlay(at.max(0) as usize));
    });

    let tx = commands.clone();
    ui.on_station_rename(move |at, name| {
        let _ = tx.send(Command::StationRename(at.max(0) as usize, name.to_string()));
    });

    let tx = commands.clone();
    ui.on_station_remove(move |at| {
        let _ = tx.send(Command::StationRemove(at.max(0) as usize));
    });

    let tx = commands.clone();
    ui.on_station_move(move |from, to| {
        let _ = tx.send(Command::StationMove(
            from.max(0) as usize,
            to.max(0) as usize,
        ));
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
    ui.on_answer_upgrade(move |go| {
        let _ = tx.send(Command::UpgradeAnswer(go));
    });

    let tx = commands.clone();
    ui.on_answer_update_notice(move |show| {
        let _ = tx.send(Command::UpdateNoticeAnswer(show));
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
    ui.on_alarm_start_changed(move |hour, minute| {
        let _ = tx.send(Command::AlarmEdit(AlarmField::Start(
            hour.clamp(0, 23) as u8,
            minute.clamp(0, 59) as u8,
        )));
    });

    let tx = commands.clone();
    ui.on_alarm_end_changed(move |hour, minute| {
        let _ = tx.send(Command::AlarmEdit(AlarmField::End(
            hour.clamp(0, 23) as u8,
            minute.clamp(0, 59) as u8,
        )));
    });

    let tx = commands.clone();
    ui.on_alarm_day_toggled(move |at| {
        let _ = tx.send(Command::AlarmEdit(AlarmField::Day(at.max(0) as usize)));
    });

    let tx = commands.clone();
    // Dismissing sends -1, which becomes an index no dialog has: the arm
    // still takes the question down, it just presses nothing.
    ui.on_dialog_press(move |at| {
        let at = usize::try_from(at).unwrap_or(usize::MAX);
        let _ = tx.send(Command::DialogPress(at));
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
    ui.on_browse_jump(move |key| {
        let _ = tx.send(Command::BrowseJump(key.to_string()));
    });

    let tx = commands.clone();
    ui.on_open_search(move || {
        let _ = tx.send(Command::OpenSearch);
    });

    let tx = commands.clone();
    ui.on_browse_ends(move |to_end| {
        let _ = tx.send(Command::BrowseEnds(to_end));
    });

    let tx = commands.clone();
    ui.on_queue_more(move || {
        let _ = tx.send(Command::QueueMore);
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
    // Cover art legitimately comes from a streaming service's CDN on another
    // host, which is why it does not share the protocol client — see the note
    // on `Client::http_client`. It still does not get to follow a redirect
    // wherever a player points it: the hop limit and the per-hop address check
    // in `artwork::permitted` apply to every destination, not just the first.
    // Shared with the client below, because a redirect has to be judged by the
    // same rule the fetch was: a player answers /Artwork with a 301 to its own
    // library service on another port, and a policy that only knew the
    // public-address rule stopped every cover the player served.
    let players: artwork::Players = Default::default();

    let art = match reqwest::Client::builder()
        // The protocol client's figure, for its reason: a cover whose host
        // swallows the SYN would otherwise hold a fetch permit until the whole
        // fetch timed out.
        .connect_timeout(bluos::client::CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom({
            let players = players.clone();
            move |attempt| {
                if attempt.previous().len() >= artwork::MAX_HOPS {
                    return attempt.stop();
                }
                match artwork::allowed_by(&players, attempt.url()) {
                    true => attempt.follow(),
                    false => attempt.stop(),
                }
            }
        }))
        .build()
    {
        Ok(art) => art,
        Err(e) => return say(&ui, format!("could not start HTTP: {e}")),
    };

    // Not fatal. This used to return, which took the command loop with it: the
    // message told the user to add a player by address and then nothing was
    // listening for one, because `run_commands` is spawned further down. A
    // window that paints and answers nothing is worse than one that cannot
    // find players by itself — remembered addresses and add-by-address are
    // both still perfectly good ways in.
    let discovery = match Discovery::bind() {
        Ok(found) => Some(Arc::new(found)),
        Err(e) => {
            tracing::warn!("could not bind the discovery port: {e}");
            say(
                &ui,
                format!("could not bind the discovery port: {e}. Add players by address instead."),
            );
            None
        }
    };

    let backend = Backend {
        registry: Arc::new(Mutex::new(BTreeMap::new())),
        selected: Arc::new(Mutex::new(None)),
        selections: Arc::new(AtomicU64::new(0)),
        commands,
        ui: ui.clone(),
        artwork: Arc::new(Artwork::new(art, players)),
        browsing: Arc::new(Mutex::new(Browsing {
            // The one field that starts as something other than empty. Read
            // here rather than lazily on the first search, so the list is
            // already under the field the first time it is opened.
            recent: searches::load(),
            ..Default::default()
        })),
        known: Arc::new(Mutex::new(Vec::new())),
        writes: Arc::new(lane::Lane::default()),
        searches: Arc::new(AtomicU64::new(0)),
        forms: Arc::new(AtomicU64::new(0)),
        openings: Arc::new(AtomicU64::new(0)),
        sent_transport: Arc::new(AtomicU64::new(0)),
        sent_queue: Arc::new(AtomicU64::new(0)),
        sent_players: Arc::new(AtomicU64::new(0)),
        sent_settings: Arc::new(AtomicU64::new(0)),
        told_about_update: Arc::new(Mutex::new(std::collections::HashSet::new())),
        update_offer: Arc::new(Mutex::new(None)),
        sent_browse: Arc::new(AtomicU64::new(0)),
        sent_index: Arc::new(AtomicU64::new(0)),
        sent_era: Arc::new(AtomicU64::new(0)),
        refreshing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        orders: Arc::new(Mutex::new(order::load())),
    };

    tokio::spawn(run_commands(
        command_rx,
        backend.clone(),
        discovery.clone(),
        http.clone(),
    ));
    tokio::spawn(tick_position(backend.clone()));

    // Only where there is a socket to look on. Everything below runs either
    // way: remembered addresses do not need discovery.
    if let Some(discovery) = &discovery {
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
    }

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
    // Without a socket there is nothing to sweep and nothing to listen to, so
    // this task simply ends. The window and its command loop carry on.
    let Some(discovery) = discovery else {
        backend.set_looking(false);
        return;
    };

    // Said out loud for as long as it is true. The sweep spreads its
    // broadcasts across twelve seconds, and for the whole of that a first-run
    // window had nothing on it and no reason given — which is the moment
    // somebody decides whether the app works at all.
    backend.set_looking(true);
    sweep(backend.clone(), discovery.clone(), http.clone()).await;
    backend.set_looking(false);

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
        // A player serves its own art from its own address, which is on the
        // user's subnet — the space `artwork::permitted` refuses to dial on a
        // player's say-so. Recorded here, where an address becomes a player.
        self.artwork.remember_player(id.host);

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
                    upgrading: false,
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
                    card_cover: None,
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
                self.selections.fetch_add(1, Ordering::SeqCst);
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

    /// The selected player, and which selection of it this is.
    ///
    /// Read together under one lock, which is what makes the pair mean
    /// anything: the number is bumped under the same lock as the id is
    /// changed, so it cannot belong to a different player than the id beside
    /// it.
    fn selection(&self) -> Option<(DeviceId, u64)> {
        let selected = self.selected.lock().unwrap();
        selected.map(|id| (id, self.selections.load(Ordering::SeqCst)))
    }

    /// Whether nothing has been selected since `selection` was read.
    ///
    /// No lock is taken, so this is the one question about the selection that
    /// can be asked with `browsing` held — which is where it has to be asked,
    /// alongside the write it guards, or the selection can change between
    /// the answer and the write.
    fn still_selected(&self, selection: u64) -> bool {
        self.selections.load(Ordering::SeqCst) == selection
    }

    /// The player whose screens are showing, if it is also the one selected.
    ///
    /// `selection` is read before `browsing` is taken, and `browsing` is what
    /// is passed in. A press on the browse pane runs on `device`, and `device`
    /// stays the last player's until the next one's Home lands — and for as
    /// long as that player is chosen, when it cannot say what screens it has.
    /// A row pressed in between is the last player's row, and it played there
    /// while the window showed another player as the one being controlled.
    /// So every press there asks this first and does nothing on `None`.
    fn browsing_selected(
        &self,
        browsing: &Browsing,
        selection: Option<(DeviceId, u64)>,
    ) -> Option<DeviceId> {
        selection
            .filter(|(id, n)| browsing.device == Some(*id) && self.still_selected(*n))
            .map(|(id, _)| id)
    }

    /// Whether a page read for `id` may be put up now, with `browsing` held.
    ///
    /// Two ways it may. Over a pane that is already that player's — its
    /// settings, its shares — because a page opened from one of those is a
    /// step deeper into it, and those panes stay up when the sidebar moves.
    /// Or while `id` is still the selected player it was when the page was
    /// asked for, which is `selection`: taken then, and `None` if `id` was
    /// not selected at all. Anything else is a reply that outlived the choice
    /// it was made under, and it would put one player's page over another's
    /// name.
    fn may_open_for(&self, browsing: &Browsing, id: DeviceId, selection: Option<u64>) -> bool {
        browsing.pane.owner() == Some(id) || selection.is_some_and(|n| self.still_selected(n))
    }

    /// The selection a page for `id` is being asked for under, if `id` is the
    /// player selected. Passed to [`Backend::may_open_for`] when it lands.
    fn selection_of(&self, id: DeviceId) -> Option<u64> {
        self.selection()
            .filter(|(selected, _)| *selected == id)
            .map(|(_, n)| n)
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

        // Gathered beside the rows rather than inside them: `Device` is held
        // in the registry and travels between threads, and `slint::Image` is
        // not `Send`, so the picture cannot live in the struct. The two are
        // built in one pass over one lock and handed to the window together,
        // which is what keeps them aligned.
        let mut covers: Vec<Option<Pixels>> = Vec::new();

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
                    covers.push(entry.card_cover.clone());
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
            // Whether each row has its art yet. Art arrives after the row it
            // belongs to, so without this the covers land in a model the memo
            // then refuses to send — the same trap the queue's fingerprint
            // documents.
            for cover in &covers {
                cover.is_some().hash(&mut hash);
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
            // silently move the selection to its neighbor.
            let selected_id = ui
                .get_devices()
                .row_data(ui.get_selected() as usize)
                .map(|d| d.id);
            let restored = selected_id
                .and_then(|id| rows.iter().position(|d| d.id == id))
                .unwrap_or(0);

            ui.set_device_covers(ModelRc::new(VecModel::from(
                covers
                    .into_iter()
                    .map(|c| c.map(slint::Image::from_rgba8).unwrap_or_default())
                    .collect::<Vec<_>>(),
            )));
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
    /// The alarms screen: either the list, or one alarm open for editing.
    ///
    /// Both go out as settings rows, so the pane, its scrolling, its title and
    /// its back arrow are the ones every other page uses. What settings rows
    /// cannot express — a time and seven days — is a strip of its own above
    /// them, published through the properties below.
    /// Tell the window a player is upgrading, and how far along.
    ///
    /// Also marks the entry, which is what takes the player's controls away:
    /// a speaker rewriting its own firmware will not answer a volume change,
    /// and offering one invites a person to keep pressing while it does not.
    fn show_upgrade(&self, id: DeviceId, progress: &bluos::upgrade::Progress) {
        let stage = progress.stage().label().to_owned();

        {
            let mut registry = self.registry.lock().unwrap();
            if let Some(entry) = registry.get_mut(&id) {
                entry.upgrading = true;
            }
        }

        // The offer was worked out when the page was built and does not
        // revise itself, so it outlived the upgrade starting: the Install row
        // stayed pressable, and pressing it put the confirmation up again over
        // a player already installing. Retired here, where the first progress
        // arrives, and the page redrawn if it happens to be the one on screen.
        let showing = {
            let mut browsing = self.browsing.lock().unwrap();
            match &mut browsing.pane {
                Pane::HelpDetail(_, _, _, offer) if *offer == Some(id) => {
                    *offer = None;
                    true
                }
                _ => false,
            }
        };
        if showing {
            self.publish_help();
        }

        // Said on the player's own row as well as in the strip, and marked
        // unreachable so the row reads as something that will not answer.
        // Note this dims the row rather than disabling its controls: taking
        // them away properly means keeping an upgrading player out of the
        // selection altogether, which is how the official controller does it
        // and is a larger change than this.
        //
        // Its own field rather than `role`. This used to write the line there
        // and it never once reached the window: `publish` recomputes `role`
        // from the grouping state on every call, including the one at the
        // bottom of this function, so the words were overwritten on the same
        // tick that carried them.
        let line = match progress.bar() {
            Some(percent) => format!("{stage} — {percent}%"),
            None => stage.clone(),
        };
        self.update(id, |view| {
            view.upgrading = true;
            view.upgrade_line = line.clone().into();
            view.reachable = false;
        });

        // A Powernode sends no counters at any point, so for that player the
        // words carry the whole thing and the bar stays at zero. `bar()` is
        // what decides there is something worth drawing.
        let percent = progress.bar().unwrap_or(0) as f32;
        let name = progress
            .name
            .clone()
            .or_else(|| self.with_entry(id, |e| e.view.name.to_string()))
            .unwrap_or_else(|| "The player".to_owned());

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_upgrading_player(name.into());
                ui.set_upgrading_stage(stage.into());
                ui.set_upgrading_percent(percent);
            }
        });
        self.publish();
    }

    /// Whether discovery is still sweeping, so the window can say so.
    fn set_looking(&self, looking: bool) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_looking_for_players(looking);
            }
        });
    }

    /// Put every player's controls back.
    fn clear_upgrade(&self) {
        let was: Vec<DeviceId> = {
            let mut registry = self.registry.lock().unwrap();
            let ids = registry
                .iter()
                .filter(|(_, e)| e.upgrading)
                .map(|(id, _)| *id)
                .collect();
            for entry in registry.values_mut() {
                entry.upgrading = false;
            }
            ids
        };

        // The next status puts the real state back; this only stops the
        // upgrade's words outliving it.
        for id in was {
            self.update(id, |view| {
                view.upgrading = false;
                view.upgrade_line = slint::SharedString::new();
                view.reachable = true;
            });
        }

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_upgrading_player(slint::SharedString::new());
                ui.set_upgrading_stage(slint::SharedString::new());
                ui.set_upgrading_percent(0.0);
            }
        });
        self.publish();
    }

    fn publish_alarms(&self) {
        let page = {
            let browsing = self.browsing.lock().unwrap();
            match &browsing.pane {
                Pane::Alarms(page) => (**page).clone(),
                _ => return,
            }
        };

        let mut rows: Vec<SettingData> = Vec::new();
        let title;

        // The picker sits over everything else on this pane: while it is open
        // it is what the page is, and Back walks it rather than the editor.
        if let Some(level) = page.picking.last() {
            title = level.title.clone();
            let mut group: Option<std::sync::Arc<str>> = None;
            for (at, row) in level.rows.rows.iter().enumerate() {
                // Some services group their rows — Radio Paradise by quality —
                // and the heading is drawn once, where it changes.
                if row.group.is_some() && row.group != group {
                    group = row.group.clone();
                    rows.push(SettingData {
                        label: group.as_deref().unwrap_or_default().to_uppercase(),
                        heading: true,
                        ..SettingData::blank()
                    });
                }
                rows.push(SettingData {
                    index: at as i32,
                    label: row.text.clone(),
                    glyph: Some(if row.playable {
                        Glyph::Play
                    } else {
                        Glyph::Folder
                    }),
                    control: "link",
                    ..SettingData::blank()
                });
            }
            if level.rows.rows.is_empty() {
                rows.push(SettingData {
                    label: "Nothing here".to_owned(),
                    detail: "This service offers nothing an alarm can play".to_owned(),
                    glyph: Some(Glyph::Info),
                    available: false,
                    ..SettingData::blank()
                });
            }

            // The time and days strip belongs to the editor, not to this.
            let ui = self.ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade() {
                    ui.set_alarm_editing(false);
                }
            });
            return self.send_settings(rows, title);
        }

        match &page.editing {
            None => {
                title = "Alarms".to_owned();
                if !page.list.alarms.is_empty() {
                    rows.push(SettingData {
                        label: "ALARMS".to_owned(),
                        heading: true,
                        ..SettingData::blank()
                    });
                }
                // Positions address list rows and the constants above address
                // the editor's, both through one `usize`, so a list long
                // enough to reach `ALARM_ROW_BASE` would have its rows read as
                // editor presses — a press on row 9006 arriving as Delete. No
                // player serves anything near this; the take is what makes
                // that a fact rather than an assumption.
                for (at, alarm) in page.list.alarms.iter().take(ALARM_ROW_BASE).enumerate() {
                    rows.push(SettingData {
                        // The list is addressed by position; the arm below
                        // carries the player's own id, which is what it needs.
                        index: at as i32,
                        label: clock(alarm.hour, alarm.minute),
                        detail: alarm_detail(alarm),
                        glyph: Some(if alarm.is_schedule() {
                            Glyph::Recent
                        } else {
                            Glyph::Alarm
                        }),
                        control: "boolean",
                        on: alarm.enabled,
                        // Both a switch and a door: arming is the commonest
                        // thing anyone does to an alarm, and making that cost
                        // a page would be the wrong trade.
                        opens: true,
                        ..SettingData::blank()
                    });
                }
                rows.push(SettingData {
                    index: NEW_ALARM as i32,
                    label: "New alarm…".to_owned(),
                    detail: "Plays for a while and stops".to_owned(),
                    glyph: Some(Glyph::Alarm),
                    control: "link",
                    ..SettingData::blank()
                });
                // Only where the player can do them. `supportsEndTime` is its
                // answer about itself, and offering the row on a player that
                // says no would be offering something that cannot be saved.
                //
                // A row of its own rather than a choice buried in the editor's
                // Stops chooser, which is where this lived and why it read as
                // missing: the official controller asks "alarm or schedule"
                // before anything else, and a thing nobody can find is not a
                // thing the app has.
                if page.list.supports_end_time {
                    rows.push(SettingData {
                        index: NEW_SCHEDULE as i32,
                        label: "New schedule…".to_owned(),
                        detail: "Plays between two times".to_owned(),
                        glyph: Some(Glyph::Recent),
                        control: "link",
                        ..SettingData::blank()
                    });
                }
            }
            Some(alarm) => {
                title = if alarm.id == 0 {
                    "New alarm".to_owned()
                } else if alarm.is_schedule() {
                    "Schedule".to_owned()
                } else {
                    "Alarm".to_owned()
                };

                // Only where the player says it can: `supportsEndTime` is its
                // answer, not a preference.
                if page.list.supports_end_time {
                    rows.push(SettingData {
                        index: ALARM_KIND as i32,
                        label: "Stops".to_owned(),
                        detail: if alarm.is_schedule() {
                            "At a set time".to_owned()
                        } else {
                            format!("After {} minutes", alarm.duration)
                        },
                        glyph: Some(Glyph::Recent),
                        control: "list",
                        options: DURATIONS
                            .iter()
                            .map(|m| format!("After {m} minutes"))
                            .chain(std::iter::once("At a set time".to_owned()))
                            .collect(),
                        option_index: if alarm.is_schedule() {
                            DURATIONS.len() as i32
                        } else {
                            DURATIONS
                                .iter()
                                .position(|m| *m == alarm.duration)
                                .unwrap_or(0) as i32
                        },
                        ..SettingData::blank()
                    });
                }

                rows.push(SettingData {
                    index: ALARM_SOURCE as i32,
                    label: "Plays".to_owned(),
                    detail: match alarm.source.as_deref().filter(|s| !s.is_empty()) {
                        Some(source) => source.to_owned(),
                        None => "Whatever the player is set to".to_owned(),
                    },
                    glyph: Some(Glyph::Radio),
                    control: "link",
                    ..SettingData::blank()
                });
                rows.push(SettingData {
                    index: ALARM_VOLUME as i32,
                    label: "Volume".to_owned(),
                    glyph: Some(Glyph::Volume),
                    control: "range",
                    number: alarm.volume as f32,
                    minimum: 0.0,
                    maximum: 100.0,
                    step: 1.0,
                    ..SettingData::blank()
                });
                rows.push(SettingData {
                    index: ALARM_FADE as i32,
                    label: "Fade in".to_owned(),
                    detail: "Come up gradually rather than starting at once".to_owned(),
                    glyph: Some(Glyph::Volume),
                    control: "boolean",
                    on: alarm.fade_in,
                    ..SettingData::blank()
                });
                rows.push(SettingData {
                    index: ALARM_SHUFFLE as i32,
                    label: "Shuffle".to_owned(),
                    glyph: Some(Glyph::Shuffle),
                    control: "boolean",
                    on: alarm.shuffle,
                    // The source's answer, not the user's — a stream cannot be
                    // shuffled, and the row says so by being dimmed.
                    available: alarm.can_shuffle,
                    ..SettingData::blank()
                });
                if !alarm.is_schedule() {
                    rows.push(SettingData {
                        index: ALARM_BACKUP as i32,
                        label: "Fall back to a tone".to_owned(),
                        detail: "If the source cannot be reached".to_owned(),
                        glyph: Some(Glyph::Alarm),
                        control: "boolean",
                        on: alarm.use_backup,
                        ..SettingData::blank()
                    });
                }

                rows.push(SettingData {
                    index: ALARM_SAVE as i32,
                    label: if alarm.id == 0 { "Create" } else { "Save" }.to_owned(),
                    glyph: Some(Glyph::Save),
                    control: "button",
                    value: "Save".to_owned(),
                    ..SettingData::blank()
                });
                if alarm.id != 0 {
                    rows.push(SettingData {
                        index: ALARM_DELETE as i32,
                        label: "Delete this alarm".to_owned(),
                        glyph: Some(Glyph::Clear),
                        control: "button",
                        value: "Delete".to_owned(),
                        ..SettingData::blank()
                    });
                }
            }
        }

        // The strip above the rows, which is the part settings rows cannot do.
        let editing = page.editing.clone();
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            match &editing {
                Some(alarm) => {
                    let (end_hour, end_minute) = schedule_end(alarm);
                    ui.set_alarm_editing(true);
                    ui.set_alarm_schedule(alarm.is_schedule());
                    ui.set_alarm_hour(alarm.hour as i32);
                    ui.set_alarm_minute(alarm.minute as i32);
                    ui.set_alarm_end_hour(end_hour as i32);
                    ui.set_alarm_end_minute(end_minute as i32);
                    ui.set_alarm_days(ModelRc::new(VecModel::from(alarm.days.to_vec())));
                    ui.set_alarm_repeat(repeat_summary(&alarm.days).into());
                    ui.set_alarm_letters(ModelRc::new(VecModel::from(
                        DAY_LETTERS
                            .iter()
                            .map(|d| slint::SharedString::from(*d))
                            .collect::<Vec<_>>(),
                    )));
                }
                None => ui.set_alarm_editing(false),
            }
        });

        self.send_settings(rows, title);
    }

    /// Put the player's question in front of the user, or take it away.
    ///
    /// The buttons are the player's, in its order and with its wording — the
    /// one it colors is the one it considers destructive, and nothing here
    /// decides which that is.
    fn publish_dialog(&self) {
        let asked = self
            .browsing
            .lock()
            .unwrap()
            .dialog
            .as_ref()
            .map(|(_, dialog)| dialog.clone());
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            let Some(asked) = asked else {
                ui.set_dialog_open(false);
                return;
            };
            let buttons: Vec<DialogButton> = asked
                .choices
                .iter()
                .enumerate()
                .map(|(index, choice)| DialogButton {
                    index: index as i32,
                    label: choice.text.as_str().into(),
                    // Colored means "this is the one that throws something
                    // away", which is what the primary styling is for here.
                    destructive: choice.color.is_some(),
                })
                .collect();
            ui.set_dialog_title(asked.title.unwrap_or_default().into());
            ui.set_dialog_body(asked.body.unwrap_or_default().into());
            ui.set_dialog_buttons(ModelRc::new(VecModel::from(buttons)));
            ui.set_dialog_open(true);
        });
    }

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
                    // `label`, not `title`: a context menu writes its rows as
                    // `<item text="Favourite">` and leaves `title` empty, so
                    // reading `title` here dropped every line and the menu
                    // reported that the player had offered nothing.
                    // The glyph is chosen from the player's own word and the
                    // label drawn in ours: the matcher reads what was written,
                    // the screen shows what this app would have written.
                    let glyph = glyphs::menu_glyph(item.label()?);
                    let label = american(item.label()?);
                    Some((at as i32, label, glyph))
                })
                .collect();

            (
                american(menu.heading().unwrap_or_default()),
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
        // The player the menu came from, not whichever is being browsed — the
        // same rule `QueueMenuAction` follows when it addresses the action.
        // The two differ while a selection is settling, and the menu's image
        // path is relative to the player that served it, so resolving it
        // against another player's base produced a URL that missed and left
        // the header without a thumbnail the row behind it was showing.
        //
        // Read out, then looked up: a guard in a method chain lives to the end
        // of the statement, so this held `browsing` while `with_entry` took
        // `registry`. Two of them at once is what the note on `Backend` says
        // never to do, and `publish_browse` spells out the shape that avoids
        // it.
        let owner = self.browsing.lock().unwrap().queue_menu_owner;
        let client = owner.and_then(|id| self.with_entry(id, |e| e.client.clone()));
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

        // What the player offers to do with the queue. Recognized by what each
        // action is rather than by what it says: matching on the label is what
        // the official controller does and it would break on the first player
        // set to another language.
        let buttons: Vec<QueueButtonData> = {
            let browsing = self.browsing.lock().unwrap();
            let building = browsing.queue_building;
            // Another player's buttons are not drawn under this one's queue:
            // until its own document arrives there are none.
            browsing
                .queue_screen
                .as_ref()
                .filter(|(owner, _)| Some(*owner) == selected)
                .map(|(_, screen)| {
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
                                // Queue Builder Mode. The player takes the
                                // call and does nothing observable with it —
                                // `/ui/Queue` and every browse screen come
                                // back byte-identical either way — so the mode
                                // is this app's to keep, exactly as it is the
                                // official controller's.
                                ActionKind::PlayerLink if uri.contains("CBQ=") => 4,
                                _ => 0,
                            };

                            Some(QueueButtonData {
                                index: at as i32,
                                glyph: glyphs::glyph_for(&label, None),
                                // Lit from this app's own flag where the
                                // player has no opinion to report.
                                highlight: button.highlight || (mode == 4 && building),

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
                    let live = queue.is_live(&status);

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
            let lit = browsing.lit();
            let mut rows: Vec<BrowseData> = Vec::new();

            for (index, (label, _)) in browsing.screens.iter().enumerate() {
                rows.push(BrowseData {
                    // The sidebar opens screens; nothing in it starts music.
                    plays: false,
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
                            plays: false,
                            track: String::new(),
                            quality: String::new(),
                            index: ordinal as i32,
                            title: title.clone(),
                            subtitle: String::new(),
                            action: section
                                .menu_actions
                                .first()
                                .and_then(|menu| menu.text.as_deref())
                                .map(american)
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
                            plays: false,
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
    /// The list of places a track can go.
    ///
    /// Indices are positions in a flattened walk of the groups, so that
    /// `SettingAction` can find its way back to the same playlist — the same
    /// arrangement the settings pages already use.
    /// The hand-typed stations, as their own pane.
    fn publish_stations(&self) {
        let (stations, editing) = {
            let browsing = self.browsing.lock().unwrap();
            let Pane::Stations(page) = &browsing.pane else {
                return;
            };
            (page.stations.clone(), page.editing)
        };

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            let rows: Vec<MyStation> = stations
                .into_iter()
                .map(|station| MyStation {
                    name: station.name.into(),
                    url: station.url.into(),
                })
                .collect();
            ui.set_stations(ModelRc::new(VecModel::from(rows)));
            ui.set_stations_editing(editing);
            ui.set_in_stations(true);
            ui.set_browse_title("My Stations".into());
            ui.set_browse_can_go_back(true);
        });
    }

    /// The preset being renamed, as a settings row.
    ///
    /// One field, so a text row is the whole page: it commits on Enter, which
    /// is the only thing that could mean "done" here.
    fn publish_edit_preset(&self) {
        let (rows, title) = {
            let browsing = self.browsing.lock().unwrap();
            let Pane::EditPreset(page) = &browsing.pane else {
                return;
            };

            let mut rows = vec![SettingData {
                index: 0,
                label: "Name".to_owned(),
                detail: "Type a new name and press Enter".to_owned(),
                glyph: Some(Glyph::Preset),
                control: "text",
                value: page.preset.name.clone(),
                available: true,
                ..SettingData::blank()
            }];

            // What it plays, shown and not editable. The stream came from the
            // player's own catalog and is what ties the preset to the thing
            // it names; retyping it by hand would quietly sever that.
            if let Some(url) = page.preset.url.as_deref() {
                rows.push(SettingData {
                    index: -1,
                    label: "Plays".to_owned(),
                    detail: url.to_owned(),
                    control: "none",
                    available: false,
                    ..SettingData::blank()
                });
            }

            (rows, format!("Preset {}", page.slot))
        };

        self.send_settings(rows, title);
    }

    fn publish_playlists(&self) {
        let (title, rows) = {
            let browsing = self.browsing.lock().unwrap();
            let Pane::Playlists(page) = &browsing.pane else {
                return;
            };

            let mut rows: Vec<SettingData> = Vec::new();
            let mut at = 0i32;

            for group in &page.options.groups {
                // A heading only where there is more than one service to tell
                // apart. One service is just a list.
                if page.options.groups.len() > 1 {
                    rows.push(SettingData {
                        index: -1,
                        label: group
                            .service_name
                            .clone()
                            .or_else(|| group.service.clone())
                            .unwrap_or_else(|| "Playlists".to_owned()),
                        heading: true,
                        available: true,
                        ..SettingData::blank()
                    });
                }

                for playlist in &group.playlists {
                    rows.push(SettingData {
                        index: at,
                        label: playlist.name.clone(),
                        glyph: Some(Glyph::Playlist),
                        control: "link",
                        available: true,
                        ..SettingData::blank()
                    });
                    at += 1;
                }

                if group.can_create {
                    rows.push(if page.naming {
                        SettingData {
                            index: at,
                            label: "New playlist".to_owned(),
                            detail: "Type a name and press Enter".to_owned(),
                            glyph: Some(Glyph::Add),
                            control: "text",
                            available: true,
                            ..SettingData::blank()
                        }
                    } else {
                        SettingData {
                            index: at,
                            label: "New playlist…".to_owned(),
                            glyph: Some(Glyph::Add),
                            control: "link",
                            available: true,
                            ..SettingData::blank()
                        }
                    });
                    at += 1;
                }
            }

            (page.title.clone(), rows)
        };

        self.send_settings(rows, title);
    }

    /// Put the Help menu, or a page reached from it, in the middle pane.
    fn publish_help(&self) {
        // Before `browsing`, which is not taken with `selected` held.
        let selected = *self.selected.lock().unwrap();
        let (page, offer) = {
            let browsing = self.browsing.lock().unwrap();
            match &browsing.pane {
                // Drawn only while the player the check was for is the one
                // selected. The page itself stays up when another is chosen,
                // and Install under that player's name would read as its own.
                Pane::HelpDetail(title, facts, _, offer) => (
                    Some((title.clone(), facts.clone())),
                    offer.is_some() && *offer == selected,
                ),
                _ => (None, false),
            }
        };

        if let Some((title, facts)) = page {
            let mut rows: Vec<SettingData> = facts
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

            // Only where the check said there is something to install. A
            // button offering to rewrite a player's firmware is not something
            // to leave lying about on a page that is otherwise all facts.
            if offer {
                rows.push(SettingData {
                    index: UPGRADE_ROW as i32,
                    label: "Install update".to_owned(),
                    detail: "Restarts the player. Takes a few minutes.".to_owned(),
                    glyph: Some(Glyph::Rescan),
                    control: "link",
                    available: true,
                    ..SettingData::blank()
                });
            }

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
        // license is are the app's own, and the trademark note is about
        // somebody else's — running them together reads as one claim.
        rows.push(SettingData {
            index: -1,
            // "License", not "licence": that is the name of the thing, and
            // the MIT License spells itself that way whatever the surrounding
            // prose does.
            label: "© 2026 Jonathan Zeppettini · MIT License".to_owned(),
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
                option_index: drawn_choice(field, values.get(&field.name).map(String::as_str)),
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
            Alarms,
        }
        let showing = match self.browsing.lock().unwrap().pane {
            Pane::Browse => Showing::Browse,
            Pane::Settings(..) => Showing::Settings,
            Pane::Help | Pane::HelpDetail(..) => Showing::Help,
            Pane::Web(..) => Showing::Web,
            Pane::Form(_) => Showing::Form,
            Pane::NowPlaying => Showing::NowPlaying,
            Pane::Customise(_) => Showing::Customise,
            Pane::Alarms(_) => Showing::Alarms,
            // Drawn with the settings rows, which already have a line that is
            // a link and a line that is a text field.
            Pane::Playlists(_) => Showing::Settings,
            Pane::Stations(_) => Showing::Settings,
            Pane::EditPreset(_) => Showing::Settings,
        };

        let large = matches!(showing, Showing::NowPlaying);
        let customising = matches!(showing, Showing::Customise);
        // Cleared here rather than only by `publish_alarms`, which is the one
        // publisher that can set it and only runs while the alarms pane is up.
        // Left to that alone, walking out of a half-made alarm — to Search, to
        // Home, anywhere — left its time and days strip pinned across the
        // bottom of whatever came next. Every flag that says "this pane is
        // showing" has to be answered by whichever pane actually is.
        //
        // Safe to set false unconditionally: `publish_alarms` runs after this
        // in the same queue and puts the real value back.
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_now_playing(large);
                ui.set_customising(customising);
                ui.set_alarm_editing(false);
                // And the stations pane, for the same reason as the two
                // above: `publish_stations` can only ever set this true, so
                // walking away from the pane would leave it drawn over
                // whatever came next. Safe to clear unconditionally —
                // `publish_stations` runs later in the same queue when the
                // pane really is up, and puts it back.
                ui.set_in_stations(false);
                // And the settings rows, which look answered but are not.
                // `publish_settings` clears this whenever the pane is no
                // longer a settings page — but the Stations, Playlists and
                // Edit-preset panes are dispatched as `Showing::Settings` and
                // go to their own publisher instead, so `publish_settings`
                // never runs for them and never gets the chance. The two that
                // draw settings-shaped rows put it back through
                // `send_settings`; the stations pane does not, and so came up
                // with the player's Alarms, Sleep timer, Audio and Player rows
                // still stacked underneath its list.
                ui.set_in_settings(false);
                // Back closes both of these outright — see the arm in
                // `Command::BrowseBack` that puts them back to `Pane::Browse`
                // — so there is always somewhere to go. Neither publisher says
                // so, though, and the flag is left reading for the browse
                // screen underneath: opened from a top-level screen it is
                // false, and Escape refused to close them. The same overlay
                // opened one screen deeper closed on the first press, which is
                // the sort of difference nobody can account for.
                if large || customising {
                    ui.set_browse_can_go_back(true);
                }
            }
        });

        match showing {
            // Settings first: it is what takes the pane back off the browse
            // screen when nothing else is showing.
            Showing::Browse => {
                self.publish_settings();
                self.publish_browse();
            }
            Showing::Settings => {
                let pane = {
                    let browsing = self.browsing.lock().unwrap();
                    match &browsing.pane {
                        Pane::Playlists(_) => 1,
                        Pane::Stations(_) => 2,
                        Pane::EditPreset(_) => 3,
                        _ => 0,
                    }
                };
                match pane {
                    1 => self.publish_playlists(),
                    2 => self.publish_stations(),
                    3 => self.publish_edit_preset(),
                    _ => self.publish_settings(),
                }
            }
            Showing::Help => self.publish_help(),
            Showing::Web => self.publish_web(),
            Showing::Form => self.publish_form(),
            // Nothing to send: it is drawn from what the transport already
            // publishes. Only the settings rows have to be taken down.
            Showing::Customise => self.publish_customise(),
            // Its rows go out through the settings pane like Help's do; the
            // editor's time and days are their own strip above them.
            Showing::Alarms => self.publish_alarms(),
            Showing::NowPlaying => self.publish_settings(),
        }
    }

    /// Turn a settings page into rows for the middle pane.
    fn publish_settings(&self) {
        let owned = self
            .browsing
            .lock()
            .unwrap()
            .pane
            .settings_owned()
            .map(|(owner, page)| (owner, page.clone()));
        let Some((owner, page)) = owned else {
            let ui = self.ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade() {
                    ui.set_in_settings(false);
                }
            });
            return;
        };

        // The sleep timer of the player whose page this is, which is also
        // where pressing the row sends the next rung. Read out of `browsing`
        // above first, so it is not still held when `with_entry` takes
        // `registry`.
        let sleep = self
            .with_entry(owner, |e| e.status.clone())
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
        // The memo covers the rows and only the rows.
        //
        // It used to gate the whole function, which was wrong: this call does
        // two separate things, and only one of them is about the content. It
        // also puts the pane up — `in_settings`, the title, the back arrow —
        // and something else takes the pane down behind its back:
        // [`Self::publish_settings`] clears `in_settings` whenever the pane is
        // no longer a settings page, and knows nothing about this memo. So the
        // second press of the format badge on one track built the identical
        // page, matched the memo, skipped everything, and left the pane down.
        // The press did nothing at all, and went on doing nothing for the rest
        // of the session.
        //
        // What this skips is the model replacement and the property writes —
        // which is the expensive half, because replacing a model re-creates
        // every row instance and restarts its transitions. It does not skip
        // building the rows: they are already built by the time the
        // fingerprint can be taken over them, so a tick that changes nothing
        // still pays for the walk. Moving the memo above that would mean
        // fingerprinting the settings document instead of the rows, which is
        // a larger change than it looks — the rows also carry state from
        // outside the document, the sleep timer among it.
        let unchanged = already_sent(&self.sent_settings, {
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            title.hash(&mut hash);
            for row in &rows {
                row.index.hash(&mut hash);
                row.label.hash(&mut hash);
                row.detail.hash(&mut hash);
                row.control.hash(&mut hash);
                row.value.hash(&mut hash);
                row.on.hash(&mut hash);
                row.heading.hash(&mut hash);
                row.available.hash(&mut hash);
                row.options.hash(&mut hash);
                row.option_index.hash(&mut hash);
                // The numbers a slider carries, which change as it is dragged.
                row.number.to_bits().hash(&mut hash);
            }
            hash.finish()
        });

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            if unchanged {
                // The rows on screen are already these. Put the pane up anyway
                // — that is the part the caller is always asking for.
                ui.set_in_settings(true);
                ui.set_browse_title(title.into());
                ui.set_browse_can_go_back(true);
                return;
            }
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
                    opens: row.opens,
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
    /// The identity permutation unless Customize Home has been used on this
    /// screen, so a screen nobody has rearranged costs one map lookup.
    fn arrangement(&self, screen: &Screen) -> Vec<usize> {
        let shown = |at: &usize| {
            screen
                .sections
                .get(*at)
                .is_none_or(|section| !order::is_hidden(section.id.as_deref()))
        };
        let plain = || (0..screen.sections.len()).filter(shown).collect::<Vec<_>>();

        let Some(id) = screen.id.as_deref() else {
            return plain();
        };

        let wanted = {
            let orders = self.orders.lock().unwrap();
            orders.get(id).cloned()
        };
        // A default where nothing has been arranged, so Home leads with what
        // was last played rather than with whatever the player listed first.
        // Customize Home overrides it the moment it is used.
        let wanted = wanted.unwrap_or_else(|| order::default_for(id));
        if wanted.is_empty() {
            return plain();
        }

        let ids: Vec<Option<String>> = screen.sections.iter().map(|s| s.id.clone()).collect();
        let pinned: Vec<bool> = screen.sections.iter().map(|s| s.no_reorder).collect();
        order::arrange(&ids, &pinned, &wanted)
            .into_iter()
            .filter(shown)
            .collect()
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

            // Whatever order Customize Home last put these in. `ordinal` stays
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
                    (1, Some(title)) => american(title),
                    (0, Some(title)) if listed > 1 => american(title),
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
                                // Prose about the app, not the name of
                                // anything on the system; see `is_chrome_row`.
                                american(item.label().unwrap_or_default()),
                                american(
                                    item.extra.get("subText").map(String::as_str).unwrap_or(""),
                                ),
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

                    // What the row is called, for the purpose of choosing an
                    // icon for it.
                    //
                    // Not always the item's own label: Home's Most Used shelf
                    // writes Radio Paradise and TuneIn as `<source>` elements
                    // with no title at all, and the name comes from the action
                    // instead — which is where the caption gets it too. With
                    // only the label to go on there was nothing to match and
                    // both kept their logos.
                    let named = item.label().unwrap_or_else(|| {
                        item.action
                            .as_ref()
                            .and_then(|a| a.title.as_deref())
                            .unwrap_or_default()
                    });

                    // A service picker is a row of names in the app's own
                    // chrome, so it gets the app's own icons; everywhere else
                    // the player's picture wins when it is content.
                    let glyph = if kind == 2 {
                        Some(glyphs::service_glyph(named))
                    } else if item
                        .action
                        .as_ref()
                        .is_some_and(bluos::Action::is_browse_menu)
                    {
                        Some(glyphs::menu_glyph(named))
                    } else {
                        glyphs::glyph_for(named, source)
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
                        // Resolved the way `activate` resolves it — its own
                        // action first, the play action only as a fallback —
                        // so this says what pressing *this* row would do and
                        // not what the row is capable of.
                        plays: item
                            .action
                            .as_ref()
                            .or(item.play_action.as_ref())
                            .and_then(|action| action.uri.as_deref())
                            .is_some_and(bluos::screen::starts_playing),
                        track: item.extra.get("track").cloned().unwrap_or_default(),
                        quality: quality_label(item.quality.as_deref().unwrap_or_default()),
                        index: at,
                        action: String::new(),
                        // Some rows are an icon and nothing else — the "add a
                        // preset" tile is one — so fall back to what the
                        // action calls itself, and then to nothing rather than
                        // to a placeholder dash.
                        title: {
                            let named = item
                                .label()
                                .or_else(|| item.action.as_ref().and_then(|a| a.title.as_deref()))
                                .unwrap_or_default();
                            if is_chrome_row(item.kind) {
                                american(named)
                            } else {
                                named.to_owned()
                            }
                        },
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
                            title: american(title),
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
                // The heading only. A screen's subtitle is the artist on an
                // album page — content, not chrome.
                (Some(heading), Some(sub)) => format!("{} — {sub}", american(heading)),
                (Some(heading), None) => american(heading),
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

        let mut blocks = blocks;
        {
            // Before `blocks` is handed over and consumed. Worth doing here
            // rather than on demand because these are the rows as flattened for
            // the window, headings and all — working them out a second time
            // somewhere else is how the letters and the rows drift apart.
            let mut browsing = self.browsing.lock().unwrap();
            // Whether the window is looking at a slice of something longer.
            // Home and End mean a fetch on one of these and a scroll on a list
            // held whole, and only the window knows how tall its own pane is.
            let paged = browsing.current().is_some_and(holds_a_window);
            let ui = self.ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade() {
                    ui.set_browse_paged(paged);
                }
            });
            browsing.derived = match browsing.current() {
                Some(screen) if !holds_a_window(screen) => {
                    collate(&mut blocks);
                    derived_index(&blocks)
                }
                // A list the player indexed uses the player's own offsets, and
                // one still arriving cannot be indexed from the part in hand:
                // the letters would stop wherever the paging had got to. Nor
                // can the last window of one, which has nothing left to page
                // on but is still thirty rows of two thousand.
                _ => Vec::new(),
            };
        }

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
        self.send_browse_index();

        // A new screen starts at the top. Replacing a ListView's model does not
        // move the Flickable — the same fact the jump reset exists for — and
        // nothing else was putting it back, so walking into a row from halfway
        // down a list opened the next screen halfway down as well.
        //
        // On the era rather than on every publish. This runs again for a status
        // change or an arriving thumbnail, and resetting there would drag the
        // list out from under anyone reading it; a page arriving as the list is
        // scrolled must not do it either, and neither changes the era.
        let era = self.browsing.lock().unwrap().era;
        if !already_sent(&self.sent_era, era) {
            self.scroll_browse_to(0.0);
        }
    }

    /// The letters a long list can be jumped to.
    ///
    /// Its own call rather than a ninth argument to `send_browse`, which
    /// already carries eight. The keys go to the window and the offsets stay
    /// here: a jump is a request, and the window has no business building one.
    fn send_browse_index(&self) {
        let keys: Vec<String> = {
            let browsing = self.browsing.lock().unwrap();
            // The player's own where there is one, the rows' where there is
            // not. The window is not told which: a letter is a letter, and
            // what happens when it is pressed is settled here.
            match browsing.current() {
                // No screen means no letters, whatever was worked out for the
                // last one — its own arm rather than an empty index falling
                // through, or a bar outlives the list it was built from.
                None => Vec::new(),
                Some(screen) if !screen.index.is_empty() => {
                    bar_keys(screen.index.iter().map(|j| j.key.clone()).collect())
                }
                Some(_) => bar_keys(
                    browsing
                        .derived
                        .iter()
                        .map(|(key, _)| key.clone())
                        .collect(),
                ),
            }
        };

        if already_sent(&self.sent_index, {
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            keys.hash(&mut hash);
            hash.finish()
        }) {
            return;
        }

        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_browse_index(ModelRc::new(VecModel::from(
                    keys.into_iter()
                        .map(slint::SharedString::from)
                        .collect::<Vec<_>>(),
                )));
            }
        });
    }

    /// Put a letter of a list held whole at the top of the pane.
    ///
    /// The same event as a fetched jump — the window is told the list has moved
    /// and where to — so the two kinds of jump land through one path. Zero is
    /// what a fetched jump sends, its rows having been replaced.
    fn scroll_browse_to(&self, y: f32) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_browse_jump_y(y);
                ui.set_browse_jumped(ui.get_browse_jumped().wrapping_add(1));
            }
        });
    }

    /// Put the bottom of the rows held at the bottom of the pane.
    ///
    /// What End does to a list held whole, sent from here for a fetched last
    /// window: the window alone knows how tall the rows came out, so it is told
    /// to go to the end rather than handed a number.
    fn scroll_browse_to_end(&self) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_browse_to_end(ui.get_browse_to_end().wrapping_add(1));
            }
        });
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
        // Everything drawn goes in, and the artwork as whether it has arrived
        // — the same rule the queue's memo follows, for the same reason: a
        // decoded cover is more expensive to compare than the redraw it would
        // save, and its arrival is the only thing about it that changes.
        if already_sent(&self.sent_browse, {
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            title.hash(&mut hash);
            can_go_back.hash(&mut hash);
            search.hash(&mut hash);
            recent.hash(&mut hash);
            empty.as_ref().map(|(a, b, g)| (a, b, *g)).hash(&mut hash);
            for row in &selector {
                browse_row_fingerprint(row, &mut hash);
            }
            for block in &blocks {
                block.kind.hash(&mut hash);
                block.title.hash(&mut hash);
                block.action.hash(&mut hash);
                block.section.hash(&mut hash);
                for row in &block.rows {
                    browse_row_fingerprint(row, &mut hash);
                }
            }
            if let Some(header) = &header {
                header.title.hash(&mut hash);
                header.subtitle.hash(&mut hash);
                header.detail.hash(&mut hash);
                header.cover.is_some().hash(&mut hash);
                header.buttons.hash(&mut hash);
            }
            hash.finish()
        }) {
            // The rows on screen are already these — but the title and the
            // back arrow may not be, and this memo cannot see that. It compares
            // against what *browse* last sent, and another pane can overwrite
            // those two properties behind its back: walk from My Stations into
            // Now Playing and close it, and the browse screen comes back under
            // a heading that still says My Stations, with a back arrow that
            // goes nowhere.
            //
            // The same trap `send_settings` documents, in the other publisher.
            // What the memo is worth skipping is the model replacement, which
            // re-creates every row; two property writes are not worth being
            // clever about.
            let ui = self.ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui.upgrade() else { return };
                ui.set_browse_title(title.into());
                ui.set_browse_can_go_back(can_go_back);
            });
            return;
        }

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
                                plays: row.plays,
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
                        // A service picker chooses which service a screen is
                        // about; it never starts anything.
                        plays: false,
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
                        // The player puts the one it means first and colors it
                        // its own accent. The color is dropped, the order is
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

        // What the service offers for this track, beyond the transport.
        //
        // Availability is `url.is_some()` rather than `Status::can`. That
        // helper falls back to "anything not a stream can do it", which is
        // right for skip and back and wrong for these: a local file is
        // queue-based, so the fallback would put a Love button on a track no
        // service will ever hear about. An action listed without a URL is the
        // player declaring it unavailable, and that is the whole signal here.
        //
        // Skip and back are the transport's, and the interval variants of them
        // are the fifteen-second nudges a podcast wants — transport territory
        // too, and no document showing their shape has been seen here, so they
        // are left out rather than guessed at.
        let track_actions = snapshot
            .as_ref()
            .map(|(status, _)| track_actions(status))
            .unwrap_or_default();

        // The player writes its own phrase for this — "MP3 128 kb/s" for a
        // stream, "FLAC 24/44.1" for a library track — and it says what the
        // badge cannot: the codec, and the depth and rate behind a tier. Too
        // long for the badge, so it goes on the line below it, where Now
        // Playing has the room.
        let stream_format = snapshot
            .as_ref()
            .and_then(|(status, _)| status.stream_format.as_deref())
            .unwrap_or_default()
            .trim()
            .to_owned();

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
            ui.set_stream_format(stream_format.as_str().into());
            ui.set_track_actions(ModelRc::new(VecModel::from(
                track_actions
                    .into_iter()
                    .map(|action| TrackAction {
                        name: action.name.into(),
                        label: action.label.into(),
                        on: action.on,
                    })
                    .collect::<Vec<_>>(),
            )));
        });
    }

    /// Put cover art in the now-playing panel, or clear it.
    ///
    /// The tint travels with it: a color taken from the artwork, which the
    /// panel washes behind everything at low opacity so the room the music is
    /// in picks up the color of the record. Without artwork there is no
    /// color, and the panel is its ordinary self.
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
    /// Whether the row opens something as well as carrying its control.
    opens: bool,
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
                        // A door now that there is a screen behind it.
                        Kind::Alarms => "link",
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
                    opens: false,
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
            opens: false,
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
/// One change to the alarm being edited.
#[derive(Debug, Clone)]
enum AlarmField {
    Start(u8, u8),
    End(u8, u8),
    /// Whether it runs to a finishing time rather than for a length.
    Schedule(bool),
    Day(usize),
    /// One of the durations the official controller offers, in minutes.
    Duration(u32),
    Volume(u32),
    FadeIn(bool),
    Shuffle(bool),
    Backup(bool),
}

enum Chosen {
    /// Another page, by id.
    Page(String),
    /// A page the player will not describe; hand it to a browser.
    Web(String),
    /// A value to write.
    Write(Box<bluos::settings::Setting>, String),
    /// The alarms screen, which is this app's own rather than a page the
    /// player describes.
    Alarms,
    /// One more step along the sleep timer's ladder. Not a write: the setting
    /// carries no value and no options, because the player owns the ladder and
    /// hands out the next rung on request.
    Sleep,
}

/// Find what row `index` of a settings page refers to.
///
/// Walks the page the same way `settings_rows` draws it, so the two agree
/// about what the numbers mean.
///
/// A setting the player has said it will not take right now — one whose
/// `dependsOn` is not met, drawn dimmed — means nothing. The row refuses the
/// press too, but it is not the only way here: the keyboard's Enter arrives as
/// the same command, and a dimmed pill used to let its click fall through to
/// the row beneath it. So the page decides, and it is the same test the row's
/// `available` was drawn from.
fn pick(page: &SettingsPage, index: usize) -> Option<Chosen> {
    fn walk(
        page: &SettingsPage,
        entries: &[SettingEntry],
        at: &mut usize,
        want: usize,
    ) -> Option<Chosen> {
        for entry in entries {
            match entry {
                SettingEntry::Group(group) if group.is_page_link() => {
                    if *at == want {
                        return Some(Chosen::Page(group.id.clone()));
                    }
                    *at += 1;
                }
                SettingEntry::Group(group) => {
                    if let Some(found) = walk(page, &group.entries, at, want) {
                        return Some(found);
                    }
                }
                SettingEntry::Setting(setting) => {
                    if *at == want {
                        if !page.is_available(setting) {
                            return None;
                        }
                        if let Some(url) = &setting.webview {
                            return Some(Chosen::Web(url.clone()));
                        }
                        return match setting.kind {
                            Kind::Boolean => setting
                                .toggled()
                                .map(|value| Chosen::Write(setting.clone(), value)),
                            Kind::Sleep => Some(Chosen::Sleep),
                            Kind::Alarms => Some(Chosen::Alarms),
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
    walk(page, &page.entries, &mut at, index)
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
    // `presets` is here because the player serves a whole screen for it —
    // rows that play, each with a context menu offering Edit and Delete — and
    // the hardware buttons it names are on the remote in the room. It sits
    // after Home for the same reason the official controller puts it there.
    const IN_SIDEBAR: &[&str] = &["home", "presets", "favourites", "search"];

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
        // The id on the left is the player's and stays as it spells it; the
        // words on the right are this app's own and do not have to.
        "favourites" => "Favorites".to_owned(),
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
///
/// The id comes back decoded. It was read off a URI, where it is still
/// percent-encoded, and `Client::settings` encodes whatever it is handed —
/// as it has to, since the other callers pass an id straight from an
/// attribute. Passing it on as it stood would encode it twice.
fn settings_page(uri: &str) -> Option<Option<String>> {
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    if !path.eq_ignore_ascii_case("/Settings") {
        return None;
    }
    let mut url = reqwest::Url::parse("http://player/").expect("a constant URL parses");
    url.set_query(Some(query));
    Some(
        url.query_pairs()
            .find(|(key, _)| key == "id")
            .map(|(_, value)| value.into_owned()),
    )
}

/// Broadcast for players and adopt whatever answers.
///
/// A sweep is a schedule of broadcasts spread over twelve seconds rather than
/// one query and an answer, because a single UDP broadcast is dropped often
/// enough to matter and a player that was asleep takes a moment to reply at
/// all.
async fn sweep(backend: Backend, discovery: Arc<Discovery>, http: reqwest::Client) {
    // Adopted as each answers rather than when the whole schedule has run: a
    // player that replies to the first broadcast used to wait out the other
    // eleven seconds before appearing, which on a first run is the whole of
    // what the window has to show.
    let _ = discovery
        .sweep_with(DEFAULT_SWEEP, |announce| backend.adopt(announce, &http))
        .await;
}

/// Fetch a screen and show it.
///
/// `push` distinguishes going deeper from starting over: Back pops the trail,
/// so every screen that is arrived at by following a row has to be on it.
async fn open_screen(backend: Backend, id: DeviceId, uri: String, arrive: Arrive) {
    // Which selection this is for, taken before the request goes out. A
    // screen of a player that is not selected is never shown — see
    // `selections` for what that did — and neither is one that comes back
    // after the choice has changed, even to this player and back.
    let Some((_, selection)) = backend.selection().filter(|(selected, _)| *selected == id) else {
        return;
    };
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return;
    };

    match client.screen(&uri).await {
        Ok(mut screen) => {
            // A list that counts rather than points — a library's Songs, 2062
            // of them — gives a total and no cursor. Work one out here, once,
            // so everything downstream sees a screen that knows where the rest
            // of itself is and nothing else has to care which kind it was.
            if screen.next.is_none() {
                screen.next = bluos::screen::continuation(&uri, &screen);
            }
            {
                let mut browsing = backend.browsing.lock().unwrap();

                // Another player chosen while this was on its way. Checked
                // with the trail locked, so the choice cannot change between
                // this and the write below.
                if !backend.still_selected(selection) {
                    return;
                }

                // Decided before anything is changed. A refresh that lost its
                // race changes nothing at all — taking the pane from Settings
                // and then dropping the screen would leave the window showing
                // a pane nobody published.
                if let Arrive::Refresh { was, .. } = &arrive
                    && !browsing.trail.last().is_some_and(|c| &c.uri == was)
                {
                    return;
                }

                // A Home is only asked for while this player's screens are not
                // the ones showing — see `BrowseHome` — so finding them up now
                // means one the user pressed for landed first. That screen is
                // what was asked for last, and Home would replace it.
                if arrive == Arrive::Home
                    && browsing.device == Some(id)
                    && !browsing.trail.is_empty()
                {
                    return;
                }

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
                    // Still the screen it was started for; checked above.
                    Arrive::Refresh { query, .. } => {
                        browsing.trail.pop();
                        query.clone()
                    }
                    Arrive::Root => {
                        browsing.trail.clear();
                        None
                    }
                    Arrive::Home => {
                        browsing.trail.clear();
                        // An entry of this player's lit since its Home was
                        // asked for is a press still on its way, or an input
                        // that shows nothing; either says more than Home.
                        if !browsing.highlighted.contains_key(&id) {
                            browsing.light(id, (0, 0));
                        }
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
                let page = screen
                    .paged()
                    .map_or_else(|| screen.items().count(), |section| section.items.len())
                    as u32;
                browsing.trail.push(Crumb {
                    uri,
                    screen,
                    query,
                    page,
                });
                browsing.moved_on();
            }
            // The whole pane, not only the browse rows. Opening a screen is
            // also leaving whichever pane was covering them, and the window
            // keeps drawing the settings rows until it is told otherwise —
            // which showed as a screen with the right title and the wrong list
            // under it.
            backend.publish_pane();
            if arrive == Arrive::Home {
                backend.publish_sidebar();
            }
            tokio::spawn(load_browse_thumbnails(backend, id, 0));
        }
        Err(e) => tracing::warn!(%id, "could not read {uri}: {e}"),
    }
}

/// The service a picker's action names, out of its query.
///
/// By convention the parameter is `C…Service`: `CfavouritesService` on the
/// Favourites screen. Matched by the suffix rather than the whole name, since
/// only that half is the same from screen to screen.
fn service_named(uri: &str) -> Option<String> {
    let (_, query) = uri.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.ends_with("Service") && !value.is_empty()).then(|| value.to_owned())
    })
}

/// `uri` with `service` set, replacing one already there.
fn with_service(uri: &str, service: &str) -> String {
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    let mut kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("service="))
        .collect();
    let service = format!("service={service}");
    kept.push(&service);
    format!("{path}?{}", kept.join("&"))
}

/// The item a published row index refers to, and the section it came from.
///
/// The rows are published in [`Backend::arrangement`] order, with hidden
/// sections dropped — so counting through `screen.items()` finds a different
/// item than the one drawn. That is not a hypothetical: with Home's promo
/// shelf hidden and Recently Played lifted to the top, pressing the first
/// tile ran an action from the shelf that is not on screen, and the app
/// reported a route it had never been asked to open.
fn item_at<'a>(
    screen: &'a Screen,
    arrangement: &[usize],
    index: usize,
) -> Option<(&'a bluos::screen::Section, &'a bluos::screen::Item)> {
    let mut at = 0usize;
    for section in arrangement.iter().filter_map(|s| screen.sections.get(*s)) {
        for item in &section.items {
            if at == index {
                return Some((section, item));
            }
            at += 1;
        }
    }
    None
}

/// Save a preset, unless the player already has one holding that stream.
///
/// Nothing on the player refuses a duplicate: saving the same station twice
/// puts it in two slots, and there are only so many. So the list is read
/// first, which is the only thing `/Presets` is needed for — every other
/// preset route is handed its subject rather than having to look it up.
///
/// A list that cannot be read is not a reason to refuse: the save is what was
/// asked for, and a duplicate is a smaller problem than a button that does
/// nothing when the player is briefly busy.
async fn save_preset_once(backend: &Backend, client: &Client, preset: &bluos::screen::Preset) {
    let Some(url) = preset.url.as_deref() else {
        return;
    };

    if let Ok(held) = client.presets().await
        && let Some(already) = held.holding(url)
    {
        say(
            &backend.ui,
            format!(
                "{} is already preset {}",
                bluos::presets::Presets::label(already),
                already.id
            ),
        );
        return;
    }

    let named = if preset.name.is_empty() {
        "it".to_owned()
    } else {
        preset.name.clone()
    };
    match client.save_preset(preset).await {
        Ok(()) => say(&backend.ui, format!("Saved {named} as a preset")),
        Err(e) => say(&backend.ui, format!("could not save it: {e}")),
    }
}

/// The cursor a screen should carry once a page has arrived.
///
/// `None` stops the paging. Two ways a list can say it is finished without
/// saying so, and both are loops once anything but a scroll can ask for the
/// next page:
///
/// - a page that brought no rows, still carrying a cursor. There is nothing
///   further to graft and asking again gets the same nothing.
/// - a page whose cursor is the one that was just asked for. That is the
///   player repeating itself, and following it walks the same page forever.
///
/// Neither showed while only scrolling could ask, because a person stops
/// scrolling. The height trigger does not get bored.
fn next_after_page(asked: &str, arrived: Option<String>, brought: usize) -> Option<String> {
    if brought == 0 {
        return None;
    }
    match arrived {
        Some(cursor) if cursor == asked => None,
        other => other,
    }
}

/// Put a page of a long list's rows into the list, on the screen it belongs to.
///
/// The list is the section the count describes, which need not be the last
/// one: with a shelf after the songs, a page put into the last section grew
/// the shelf, and a jump replaced it and left the songs where they were. A
/// jump replaces the window it moved away from; paging on adds to it.
fn put_page(screen: &mut Screen, rows: Vec<bluos::screen::Item>, replace: bool) {
    let Some(section) = screen.window().and_then(|at| screen.sections.get_mut(at)) else {
        return;
    };
    if replace {
        section.items = rows;
    } else {
        section.items.extend(rows);
    }
}

/// Whether a screen holds only a window on a longer list, so that Home and End
/// are fetches rather than scrolls.
///
/// A cursor or an index says so, and so does a count that runs past the rows
/// in hand. The count is the one that matters after End: the last window of a
/// counted list has nothing left to page on, and one with no index to fall
/// back on then read as held whole. Home scrolled to the top of those thirty
/// rows instead of fetching the first, and nothing moved the view out of them.
fn holds_a_window(screen: &Screen) -> bool {
    // The counted section's rows, measured as `continuation` measures them.
    let held = screen
        .paged()
        .map_or_else(|| screen.items().count(), |section| section.items.len())
        as u32;
    !screen.index.is_empty()
        || screen.next.is_some()
        || screen.offset.is_some_and(|offset| offset > 0)
        || screen.total.is_some_and(|total| total > held)
}

/// Where the window Home or End asks for starts, on a paged list.
///
/// One page back from the end, not everything held back from it: after
/// scrolling through three pages the second measure lands sixty rows short of
/// the last. Counted lists only for End. A list that hands out a cursor says
/// nothing about how long it is, so its end cannot be aimed at — paging
/// forward is the only way there.
fn end_offset(crumb: &Crumb, to_end: bool) -> Option<u32> {
    match crumb.screen.total {
        Some(total) if to_end => Some(total.saturating_sub(crumb.page.max(1))),
        _ if !to_end => Some(0),
        _ => None,
    }
}

/// Do whatever row `index` of the current screen says to do.
async fn activate(backend: Backend, index: usize) {
    // The selection out first; see the note on `Backend`.
    let selection = backend.selection();
    let (id, action, named, arrive, worth_keeping, switch_to) = {
        let browsing = backend.browsing.lock().unwrap();
        let Some(id) = backend.browsing_selected(&browsing, selection) else {
            let showing = browsing.device.is_some();
            drop(browsing);
            if showing {
                refuse_other_players_screen(&backend);
            }
            return;
        };
        let Some(crumb) = browsing.trail.last() else {
            return;
        };

        // Which section the row came from decides how its screen arrives. A
        // service picker asks to replace the screen it sits on — switching
        // from Library to TuneIn is the same screen about a different service,
        // not a step into one.
        let arrangement = backend.arrangement(&crumb.screen);
        let Some((section, item)) = item_at(&crumb.screen, &arrangement, index) else {
            return;
        };
        let picker = section.kind == SectionKind::SelectorMenu;
        let replaces = picker && section.replace_screen;

        // Choosing a service out of a picker is done on the URL, not by the
        // action attached to it.
        //
        // The Favorites picker offers `<action type="player-link"
        // URI="/ui/action?CfavouritesService=TuneIn" refreshScreen="true">`,
        // and on a Powernode that call answers 200 and changes nothing: the
        // refresh that follows fetches the same address and gets Library back,
        // so pressing TuneIn did nothing at all. `/ui/Favourites?service=TuneIn`
        // does answer with TuneIn — and does not stick either, so the choice
        // has to stay on the address this app keeps for the screen.
        let switch_to = picker
            .then(|| item.action.as_ref().and_then(|a| a.uri.as_deref()))
            .flatten()
            .and_then(service_named)
            .map(|service| with_service(&crumb.uri, &service));

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
            item.label().unwrap_or("that input").to_owned(),
            arrive,
            worth_keeping,
            switch_to,
        )
    };

    if let Some(query) = worth_keeping {
        remember_search(&backend, query);
    }
    // The picker's own address, where it has one: the same screen asked for a
    // different service. Replaces rather than pushes, because it is not a step
    // into anything — Back from TuneIn's favorites should leave Favorites,
    // not return to Library's.
    if let Some(uri) = switch_to {
        open_screen(backend, id, uri, Arrive::Replace(None)).await;
        return;
    }

    let Some(action) = action else { return };

    // The same question the sidebar asks. An input reaches this path too —
    // Home draws the sources as a shelf, and Most Used puts the ones you
    // reach for at the top — and warning on one path but not the others made
    // the same press behave differently depending on where it was made.
    if ask_before_input(&backend, id, &action, &named) {
        return;
    }
    run_action(backend, id, action, arrive).await;
}

/// What a form coming up for a player may take the place of.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Replacing {
    /// Any pane of that player's: the settings page or the list of shares or
    /// services the form was chosen on. This is the placeholder going up.
    Page,
    /// That player's form, the one opened as this number, and nothing else.
    /// This is the page arriving for the placeholder, or the next step of a
    /// form that was submitted.
    ///
    /// Stricter than `Page` because it lands a round trip later. The wireless
    /// page takes three and a half seconds, which is time enough to press
    /// Back out of the placeholder, and a form that then came up anyway over
    /// the settings page was a pane nobody had asked for. Time enough, too,
    /// to open another of the same player's forms; see `FormPage::opened`.
    Form(u64),
}

/// Say that a press on the browse pane was not run, because the screen showing
/// is not the selected player's.
///
/// Said rather than silent: the row looks as pressable as any other, and a
/// press that does nothing without a word reads as a broken app.
fn refuse_other_players_screen(backend: &Backend) {
    tracing::debug!("a press on a screen of a player that is not selected");
    say(&backend.ui, "That screen is another player's");
}

/// Take down one opening of a form, putting back the page it was opened from,
/// if that form is still the pane showing. Whether it was.
///
/// For every way off a form that is not Back: a page this crate cannot read,
/// and a submit answered with no next step. Both used to land on Home, which is
/// what `FormPage::from` exists to stop — the WiFi form's scan coming back
/// unreadable closed it three and a half seconds after its row was pressed,
/// and put a Recently Played tile that plays under the pointer. Whatever
/// replaced the form in the meantime, another of this player's forms included,
/// is not the reply's to close.
fn close_form(backend: &Backend, device: DeviceId, opened: u64) -> bool {
    let mut browsing = backend.browsing.lock().unwrap();
    let Pane::Form(page) = &mut browsing.pane else {
        return false;
    };
    if page.device != device || page.opened != opened {
        return false;
    }
    let from = page.from.take();
    browsing.pane = from.map_or(Pane::Browse, |pane| *pane);
    true
}

/// Take down a form the player answered with no next step, and read again the
/// page it goes back to.
///
/// The page put back is the copy taken when the form went up, before the
/// submit, and the submit is what changed it: signed into a service, the
/// Services list came back still offering the sign-in, and the WiFi form left
/// Player settings showing the network it had just replaced. Every other write
/// from those pages re-reads its page, for this reason.
///
/// Only a page of the form's own player, and only the kind that went back up.
/// Anything else showing by the time the lock is taken again is not this
/// reply's to refresh.
fn close_answered_form(backend: &Backend, device: DeviceId, opened: u64) {
    if !close_form(backend, device, opened) {
        return;
    }
    let again = match &backend.browsing.lock().unwrap().pane {
        Pane::Settings(owner, trail) if *owner == device => trail
            .last()
            .map(|page| Command::OpenSettings(page.page_id.clone(), Step::Reload(device))),
        Pane::Web(owner, WebPage::Services(_)) if *owner == device => Some(Command::OpenServices {
            device,
            reload: true,
        }),
        Pane::Web(owner, WebPage::Shares { .. }) if *owner == device => Some(Command::OpenShares {
            device,
            reload: true,
        }),
        _ => None,
    };
    if let Some(again) = again {
        let _ = backend.commands.send(again);
    }
}

/// Show a form, keeping whatever the page arrived filled in with, and give
/// the number it went up as, or `None` if it did not.
///
/// Passwords are the exception: a page comes back with the field empty, and
/// seeding it with anything would be inventing a value nobody typed.
///
/// It goes up only over what `replacing` allows of `device`'s. Anything else
/// on show means the pane it belonged with has gone — another player's Home
/// landed, Back was pressed, another form was opened — and a form put up
/// then is one form over whatever replaced it.
fn show_form(
    backend: &Backend,
    device: DeviceId,
    replacing: Replacing,
    title: String,
    form: bluos::forms::Form,
    note: String,
) -> Option<u64> {
    let values = form_values(&form);

    let opened = {
        let mut browsing = backend.browsing.lock().unwrap();
        let opened = match (&browsing.pane, replacing) {
            (Pane::Form(page), Replacing::Form(opened))
                if page.device == device && page.opened == opened =>
            {
                opened
            }
            (_, Replacing::Form(_)) => return None,
            (pane, Replacing::Page) if pane.owner() == Some(device) => {
                backend.forms.fetch_add(1, Ordering::Relaxed) + 1
            }
            (_, Replacing::Page) => return None,
        };
        // Taken rather than read: the form is replacing it, and putting it in
        // the form is what lets Back give it back. A form replacing a form
        // keeps the page the first one came from.
        let from = match std::mem::replace(&mut browsing.pane, Pane::Browse) {
            pane @ (Pane::Web(..) | Pane::Settings(..)) => Some(Box::new(pane)),
            Pane::Form(page) => page.from,
            _ => None,
        };
        browsing.pane = Pane::Form(Box::new(FormPage {
            device,
            opened,
            title,
            form,
            values,
            note,
            from,
        }));
        opened
    };
    // Through `publish_pane`, not `publish_form`. Every caller of this reaches
    // it after awaiting the player, and the command loop keeps reading its
    // channel while that is in flight — deliberately, so a player that has
    // gone away cannot freeze the window. The wireless form is the worst of
    // them: the player scans for networks first and takes about three and a
    // half seconds, which is long enough to press My Stations in the sidebar
    // and have the form land on top of a pane that is still drawing itself.
    // Only `publish_pane` answers the other panes' flags.
    backend.publish_pane();
    Some(opened)
}

/// What a form's fields hold before anybody touches them, by name.
///
/// A switch holds only whether it is on — `on` or empty, the two states
/// `Edit::Toggle` flips between — and not the value it would send. Its `value`
/// is what a ticked box posts, which a box arrives with whether it is ticked
/// or not; seeded from that, a box with `value="1"` drew itself on when the
/// page had it off. `submit_form` swaps in the page's own value on the way out.
///
/// A set of radios with none checked holds nothing at all, not an empty value.
/// Empty is a value a radio can really carry — `value=""` — so holding the
/// unset group as empty drew it on that radio and posted `name=` for a group
/// nobody had picked from. Absent until something is picked, it cannot be
/// mistaken for a pick.
fn form_values(form: &bluos::forms::Form) -> BTreeMap<String, String> {
    form.fields
        .iter()
        .filter(|field| field.kind != bluos::forms::Kind::Password)
        .filter_map(|field| {
            let value = if field.kind == bluos::forms::Kind::Switch {
                if field.checked { "on" } else { "" }.to_owned()
            } else if field.kind == bluos::forms::Kind::Choice && field.value.is_empty() {
                field
                    .choices
                    .iter()
                    .find(|c| c.selected)
                    .map(|c| c.value.clone())?
            } else {
                field.value.clone()
            };
            Some((field.name.clone(), value))
        })
        .collect()
}

/// Which of a form field's choices its row is drawn on, or -1 for none.
///
/// A radio group with nothing checked holds no value (see `form_values`) and
/// sends nothing (see `form_body`), so it is drawn with nothing chosen.
/// Falling back to the first choice drew the list on `MP3` while the form
/// posted no quality at all. A `<select>` never reaches this: the parser marks
/// its first option selected when the page marks none, which is what a
/// browser sends. The -1 holds on screen because a changed form replaces the
/// whole settings model; the ComboBox only clamps its index when the model
/// under a live row changes, and picking the first choice from -1 still fires
/// `selected`.
fn drawn_choice(field: &bluos::forms::Field, held: Option<&str>) -> i32 {
    held.and_then(|held| field.choices.iter().position(|c| c.value == held))
        .map_or(-1, |at| at as i32)
}

/// Ask about a player's firmware, through its leader if it will not answer.
///
/// Direct is the confirmed route and is always tried first, so nothing that
/// works today is routed differently. Only a player that does not answer at
/// all is asked about through somebody else: a follower whose leader is
/// reachable is asked about with `&slave=`, which is what that parameter is
/// for and is the one case where a player can be beyond reach on its own.
///
/// Returns whichever client answered along with the member parameter that got
/// the answer, so starting an upgrade can repeat exactly the request that the
/// check succeeded with rather than guessing at the route a second time.
///
/// The zone half of this has never run against real hardware — one speaker
/// here, and `&slave=` was read out of the official controller rather than
/// observed. It is reached only after a direct request has already failed, so
/// the worst it can do is fail differently.
async fn ask_about_firmware(
    backend: &Backend,
    id: DeviceId,
) -> bluos::Result<(Client, Option<DeviceId>, bluos::upgrade::Availability)> {
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return Err(bluos::Error::Refused("no such player".to_owned()));
    };

    let direct = client.upgrade_available().await;
    let Err(e) = direct else {
        return direct.map(|ready| (client, None, ready));
    };

    // Only a leader is worth trying, and only one this app can still reach.
    let leader = backend
        .with_entry(id, |entry| entry.sync.as_ref().and_then(|s| s.master_id()))
        .flatten()
        .and_then(|master| backend.with_entry(master, |e| e.client.clone()));

    let Some(leader) = leader else { return Err(e) };

    tracing::debug!(%id, "no answer about firmware; asking its leader instead");
    leader
        .upgrade_available_for(Some(id))
        .await
        .map(|ready| (leader, Some(id), ready))
}

/// Put a query at the top of the recent list, and write the list down.
fn remember_search(backend: &Backend, query: String) {
    // A query the file will not keep does not go in the list either. It would
    // draw a row that is gone at the next start, and take a real one off the
    // end of the list with it on the way.
    if !searches::keepable(&query) {
        return;
    }

    let keeping = {
        let mut browsing = backend.browsing.lock().unwrap();
        browsing.recent.retain(|seen| seen != &query);
        browsing.recent.insert(0, query);
        browsing.recent.truncate(RECENT_SEARCHES);
        browsing.recent.clone()
    };
    // Off the event loop and out from under the lock: this is a file write on
    // the path that runs when a search is committed.
    tokio::task::spawn_blocking(move || searches::save(&keeping));
    backend.publish_browse();
}

/// Search for what has been typed so far.
///
/// Called once the typing has settled rather than on the keystroke itself, and
/// then only if nothing has been typed since. `selection` is the one the
/// typing was done under.
async fn run_search(backend: Backend, query: String, selection: Option<(DeviceId, u64)>) {
    let query = query.trim().to_owned();

    // A search runs on the screen showing, so on the player that served it,
    // and only while that is the one selected: see `browsing_selected`.
    let plan = {
        let browsing = backend.browsing.lock().unwrap();
        // Another player chosen while the typing settled. The search is
        // simply abandoned: the refusal below is for typing on a screen that
        // was not the selected player's, and said now it would read as the
        // choice just made having failed.
        if selection.is_some_and(|(_, n)| !backend.still_selected(n)) {
            return;
        }
        let Some(id) = backend.browsing_selected(&browsing, selection) else {
            let showing = browsing.device.is_some();
            drop(browsing);
            if showing {
                refuse_other_players_screen(&backend);
            }
            return;
        };
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
            {
                let mut browsing = backend.browsing.lock().unwrap();
                browsing.trail.pop();
                browsing.moved_on();
            }
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
    // Which selection this press was made under, if `id` is the player
    // selected at all. What runs here always goes to `id`, whoever is
    // selected; but a pane or a question it puts up is only put up while
    // that is still the same choice, as `open_screen` does for screens.
    // Otherwise one player's page lands over another's name, and a press on
    // it reads as being about the player beside it.
    let selection = backend
        .selection()
        .filter(|(selected, _)| *selected == id)
        .map(|(_, selection)| selection);

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
                            // Read from where the press came from rather
                            // than threaded through: the format badge on the
                            // record opens this with Now Playing up, and a
                            // track's context menu opens the same page from a
                            // browse screen. Back belongs wherever you were.
                            let mut browsing = backend.browsing.lock().unwrap();
                            // With the lock held, for the reason on
                            // `Backend::still_selected`.
                            if !selection.is_some_and(|n| backend.still_selected(n)) {
                                return;
                            }
                            let whence = match browsing.pane {
                                Pane::NowPlaying => Whence::NowPlaying,
                                _ => Whence::Browse,
                            };
                            browsing.pane = Pane::HelpDetail(title, facts, whence, None);
                            drop(browsing);
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
                // Not a screen either: the ingredients for a request the
                // client assembles, once somewhere has been picked to put the
                // track.
                Some("AddToPlaylistOptions") => match client.playlist_options(&uri).await {
                    Ok(options) if !options.is_empty() => {
                        let title = action
                            .title
                            .clone()
                            .unwrap_or_else(|| "Add to playlist".to_owned());
                        {
                            let mut browsing = backend.browsing.lock().unwrap();
                            // The options name a track on `id`, and the list
                            // is drawn under whichever player is selected.
                            // Checked with the lock held, as above.
                            if !selection.is_some_and(|n| backend.still_selected(n)) {
                                return;
                            }
                            browsing.pane = Pane::Playlists(Box::new(PlaylistPage {
                                device: id,
                                opened: backend.openings.fetch_add(1, Ordering::Relaxed) + 1,
                                title,
                                options,
                                naming: false,
                            }));
                        }
                        backend.publish_pane();
                    }
                    other => {
                        if let Err(e) = other {
                            tracing::debug!(%id, "no playlist options: {e}");
                        }
                        say(&backend.ui, "Nowhere to put this track");
                    }
                },

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

            // While Queue Builder Mode is on, anything that would start
            // playing is turned into an append instead. `appending` returns
            // nothing for an action that does not play — favoriting,
            // switching service, clearing — so those run untouched.
            let building = backend.browsing.lock().unwrap().queue_building;
            let (uri, appended) = match building.then(|| bluos::screen::appending(&uri)).flatten() {
                Some(rewritten) => (rewritten, true),
                None => (uri, false),
            };

            match client.follow(&uri).await {
                // The player asked something instead of acting. Put the
                // question up; nothing has happened yet, and pressing one of
                // its buttons is what runs the action it carries.
                Ok(Some(dialog)) => {
                    if let Some(selection) = selection {
                        offer_dialog(&backend, id, selection, dialog);
                    }
                }
                Ok(None) => {
                    // The player has no phrase for this one, because it does
                    // not know the mode exists.
                    if appended {
                        say(&backend.ui, "Added to the end of the queue");
                        fetch_queue(backend.clone(), id).await;
                    }
                    // Even the wording of the confirmation comes from the
                    // player — "Added to favourites" is its phrase, not ours.
                    if let Some(text) = &action.notification {
                        say(&backend.ui, text.clone());
                    }
                    // Favoriting changes what the menu should say next time,
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
                                .as_deref()
                                .map(american)
                                .unwrap_or_else(|| "Customize".to_owned()),
                            rows,
                            presets: None,
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

            // The player gathers what a preset needs and hands it over rather
            // than saving it itself: a station's menu offers
            // `/add-preset?name=…&url=…&image=…`, everything already decided.
            // So this is a decode and a request, with nothing to ask anybody.
            // Dragging the player's presets into a new order. The same page
            // as Customize Home, because it is the same gesture over the same
            // shape of row; only the saving differs.
            //
            // The rows come from the screen already showing rather than from a
            // fresh request: the Presets screen is where this is pressed, its
            // items carry the slot number as `counter`, and re-reading it
            // would only invite the two to disagree.
            // The `+` at the top of the Presets screen, which unlike every
            // other preset route arrives with nothing attached: it is asking
            // for something to be chosen.
            //
            // What is playing is the answer, and the player has already said
            // what that is. `streamUrl` is set whenever it is playing a URL
            // rather than working through the queue — a station, or an input —
            // and that is exactly what a preset holds. `title1` is the
            // station, `title2` and `title3` being the track.
            //
            // The image is left off deliberately. `/Status`'s is the cover of
            // the song playing this minute, and a preset wearing it would be
            // wrong by the next one; the player draws its own placeholder.
            Some("/add-preset") => {
                let playing = backend.with_entry(id, |entry| {
                    entry.status.as_ref().and_then(|status| {
                        let url = status.stream_url.as_deref().filter(|u| !u.is_empty())?;
                        Some(bluos::screen::Preset {
                            name: status.title1.clone().unwrap_or_default(),
                            url: Some(url.to_owned()),
                            image: None,
                        })
                    })
                });

                let Some(Some(preset)) = playing else {
                    say(
                        &backend.ui,
                        "Nothing is playing that can be a preset — open a station \
                         and choose Add preset from its menu",
                    );
                    return;
                };

                save_preset_once(&backend, &client, &preset).await;
                refresh_current(backend).await;
            }

            // Renaming a preset. The player's own menu offers it and hands
            // over everything the slot holds, so this only has to ask for a
            // new name and send the lot back.
            Some(route) if bluos::screen::preset_to_edit(route).is_some() => {
                // Decoded twice, once to choose this arm and once to use it:
                // a match guard cannot hand its answer to the body.
                if let Some((slot, preset)) = bluos::screen::preset_to_edit(route) {
                    {
                        let mut browsing = backend.browsing.lock().unwrap();
                        // The rename goes to `id` whatever happens, but the
                        // field is drawn under the player selected; opened
                        // over another one, it looks like that one's preset.
                        if !selection.is_some_and(|n| backend.still_selected(n)) {
                            return;
                        }
                        browsing.pane = Pane::EditPreset(Box::new(EditPresetPage {
                            device: id,
                            slot,
                            preset,
                        }));
                    }
                    // Through publish_pane, so every "this pane is showing"
                    // flag is answered.
                    backend.publish_pane();
                }
            }

            Some(route) if bluos::screen::presets_to_reorder(route).is_some() => {
                let prid = bluos::screen::presets_to_reorder(route).unwrap_or_default();
                // Put up under the same lock it is checked under. Taken again
                // to write, another player could be chosen in between, and
                // this player's slots went up under that one's name.
                let shown = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    // The slots are read off the screen showing and the moves
                    // go to `id`, so the two have to be the same player's. They
                    // normally are — the button is on that screen — and while
                    // a selection is settling they are not, and one player's
                    // slot numbers would be sent to reorder another's.
                    //
                    // `device` alone is not enough: it stays the last player's
                    // until the next one's Home lands, so it is checked with
                    // the selection this press was made under.
                    let showing = browsing.device == Some(id)
                        && selection.is_some_and(|n| backend.still_selected(n));
                    let page = browsing.current().filter(|_| showing).map(|screen| {
                        let rows: Vec<(String, String)> = screen
                            .items()
                            .filter_map(|item| {
                                let slot = item.extra.get("counter")?.clone();
                                Some((slot, item.label().unwrap_or_default().to_owned()))
                            })
                            .collect();
                        CustomisePage {
                            screen: screen.id.clone().unwrap_or_default(),
                            title: "Reorder Presets".to_owned(),
                            rows,
                            presets: Some((id, prid)),
                        }
                    });
                    match page {
                        // Fewer than two and there is nothing to arrange.
                        Some(page) if page.rows.len() > 1 => {
                            browsing.pane = Pane::Customise(page);
                            true
                        }
                        _ => false,
                    }
                };
                if shown {
                    backend.publish_pane();
                } else {
                    say(&backend.ui, "There are not enough presets to reorder");
                }
            }

            Some(route) if bluos::screen::preset_to_save(route).is_some() => {
                // Decoded twice — once to choose this arm and once to use it —
                // because a match guard cannot hand its answer to the body. It
                // is a string split; the alternative is a nested `if let` in
                // the arm below, which buries the case that matters.
                let preset = bluos::screen::preset_to_save(route).unwrap_or_default();
                save_preset_once(&backend, &client, &preset).await;
                // The shelf and the Presets screen are both the player's, and
                // both may now be out of date.
                if action.refresh_screen {
                    refresh_current(backend).await;
                }
            }

            Some(route) => {
                tracing::debug!(%id, "no equivalent for the route {route}");
                say(
                    &backend.ui,
                    match route {
                        // Handled above when it carries what to save. The
                        // bare form cannot reach here.
                        "/add-preset" => {
                            "Open a station and choose Add preset from its menu".to_owned()
                        }
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
                // Customize button on the Inputs row leads to
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
                    let _ = backend.commands.send(Command::OpenServices {
                        device: id,
                        reload: false,
                    });
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

    // Asked once, here, for the same reason the address is written down here:
    // this is the moment an address is known to be a player. Off on its own
    // task because the check is the slowest request the player answers — the
    // player goes and asks somebody else — and nothing about starting up
    // should wait on it.
    if backend.told_about_update.lock().unwrap().insert(id) {
        let backend = backend.clone();
        tokio::spawn(async move {
            // Logged either way. Whether an offer appears is otherwise
            // invisible from outside, and "it did not ask me" and "it asked
            // and the player said no" look identical without this.
            match ask_about_firmware(&backend, id).await {
                Ok((_, _, ready)) => {
                    tracing::debug!(
                        %id,
                        available = ready.available,
                        in_progress = ready.in_progress,
                        "asked the player about firmware"
                    );
                    if ready.available && !ready.in_progress {
                        let _ = backend.commands.send(Command::UpdateNotice(id));
                    }
                }
                // Not said out loud: a player that cannot answer this is not
                // worth interrupting anyone over, and the Upgrade Check page
                // reports it properly to anyone who asks.
                Err(e) => tracing::debug!(%id, "could not ask about firmware: {e}"),
            }
        });
    }

    // Remembered only now, not when it was adopted: answering /SyncStatus is
    // what makes an address a player, so a mistyped one is never written down.
    let newly_known = {
        let mut known = backend.known.lock().unwrap();
        known::remember(&mut known, id).then(|| known.clone())
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
    // Read the crumb, do not take it. Popping first and pushing back only on
    // success meant a refetch that failed deleted the screen outright — the
    // pane kept drawing rows nobody could reach, and Back skipped a level —
    // while two refreshes in flight at once swapped two levels around. The
    // shape that works is already in `open_screen`: peek, fetch, and let
    // `Replace` pop and push under one lock once the player has answered.
    //
    // `Replace` and not `Deeper` for a second reason: `Deeper` discards the
    // crumb's query, so refreshing a screen of search results stopped it
    // counting as results and the next search stacked instead of replacing.
    //
    // The caller sets `refreshing` to claim the slot, and this releases it on
    // every path out — including the early return below, which would otherwise
    // leave the flag set and stop the screen ever refreshing again.
    let _done = Refreshing(backend.clone());

    let Some((id, uri, query)) = ({
        let browsing = backend.browsing.lock().unwrap();
        browsing.trail.last().and_then(|crumb| {
            browsing
                .device
                .map(|id| (id, crumb.uri.clone(), crumb.query.clone()))
        })
    }) else {
        return;
    };
    open_screen(
        backend.clone(),
        id,
        uri.clone(),
        Arrive::Refresh { query, was: uri },
    )
    .await;
}

/// Holds the single refresh slot, and gives it back however the refresh ends.
///
/// A plain store at the end of [`refresh_current`] is not enough: the function
/// returns early when there is no crumb to refresh, and a task that is dropped
/// mid-await never reaches its last line at all. Either would strand the flag
/// and leave the screen unable to refresh for the rest of the session.
struct Refreshing(Backend);

impl Drop for Refreshing {
    fn drop(&mut self) {
        self.0
            .refreshing
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Fetch the icons and cover art for the screen on show.
/// Fetch the pictures the browse screen needs.
///
/// `done` is how many items at the front of the screen a previous run already
/// walked. Paging appends to the screen rather than replacing it, so without
/// this the second page re-derived every URL on the first, the third re-derived
/// the first two, and a library page fetched forty times over — each one a
/// cache lookup rather than a request, but the memory cache is bounded, and on
/// a list long enough to page the early entries have been evicted by the time
/// the walk reaches them again. Skipping them is also what stops the header's
/// cover being re-queued ahead of the rows that actually just arrived.
async fn load_browse_thumbnails(backend: Backend, id: DeviceId, done: usize) {
    // Registry first and released, then the browse state: never both at once.
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return;
    };

    // The screen this was started for. Nothing here checked it was still on
    // show, so walking away from a long list left its loader running: it kept
    // its share of the four artwork permits and rebuilt the whole model every
    // 400ms for a screen nobody could see. Its sibling `load_cover` has
    // checked the equivalent from the start.
    let era = backend.browsing.lock().unwrap().era;

    let urls: Vec<(String, u32)> = {
        let browsing = backend.browsing.lock().unwrap();
        let Some(screen) = browsing.current() else {
            return;
        };

        let mut seen = std::collections::BTreeSet::new();
        // The header's cover first: it is the largest thing on the screen and
        // the one the eye lands on, so it should not queue behind forty rows.
        // Only on the first walk — it has not changed since.
        let header = screen
            .header
            .as_ref()
            .filter(|_| done == 0)
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
                    // Before the filter, so it counts items and not pictures:
                    // the caller knows how many items it had, not how many of
                    // them turned out to want one.
                    .skip(done)
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
        if backend.browsing.lock().unwrap().era != era {
            return;
        }
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
/// Fetch this player's art at the size its sidebar row draws it.
///
/// Separate from [`load_cover`] and deliberately simpler. That one settles for
/// a moment before drawing, because switching input walks the art through
/// three values in about a second and the hero is 560px of it — a flicker
/// there is the whole screen changing twice. A 40px row can take the flicker,
/// and waiting would mean the sidebar lagged the thing it is describing.
///
/// Failure is silence: a row with no art draws the speaker glyph it always
/// drew, which is a complete state rather than a gap.
async fn load_card_cover(backend: Backend, id: DeviceId) {
    let wanted = backend.with_entry(id, |e| e.cover_url.clone()).flatten();

    let pixels = match &wanted {
        Some(url) => backend.artwork.get(url, CARD_SIZE).await,
        None => None,
    };

    // Checked against what the player wants *now*, not what it wanted when
    // this started: a fetch that lost a race to a track change would otherwise
    // put the previous record back on the row.
    let still_wanted = backend.with_entry(id, |e| e.cover_url.clone()).flatten();
    if still_wanted != wanted {
        return;
    }

    let changed = {
        let mut registry = backend.registry.lock().unwrap();
        match registry.get_mut(&id) {
            Some(entry) => {
                let was = entry.card_cover.is_some();
                entry.card_cover = pixels;
                was != entry.card_cover.is_some()
            }
            None => false,
        }
    };

    // Only when the row's appearance actually changed. The players model is
    // memo-guarded, so an unconditional publish would be a rebuild per fetch
    // that the memo then throws away.
    if changed {
        backend.publish();
    }
}

async fn load_cover(backend: Backend, id: DeviceId) {
    let wanted = backend.with_entry(id, |e| e.cover_url.clone()).flatten();

    // Let the artwork settle before drawing any of it.
    //
    // Switching input walks it through three values in about a second: the
    // plain icon named in the `/Play` command, the now-playing variant of the
    // same icon seventy milliseconds after that, and then whatever the player
    // lands on. Measured on a Powernode — the middle one arrives so soon after
    // the first that drawing both is a flicker rather than a change, and the
    // first is not usually cached, so it costs a blank frame on the way in too.
    //
    // Anything superseded inside this window never reaches the window at all:
    // a newer value has its own `load_cover`, and this one finds the address
    // has moved on and gives up.
    tokio::time::sleep(COVER_SETTLE).await;
    if !backend.is_selected(id) {
        return;
    }
    if backend.with_entry(id, |e| e.cover_url.clone()).flatten() != wanted {
        return;
    }

    // What is already decoded, before anything is fetched. A cover that has
    // been seen — every input's icon, after the first time — arrives with no
    // gap at all.
    let ready = wanted
        .as_deref()
        .and_then(|url| backend.artwork.cached(url, COVER_SIZE));

    if ready.is_some() {
        if !backend.is_selected(id) {
            return;
        }
        let tint = wanted.as_deref().and_then(|url| backend.artwork.tint(url));
        backend.set_cover(ready, tint);
        return;
    }

    // Nothing to draw yet, so stop drawing the last thing. Left alone, the
    // previous track's sleeve sat in the corner until the fetch returned —
    // switching to Bluetooth showed the record that had been playing, which
    // reads as the wrong icon rather than as a picture still loading.
    if backend.is_selected(id) {
        backend.set_cover(None, None);
    }

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

/// The bar's catch-all, for a letter that is neither a digit nor Latin.
///
/// One entry however many scripts are behind it. A library with Chinese,
/// Japanese and Korean titles produces a letter per character, and thirty of
/// those down the side of the window is not a bar any more — it is a second
/// list, taller than the pane, in front of the first.
const OTHER_KEY: &str = "*";

/// The two row heights are `BrowseRowView`'s and have to stay in step with it.
/// Measured rather than assumed: a twenty-row list reported a viewport of
/// 1040px, so the pitch is exactly 52 with no spacing or padding between rows.
/// If that component's height changes, the letters drift further from their
/// rows the further down the list you go.
const ROW_PX: f32 = 52.0;
const HEADING_PX: f32 = 30.0;

/// How many rows are worth a bar. Below this the list fits on a screen or two
/// and the letters would be furniture: the folder root is seven entries.
const WORTH_INDEXING: usize = 40;

/// Whether a letter stands on the bar as itself.
fn has_own_letter(key: &str) -> bool {
    key == "#" || (key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic())
}

/// The bar as drawn: every letter that is one, then a single `*` for the rest.
///
/// The rest keep their offsets — only the drawing is collapsed, and pressing
/// `*` goes to the first of them. Applied to the player's own index as well as
/// to a worked-out one, because the player has the same problem: its Composers
/// index ends in two CJK characters, and would end in fifty for a library of
/// mostly CJK titles.
fn bar_keys(keys: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in keys {
        if has_own_letter(&key) {
            out.push(key);
        } else if out.last().map(String::as_str) != Some(OTHER_KEY) {
            out.push(OTHER_KEY.to_owned());
        }
    }
    out
}

/// Every place a letter of a worked-out bar can take you, in list order.
///
/// Exact match first, because a click sends a key straight off the bar. Then
/// case-insensitively, so typing `e` reaches `E`. `*` gathers every letter
/// behind it, since the bar drew them as one.
fn stops_derived(index: &[(String, Vec<f32>)], key: &str) -> Vec<f32> {
    if key == OTHER_KEY {
        return index
            .iter()
            .filter(|(k, _)| !has_own_letter(k))
            .flat_map(|(_, stops)| stops.iter().copied())
            .collect();
    }
    index
        .iter()
        .find(|(k, _)| k == key)
        .or_else(|| index.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)))
        .map(|(_, stops)| stops.clone())
        .unwrap_or_default()
}

/// The same, for the index the player sent: one offset per letter, except the
/// catch-all, which stands for as many as the bar collapsed into it.
fn stops_sent(index: &[bluos::screen::Jump], key: &str) -> Vec<u32> {
    if key == OTHER_KEY {
        return index
            .iter()
            .filter(|jump| !has_own_letter(&jump.key))
            .map(|jump| jump.offset)
            .collect();
    }
    index
        .iter()
        .find(|jump| jump.key == key)
        .or_else(|| index.iter().find(|jump| jump.key.eq_ignore_ascii_case(key)))
        .map(|jump| vec![jump.offset])
        .unwrap_or_default()
}

/// How many rows would have to be taken out for the letters to run in order.
///
/// The number of rows less the longest run already in letter order — so a
/// sorted list answers zero, a sorted list with three stragglers on the end
/// answers three, and a shuffled one answers most of its length. O(n log n).
///
/// This is the question [`collate`] needs, because sorting the rows cannot
/// change the answer to it: it is asked once, of the rows as the player sent
/// them.
fn out_of_place(rows: &[BrowseData]) -> usize {
    let keys: Vec<String> = rows
        .iter()
        .filter(|row| !row.heading)
        .map(|row| leading_key(&row.title))
        .collect();

    // Patience sorting: `tails[i]` is the smallest letter that can end a run of
    // length i+1. Non-decreasing rather than increasing, because a letter
    // repeats for every row filed under it.
    let mut tails: Vec<&str> = Vec::new();
    for key in &keys {
        let at = tails.partition_point(|end| *end <= key.as_str());
        if at == tails.len() {
            tails.push(key);
        } else {
            tails[at] = key;
        }
    }
    keys.len() - tails.len()
}

/// Put the rows the player filed by character under the letter they read as.
///
/// The player sorts these lists by raw character code, so "classical" lands
/// after "Vocaloid" and "Électronique" after that — outside the C and E they
/// plainly belong to. Nothing else moves: the rows are stably sorted by the
/// letter they file under, and a list whose letters are already in order is
/// unchanged by that, so the only rows that travel are the stragglers, each
/// joining the bottom of its own letter.
///
/// The rows carry their own `index` into the screen, and pressing one looks the
/// item up by that rather than by where it sits, so moving a row does not
/// change what it plays.
fn collate(blocks: &mut [BlockData]) {
    if blocks.len() != 1 || blocks[0].kind != 0 {
        return;
    }
    let rows = &mut blocks[0].rows;
    if rows.len() < WORTH_INDEXING {
        return;
    }
    // A heading belongs where the player put it and is not a row that can be
    // filed under a letter. A list with any heading is left alone rather than
    // sorted around them: the lists this is for do not have them, and a rule
    // with an exception in it is a rule waiting to be got wrong.
    if rows.iter().any(|row| row.heading) {
        return;
    }
    // Deliberately not "does `letters` succeed". That was the first version and
    // it could never fire: `letters` refuses a list whose stragglers fall
    // outside the letters already seen, which is precisely the list this exists
    // to repair — the guard was the damage. A library with "quiet storm" and no
    // uppercase Q got no bar at all rather than a Q in the right place.
    //
    // Asked of the rows in a way sorting them cannot invalidate instead. A
    // handful out of place means an alphabetical list with codepoint stragglers
    // on the end. A tenth of them means something that was never alphabetical —
    // an album's tracks are in track order — and it is left exactly as sent.
    if out_of_place(rows) * 10 > rows.len() {
        return;
    }
    // Stable, so rows sharing a letter keep the order the player gave them.
    rows.sort_by_key(|row| leading_key(&row.title));
}

/// Letters for a list the player did not index, with where each one starts.
///
/// Only for a list that is entirely in hand — the caller checks that. The
/// offsets are pixels down the viewport, because scrolling is what a jump does
/// here: there is nothing to fetch.
fn derived_index(blocks: &[BlockData]) -> Vec<(String, Vec<f32>)> {
    // Only a plain list. A screen of shelves and tiles has no single column to
    // scroll a letter to, and mixing kinds would put the offsets out anyway
    // because a tile is not `ROW_PX` tall.
    if blocks.len() != 1 || blocks[0].kind != 0 {
        return Vec::new();
    }
    let rows = &blocks[0].rows;
    if rows.len() < WORTH_INDEXING {
        return Vec::new();
    }

    let index = letters(rows).unwrap_or_default();

    // Three letters is a bar; one is a decoration.
    if index.len() < 3 { Vec::new() } else { index }
}

/// The letters of a list, or `None` if the rows are not in letter order.
///
/// Each letter carries **every** place it starts, not just the first, so a
/// straggler [`collate`] could not move — a list with a heading in it, say — is
/// still reachable by pressing its letter again.
///
/// Refused outright when the first appearances do not ascend, or when repeats
/// are more than a quarter of the runs. Both mean the list is not alphabetical,
/// and a bar on an unsorted list is worse than none: every letter would be a
/// cycle through arbitrary rows.
fn letters(rows: &[BrowseData]) -> Option<Vec<(String, Vec<f32>)>> {
    let mut index: Vec<(String, Vec<f32>)> = Vec::new();
    let mut last: Option<String> = None;
    let mut runs = 0usize;
    let mut repeats = 0usize;
    let mut y = 0.0;

    for row in rows {
        // A heading is not an entry, but it does take space.
        if !row.heading {
            let key = leading_key(&row.title);
            if last.as_deref() != Some(key.as_str()) {
                runs += 1;
                match index.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, stops)) => {
                        stops.push(y);
                        repeats += 1;
                    }
                    None => index.push((key.clone(), vec![y])),
                }
                last = Some(key);
            }
        }
        y += if row.heading { HEADING_PX } else { ROW_PX };
    }

    if !index.windows(2).all(|pair| pair[0].0 < pair[1].0) {
        return None;
    }
    if repeats * 4 > runs {
        return None;
    }
    Some(index)
}

/// The letter a row files under.
///
/// `#` for anything not alphabetic, which is what the player itself puts at the
/// head of its indexes — its Artists list starts `# A B C`. Accents are folded
/// away and case with them, so Sigur Rós, Motörhead and "the xx" file under R,
/// M and T rather than under letters of their own at the end of the bar.
///
/// A script with neither case nor a Latin base keeps itself here and is
/// collected into one entry by [`bar_keys`].
fn leading_key(title: &str) -> String {
    match title.chars().find(|c| !c.is_whitespace()) {
        Some(c) if c.is_alphabetic() => base_letter(c).to_uppercase().to_string(),
        _ => "#".to_owned(),
    }
}

/// A Latin letter with its accent taken off, or the character unchanged.
///
/// Covers U+00C0..=U+024F — Latin-1 Supplement through Latin Extended-B, which
/// is every accented letter a Western or Central European name uses. Generated
/// from Unicode's own decompositions rather than typed out by hand, with the
/// handful that do not decompose (Æ, Ø, Ł, ß and their like) mapped to the
/// letter they are alphabetized under.
///
/// Anything outside that range — Greek, Cyrillic, the CJK scripts — is left
/// alone, having no Latin letter to be filed under.
fn base_letter(c: char) -> char {
    const LATIN_BASE: [char; 400] = [
        'A', 'A', 'A', 'A', 'A', 'A', 'A', 'C', 'E', 'E', 'E', 'E', 'I', 'I', 'I', 'I', 'D', 'N',
        'O', 'O', 'O', 'O', 'O', '×', 'O', 'U', 'U', 'U', 'U', 'Y', 'T', 's', 'a', 'a', 'a', 'a',
        'a', 'a', 'a', 'c', 'e', 'e', 'e', 'e', 'i', 'i', 'i', 'i', 'd', 'n', 'o', 'o', 'o', 'o',
        'o', '÷', 'o', 'u', 'u', 'u', 'u', 'y', 't', 'y', 'A', 'a', 'A', 'a', 'A', 'a', 'C', 'c',
        'C', 'c', 'C', 'c', 'C', 'c', 'D', 'd', 'D', 'd', 'E', 'e', 'E', 'e', 'E', 'e', 'E', 'e',
        'E', 'e', 'G', 'g', 'G', 'g', 'G', 'g', 'G', 'g', 'H', 'h', 'H', 'h', 'I', 'i', 'I', 'i',
        'I', 'i', 'I', 'i', 'I', 'i', 'Ĳ', 'ĳ', 'J', 'j', 'K', 'k', 'ĸ', 'L', 'l', 'L', 'l', 'L',
        'l', 'Ŀ', 'ŀ', 'L', 'l', 'N', 'n', 'N', 'n', 'N', 'n', 'ŉ', 'Ŋ', 'ŋ', 'O', 'o', 'O', 'o',
        'O', 'o', 'O', 'o', 'R', 'r', 'R', 'r', 'R', 'r', 'S', 's', 'S', 's', 'S', 's', 'S', 's',
        'T', 't', 'T', 't', 'T', 't', 'U', 'u', 'U', 'u', 'U', 'u', 'U', 'u', 'U', 'u', 'U', 'u',
        'W', 'w', 'Y', 'y', 'Y', 'Z', 'z', 'Z', 'z', 'Z', 'z', 'ſ', 'ƀ', 'Ɓ', 'Ƃ', 'ƃ', 'Ƅ', 'ƅ',
        'Ɔ', 'Ƈ', 'ƈ', 'Ɖ', 'Ɗ', 'Ƌ', 'ƌ', 'ƍ', 'Ǝ', 'Ə', 'Ɛ', 'Ƒ', 'ƒ', 'Ɠ', 'Ɣ', 'ƕ', 'Ɩ', 'Ɨ',
        'Ƙ', 'ƙ', 'ƚ', 'ƛ', 'Ɯ', 'Ɲ', 'ƞ', 'Ɵ', 'O', 'o', 'Ƣ', 'ƣ', 'Ƥ', 'ƥ', 'Ʀ', 'Ƨ', 'ƨ', 'Ʃ',
        'ƪ', 'ƫ', 'Ƭ', 'ƭ', 'Ʈ', 'U', 'u', 'Ʊ', 'Ʋ', 'Ƴ', 'ƴ', 'Ƶ', 'ƶ', 'Ʒ', 'Ƹ', 'ƹ', 'ƺ', 'ƻ',
        'Ƽ', 'ƽ', 'ƾ', 'ƿ', 'ǀ', 'ǁ', 'ǂ', 'ǃ', 'Ǆ', 'ǅ', 'ǆ', 'Ǉ', 'ǈ', 'ǉ', 'Ǌ', 'ǋ', 'ǌ', 'A',
        'a', 'I', 'i', 'O', 'o', 'U', 'u', 'U', 'u', 'U', 'u', 'U', 'u', 'U', 'u', 'ǝ', 'A', 'a',
        'A', 'a', 'Æ', 'æ', 'Ǥ', 'ǥ', 'G', 'g', 'K', 'k', 'O', 'o', 'O', 'o', 'Ʒ', 'ʒ', 'j', 'Ǳ',
        'ǲ', 'ǳ', 'G', 'g', 'Ƕ', 'Ƿ', 'N', 'n', 'A', 'a', 'Æ', 'æ', 'Ø', 'ø', 'A', 'a', 'A', 'a',
        'E', 'e', 'E', 'e', 'I', 'i', 'I', 'i', 'O', 'o', 'O', 'o', 'R', 'r', 'R', 'r', 'U', 'u',
        'U', 'u', 'S', 's', 'T', 't', 'Ȝ', 'ȝ', 'H', 'h', 'Ƞ', 'ȡ', 'Ȣ', 'ȣ', 'Ȥ', 'ȥ', 'A', 'a',
        'E', 'e', 'O', 'o', 'O', 'o', 'O', 'o', 'O', 'o', 'Y', 'y', 'ȴ', 'ȵ', 'ȶ', 'ȷ', 'ȸ', 'ȹ',
        'Ⱥ', 'Ȼ', 'ȼ', 'Ƚ', 'Ⱦ', 'ȿ', 'ɀ', 'Ɂ', 'ɂ', 'Ƀ', 'Ʉ', 'Ʌ', 'Ɇ', 'ɇ', 'Ɉ', 'ɉ', 'Ɋ', 'ɋ',
        'Ɍ', 'ɍ', 'Ɏ', 'ɏ',
    ];

    let point = c as u32;
    match point.checked_sub(0xC0) {
        Some(at) if (at as usize) < LATIN_BASE.len() => LATIN_BASE[at as usize],
        _ => c,
    }
}

/// Put the window a jump fetched in place of the one on screen.
///
/// `false` when it no longer belongs there, or brought nothing, and the screen
/// is as it was.
fn land_jump(browsing: &mut Browsing, mut page: Screen, offset: u32, era: u64, jump: u64) -> bool {
    // The screen may have gone while the request was out, exactly as a
    // page of it may — and another jump may have overtaken this one.
    if browsing.era != era || browsing.jumps != jump {
        return false;
    }
    let Some(crumb) = browsing.trail.last_mut() else {
        return false;
    };

    // The list's rows, into the list. The page is the whole screen again,
    // and the section last on the crumb need not be the list: with a shelf
    // after the songs, the jump replaced the shelf and left the songs.
    let arriving = page.take_page();
    if arriving.is_empty() {
        return false;
    }
    let brought = arriving.len();
    put_page(&mut crumb.screen, arriving, true);

    // Where the window now starts, which is what tells `continuation` where
    // the next page begins. Without this the list would page from zero
    // again and repeat everything up to the letter.
    crumb.screen.offset = page.offset.or(Some(offset));
    // Cleared first: `continuation` refuses to build one for a screen that
    // already has a cursor, and the cursor still on the crumb is the old
    // window's. Leaving it in place made the fallback return None for the
    // one list kind it exists for — a library's Songs, which counts rather
    // than points — and the list was then stuck with nothing to page on.
    crumb.screen.next = None;
    crumb.screen.next = page
        .next
        .or_else(|| bluos::screen::continuation(&crumb.uri, &crumb.screen));
    // A continuation carries the index too, but an empty one is not a
    // reason to throw away the letters already on screen.
    if !page.index.is_empty() {
        crumb.screen.index = page.index;
    }

    // Claimed again now the window has changed. A page asked for while this
    // jump was out took the count as it stood after the jump claimed it, so
    // the check a page makes on landing passed, and the old window's next
    // thirty rows went on after the new window's last: songs 60 to 89 below
    // song 2061, with a cursor that went on paging from there.
    browsing.jumps += 1;
    tracing::debug!(offset, brought, "jumped the list");
    true
}

/// Fetch the page a letter starts on and put it on screen.
///
/// One request rather than paging up to the letter: a page is thirty rows
/// whatever is asked for, a fetch at any offset takes about thirty
/// milliseconds, and the alternative is seventy requests to reach the end of
/// the Songs list.
///
/// `to_end` is whether the view lands on the last of the new rows rather than
/// the first: End wants to see where the list stops, a letter or Home where it
/// starts.
async fn jump_to_letter(
    backend: Backend,
    client: Client,
    uri: String,
    offset: u32,
    era: u64,
    jump: u64,
    to_end: bool,
) {
    let asked = bluos::screen::jump_to(&uri, offset);
    let page = match client.screen(&asked).await {
        Ok(page) => page,
        Err(e) => {
            tracing::debug!("could not jump to {offset}: {e}");
            return;
        }
    };

    if !land_jump(
        &mut backend.browsing.lock().unwrap(),
        page,
        offset,
        era,
        jump,
    ) {
        return;
    }

    backend.publish_browse();
    // Then move the view onto them, after the rows so it moves over the new
    // ones. Replacing a ListView's model leaves the Flickable where it was,
    // and a list is only asked to grow when its scroll or its height changes.
    // Thirty rows swapped for thirty from a view already at the bottom changes
    // neither, so the window sat on the old window's last rows with nothing
    // left to scroll and never asked for the next page: after End, Home and M
    // both stuck there until PageUp moved it.
    if to_end {
        backend.scroll_browse_to_end();
    } else {
        backend.scroll_browse_to(0.0);
    }
    // The rows are all new, so the artwork walk starts at the top of them.
    if let Some(id) = backend.browsing.lock().unwrap().device {
        tokio::spawn(load_browse_thumbnails(backend.clone(), id, 0));
    }
}

/// Whether a window that has arrived still belongs on the rows already held.
///
/// The request goes out off the command loop, so anything can happen while it
/// is in flight. Getting this wrong is not a cosmetic problem: grafting a
/// window of one playlist onto the rows of another interleaves two lists, and
/// every row below the join then plays something other than what it says.
///
/// `pid` is the queue this page was asked about. A queue is replaced whole and
/// takes a new id when it is, so a page whose id does not match on *both*
/// sides — the rows held now and the page just read — is about a list that no
/// longer exists.
fn page_belongs(
    held_id: Option<u32>,
    held: usize,
    pid: Option<u32>,
    page_id: Option<u32>,
    page: usize,
    from: u32,
) -> bool {
    // A queue with no id cannot be told from another one with no id, so it is
    // never grown: the player sends one for every queue worth paging.
    if pid.is_none() || held_id != pid || page_id != pid {
        return false;
    }
    // Something else grew the list while this was out, so this page either
    // duplicates rows or leaves a hole. Either way it is not the next one.
    if held as u32 != from {
        return false;
    }
    // Nothing more to give. Grafting it would be harmless and asking again
    // would not, so this is the answer that stops the scroll re-asking.
    page != 0
}

/// Read the next window of the queue and graft it onto what is already held.
///
/// `from` is how many rows are held, which is also the index of the first one
/// that is not — the player numbers from zero and `queue_range` is inclusive
/// at both ends.
async fn fetch_more_queue(backend: Backend, id: DeviceId, from: u32, pid: Option<u32>) {
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        backend.browsing.lock().unwrap().fetching_queue = false;
        return;
    };

    let read = client.queue_range(from, from + QUEUE_WINDOW - 1).await;
    // Cleared before anything else can return, so a failure does not wedge the
    // list into never asking again.
    backend.browsing.lock().unwrap().fetching_queue = false;

    let more = match read {
        Ok(more) => more,
        Err(e) => {
            tracing::debug!(%id, "could not read more of the queue: {e}");
            return;
        }
    };

    let grew = {
        let mut guard = backend.registry.lock().unwrap();
        let Some(entry) = guard.get_mut(&id) else {
            return;
        };
        let Some(queue) = entry.queue.as_mut() else {
            return;
        };

        if !page_belongs(
            queue.id,
            queue.songs.len(),
            pid,
            more.id,
            more.songs.len(),
            from,
        ) {
            tracing::debug!(%id, "dropping a queue page that no longer fits");
            return;
        }

        queue.songs.extend(more.songs);
        // The player's own count wins: tracks can be added or removed between
        // one window and the next, and the header reads from this.
        queue.length = more.length;
        queue.songs.len()
    };

    tracing::debug!(%id, tracks = grew, "grew the queue");
    backend.publish_queue();
    // Walks the whole list and skips what the cache already holds, so the
    // rows that just arrived are the only ones that cost anything.
    tokio::spawn(load_thumbnails(backend, id));
}

/// Read the queue's own document, for the buttons the player puts under it.
///
/// Separate from the rows, which come from `/Playlist`: that endpoint pages and
/// this one names the actions. Failing is not worth reporting — the queue still
/// draws, just without its buttons.
async fn fetch_queue_buttons(backend: Backend, id: DeviceId) {
    // Only the selected player's queue is drawn, so only its buttons are
    // worth asking for.
    let Some((selected, selection)) = backend.selection() else {
        return;
    };
    if selected != id {
        return;
    }
    let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
        return;
    };
    // The address is this player's or it is not used. Not yet configured
    // means its configuration is on its way, and that asks again when it
    // lands; never configured means there is no address to read.
    let uri = {
        let browsing = backend.browsing.lock().unwrap();
        if browsing.configured != Some(id) {
            return;
        }
        browsing
            .queue_uri
            .clone()
            .unwrap_or_else(|| "/ui/Queue".to_owned())
    };

    match client.screen(&uri).await {
        Ok(screen) => {
            {
                let mut browsing = backend.browsing.lock().unwrap();
                // Another player chosen while this was on its way: the queue
                // on screen is that one's now.
                if !backend.still_selected(selection) {
                    return;
                }
                browsing.queue_screen = Some((id, screen));
            }
            backend.publish_queue();
        }
        Err(e) => tracing::debug!(%id, "no queue document: {e}"),
    }
}

/// Keep one player's row, its queue and its MPRIS object current for as long as
/// the app runs.
///
/// `mpris_index` and the name read below are both for the bridge, so off Linux
/// they are gathered and not used. Kept rather than gated so the signature and
/// the body read the same on every platform; the caller should not have to
/// know which one it is compiling for.
#[cfg_attr(
    not(target_os = "linux"),
    allow(unused_variables, unused_assignments, unused_mut)
)]
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
    #[cfg(target_os = "linux")]
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
                if art_changed {
                    // Every player gets the small one: the sidebar draws a
                    // cover per row, so art matters for the rooms nobody is
                    // looking at as much as for the one they are.
                    tokio::spawn(load_card_cover(backend.clone(), id));

                    if backend.is_selected(id) {
                        tokio::spawn(load_cover(backend.clone(), id));
                    }
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
                // Only one at a time: see `refreshing`. A refresh that loses
                // this race is dropped rather than queued, because the one in
                // flight is fetching the same screen.
                if stale
                    && !backend
                        .refreshing
                        .swap(true, std::sync::atomic::Ordering::AcqRel)
                {
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
                // Keyed on the player the pane belongs to, which is whose
                // status that row shows.
                if matches!(
                    backend.browsing.lock().unwrap().pane,
                    Pane::Settings(owner, _) if owner == id
                ) {
                    backend.publish_settings();
                }

                if backend.is_selected(id) {
                    backend.publish_transport();
                }

                #[cfg(target_os = "linux")]
                {
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
                            backend.artwork.clone(),
                        )
                        .await;
                    }
                    if let Some(bridge) = &mpris {
                        bridge.publish(&status).await;
                    }
                }
            }
            Err(e) => {
                // Two different failures wearing one shape. A player that
                // cannot be reached is offline and the app should say so; a
                // player that answered with a document this crate cannot read
                // is perfectly alive, and it does emit one on occasion while
                // it changes input. Taking the whole app offline over that —
                // graying the transport and writing "Not responding" under
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
                    #[cfg(target_os = "linux")]
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
    discovery: Option<Arc<Discovery>>,
    http: reqwest::Client,
) {
    while let Some(command) = commands.recv().await {
        let (id, action) = match command {
            Command::UpdateNotice(id) => {
                // One at a time: a second offer while one is up would replace
                // it, and the person would answer a question they never saw.
                if backend.update_offer.lock().unwrap().is_some() {
                    continue;
                }
                let name = backend
                    .with_entry(id, |e| e.view.name.to_string())
                    .unwrap_or_else(|| "a player".to_owned());
                *backend.update_offer.lock().unwrap() = Some(id);

                let ui = backend.ui.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui.upgrade() {
                        ui.set_update_notice(name.into());
                    }
                });
                continue;
            }

            Command::UpdateNoticeAnswer(show) => {
                let Some(id) = backend.update_offer.lock().unwrap().take() else {
                    continue;
                };
                if !show {
                    continue;
                }
                // Select it first: the page reads the selection, and the offer
                // may be about a player other than the one being looked at.
                let _ = backend.commands.send(Command::Select(id));
                let _ = backend.commands.send(Command::OpenHelp);
                // Found rather than written down: these rows are addressed by
                // their position in HELP_ENTRIES, and a constant would rot the
                // first time an entry is added above this one.
                if let Some(row) = HELP_ENTRIES
                    .iter()
                    .position(|(_, kind, _, _)| *kind == HelpKind::Upgrade)
                {
                    let _ = backend.commands.send(Command::HelpAction(row));
                }
                continue;
            }

            // Only puts the question up. Nothing reaches the player here.
            Command::UpgradeAsk(id) => {
                // The page stays up when another player is chosen. Its facts
                // and its offer are still this player's, and a question
                // naming the player selected instead would be asking about an
                // update nobody checked for.
                if *backend.selected.lock().unwrap() != Some(id) {
                    say(
                        &backend.ui,
                        "That check was for another player — check this one again",
                    );
                    continue;
                }
                // Belt and braces with `start_upgrade`, which refuses a second
                // upgrade on its own. This is the half that keeps the question
                // from being asked at all, since answering yes to something
                // that will be refused is a worse experience than not being
                // asked.
                if backend.with_entry(id, |e| e.upgrading).unwrap_or(false) {
                    say(&backend.ui, "That player is already installing an update");
                    continue;
                }
                let name = backend
                    .with_entry(id, |e| e.view.name.to_string())
                    .unwrap_or_else(|| "this player".to_owned());
                backend.browsing.lock().unwrap().upgrade_asked = Some(id);
                let ui = backend.ui.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui.upgrade() {
                        ui.set_confirm_upgrade(name.into());
                    }
                });
                continue;
            }

            Command::UpgradeAnswer(go) => {
                // Taken whatever the answer, so a yes can only ever answer
                // the question that was put up.
                let asked = backend.browsing.lock().unwrap().upgrade_asked.take();
                let Some(id) = asked.filter(|_| go) else {
                    continue;
                };
                // The question is modal, but the selection can still move
                // under it — an update notice selects the player it is about.
                if *backend.selected.lock().unwrap() != Some(id) {
                    say(
                        &backend.ui,
                        "Another player was chosen — nothing was updated",
                    );
                    continue;
                }
                let backend = backend.clone();
                tokio::spawn(async move {
                    // Checked again before anything is sent: the answer that
                    // put the offer on screen may be minutes old, and the
                    // player may have started one of its own in between. The
                    // route is settled by that check, so the start repeats
                    // whatever request actually got an answer. What it says is
                    // not read here: the start asks the same question over the
                    // same route and refuses a player with nothing to install
                    // or an install already running.
                    let (client, member) = match ask_about_firmware(&backend, id).await {
                        Ok((client, member, _)) => (client, member),
                        Err(e) => {
                            say(&backend.ui, format!("could not start the update: {e}"));
                            return;
                        }
                    };
                    match client.start_upgrade_for(member).await {
                        Ok(()) => {
                            say(&backend.ui, "Update started — leave the player powered on");
                            follow_upgrade(backend.clone(), id).await;
                        }
                        Err(e) => say(&backend.ui, format!("could not start the update: {e}")),
                    }
                });
                continue;
            }

            Command::Rescan => {
                // Nothing to sweep with where the port could not be bound, and
                // saying so beats a button that looks like it did something.
                let Some(discovery) = discovery.clone() else {
                    say(
                        &backend.ui,
                        "discovery is not running — add players by address",
                    );
                    continue;
                };
                // Spawned rather than awaited: a sweep runs for twelve seconds
                // and this loop is what carries every other command.
                tokio::spawn(sweep(backend.clone(), discovery, http.clone()));
                continue;
            }
            Command::Select(id) => {
                let already = {
                    let mut selected = backend.selected.lock().unwrap();
                    let already = *selected == Some(id);
                    *selected = Some(id);
                    // Before `fresh` is read below, and that order is what
                    // closes the race `selections` is for: a write for the
                    // last player either lands before this bump, and `fresh`
                    // then sees it and fetches this player's own over it, or
                    // lands after and is dropped.
                    //
                    // That holds only because `fresh` looks at everything
                    // such a write changes. `BrowseHome` writes twice, a round
                    // trip apart — the configuration and sidebar, then Home —
                    // and a choice made between the two left the sidebar and
                    // the addresses beside it the other player's for good
                    // while `fresh` looked only at the screens.
                    if !already {
                        backend.selections.fetch_add(1, Ordering::SeqCst);
                    }
                    already
                };
                // An Install offered for one player is drawn only while that
                // player is selected (see `publish_help`), and the page itself
                // does not change with the choice, so it is drawn again here.
                if !already
                    && matches!(
                        backend.browsing.lock().unwrap().pane,
                        Pane::HelpDetail(_, _, _, Some(_))
                    )
                {
                    backend.publish_pane();
                }
                // Show whatever is already known straight away, and only go to
                // the network when this is a player whose queue is not held.
                backend.publish_queue();
                backend.publish_transport();
                tokio::spawn(load_cover(backend.clone(), id));
                // Warm the browser for this player, so the tab is not a wait.
                let fresh = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing.device != Some(id)
                        || browsing.configured != Some(id)
                        || browsing.trail.is_empty()
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
                let Some((id, selection)) = backend.selection() else {
                    continue;
                };
                // Already showing this player's screens, with its sidebar and
                // addresses beside them: leave the trail where the user left
                // it rather than resetting to the root.
                //
                // All three, not only the screens. The configuration is
                // written a round trip before Home, so another player's can
                // be the one held under this player's trail — see the note on
                // the bump in `Select` — and skipping then kept it for good.
                {
                    let browsing = backend.browsing.lock().unwrap();
                    if browsing.device == Some(id)
                        && browsing.configured == Some(id)
                        && !browsing.trail.is_empty()
                    {
                        drop(browsing);
                        backend.publish_pane();
                        continue;
                    }
                }

                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };

                // Off the loop from here. Two round trips follow, and awaiting
                // them here stopped the loop reading its channel at all: on a
                // player that has gone away that is twenty seconds of a window
                // that still paints and answers nothing, once per press.
                //
                // Nothing below has to run in order with anything else — it
                // ends by taking the locks it needs — so it spawns, the way
                // `Rescan` and the transport commands already do.
                let backend = backend.clone();
                tokio::spawn(async move {
                    // The player says which screens it has; the app does not have a
                    // list of its own.
                    //
                    // And without that answer there is nothing to write. This
                    // used to carry on with an empty one, which replaced the
                    // sidebar and the queue and search addresses with nothing
                    // while the last player's screens stayed up — and nothing
                    // put them back, because choosing that player again finds
                    // its trail still showing and fetches nothing. Leaving it
                    // all as it was keeps a consistent window, and this player
                    // is asked again the next time it is chosen.
                    //
                    // Numbered as it is asked, for the note on
                    // `Browsing::configurations`.
                    let asked = {
                        let mut browsing = backend.browsing.lock().unwrap();
                        browsing.configurations += 1;
                        browsing.configurations
                    };
                    let config = match client.ui_configuration().await {
                        Ok(config) => config,
                        Err(e) => {
                            tracing::warn!(%id, "could not read its configuration: {e}");
                            return;
                        }
                    };
                    let screens = user_screens(&config);
                    let root = screens
                        .first()
                        .map(|(_, uri)| uri.clone())
                        .unwrap_or_else(|| "/ui/Sources".to_owned());

                    // The sidebar's lower half is the Sources screen's own rows,
                    // so it is read once here rather than every time the sidebar
                    // is redrawn.
                    let sources_uri = config.uri("sources").unwrap_or("/ui/Sources").to_owned();
                    let sources = client.screen(&sources_uri).await.ok();

                    let keeping_trail = {
                        let mut browsing = backend.browsing.lock().unwrap();
                        // Another player chosen while these were on their way:
                        // this is its sidebar and its addresses now, whether or
                        // not its own replies have arrived yet.
                        if !backend.still_selected(selection) {
                            return;
                        }
                        // This player's screens are already the ones showing,
                        // and only what sits beside them was somebody else's:
                        // chosen, another player's configuration landed, and
                        // this one was chosen back before that player's Home.
                        // Putting the sidebar right is the whole repair; the
                        // trail is where the user left it and stays there.
                        let keeping_trail =
                            browsing.device == Some(id) && !browsing.trail.is_empty();
                        browsing.queue_uri = config.uri("queue").map(str::to_owned);
                        browsing.configured = Some(id);
                        browsing.now_playing_menu =
                            config.uri("nowPlayingContextMenu").map(str::to_owned);
                        browsing.queue_menu_uri =
                            config.uri("queueItemContextMenu").map(str::to_owned);
                        browsing.search_uri = config.uri("search").map(str::to_owned);
                        browsing.screens = screens;
                        browsing.sources = sources;
                        // What is lit is left alone: it describes the trail
                        // showing, and that is still this player's or still
                        // the last one's, which `Browsing::lit` keeps off
                        // this sidebar. Home lights its own entry when it
                        // lands; see `Arrive::Home`.
                        //
                        // Except an entry of this player's own when its
                        // trail is not the one showing: left from an earlier
                        // visit, it would light a screen nobody is looking
                        // at, and Home would take it for a press made since.
                        //
                        // And only an entry lit before this configuration was
                        // asked for. Two can be out at once — a sidebar press
                        // refused before the first landed asks again — and the
                        // second reply took the press made between the two for
                        // a leftover, so Home lit itself over Favourites. Who
                        // was configured could not tell them apart either: a
                        // player chosen away and back is still configured, and
                        // its old entry was kept the same way.
                        let left_over = browsing
                            .highlighted
                            .get(&id)
                            .is_some_and(|(_, lit_at)| *lit_at < asked);
                        if !keeping_trail && left_over {
                            browsing.highlighted.remove(&id);
                        }
                        keeping_trail
                    };
                    backend.publish_sidebar();
                    // The queue's buttons are read from the address just
                    // written, and a queue that arrived before it had none to
                    // read them from.
                    tokio::spawn(fetch_queue_buttons(backend.clone(), id));
                    if !keeping_trail {
                        open_screen(backend.clone(), id, root, Arrive::Home).await;
                    }
                });
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
                        // The picker is a page over the editor, so it is the
                        // first thing Back takes away — one level at a time,
                        // the way it was walked in.
                        Pane::Alarms(page) if !page.picking.is_empty() => {
                            page.picking.pop();
                            true
                        }
                        // Out of the editor before out of the screen: the
                        // editor is a page over the list, not beside it.
                        Pane::Alarms(page) if page.editing.is_some() => {
                            page.editing = None;
                            true
                        }
                        Pane::Alarms(_) => {
                            browsing.pane = Pane::Browse;
                            true
                        }
                        // One level: out of editing before out of the page,
                        // so Back leaves the mode rather than carrying it over
                        // to whenever the page is next opened, with a bin on
                        // every row waiting for a press meant for playing.
                        Pane::Stations(page) if page.editing => {
                            page.editing = false;
                            true
                        }
                        Pane::Stations(_) => {
                            browsing.pane = Pane::Browse;
                            true
                        }
                        Pane::EditPreset(_) => {
                            browsing.pane = Pane::Browse;
                            true
                        }
                        Pane::HelpDetail(_, _, whence, _) => {
                            browsing.pane = match whence {
                                Whence::Help => Pane::Help,
                                Whence::Browse => Pane::Browse,
                                Whence::NowPlaying => Pane::NowPlaying,
                            };
                            true
                        }
                        Pane::Settings(_, trail) if trail.len() > 1 => {
                            trail.pop();
                            true
                        }
                        // Back out of a service's sign-in form lands on the
                        // list of services it was chosen from, and out of the
                        // WiFi form on the settings page it was chosen on.
                        // Placeholder or not: the page is kept from the moment
                        // the form goes up.
                        Pane::Form(page) if page.from.is_some() => {
                            let from = page.from.take();
                            if let Some(pane) = from {
                                browsing.pane = *pane;
                            }
                            true
                        }
                        Pane::Help
                        | Pane::Settings(..)
                        | Pane::Web(..)
                        | Pane::Form(_)
                        | Pane::Customise(_)
                        | Pane::Playlists(_)
                        | Pane::NowPlaying => {
                            browsing.pane = Pane::Browse;
                            true
                        }
                        // Already browsing, so back means the screen before.
                        Pane::Browse => {
                            let deeper = browsing.trail.len() > 1;
                            if deeper {
                                browsing.trail.pop();
                                browsing.moved_on();
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
                // The menu's lines run on the player it was read from, so it
                // is only opened for a row of the selected player's screen:
                // see `Backend::browsing_selected`.
                let selection = backend.selection();
                let opened = {
                    let browsing = backend.browsing.lock().unwrap();
                    let item = browsing.current().and_then(|screen| {
                        let arrangement = backend.arrangement(screen);
                        item_at(screen, &arrangement, index).map(|(_, item)| item.clone())
                    });

                    let Some(id) = backend.browsing_selected(&browsing, selection) else {
                        let showing = browsing.device.is_some();
                        drop(browsing);
                        if showing {
                            refuse_other_players_screen(&backend);
                        }
                        continue;
                    };
                    item.and_then(|item| item.context_menu)
                        .and_then(|action| action.uri)
                        .map(|uri| (id, uri))
                };
                let Some((id, uri)) = opened else { continue };
                // `browsing_selected` said yes, so this is the number it did.
                let Some((_, selection)) = selection else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };

                // The same panel the queue's rows open, rather than a screen
                // pushed onto the browse trail. One gesture, one answer: a
                // menu that replaced the page you were reading was how the
                // queue's dots used to behave, and it read as nothing having
                // happened.
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.screen(&uri).await {
                        Ok(menu) => put_up_menu(&backend, id, selection, menu),
                        Err(e) => {
                            tracing::debug!(%id, "no menu for that row: {e}");
                            say(&backend.ui, "The player offers nothing for that");
                        }
                    }
                });
                continue;
            }

            Command::QueueMenu(song) => {
                let Some((id, selection)) = backend.selection() else {
                    continue;
                };
                // The player names the endpoint in /ui/Configuration; only the
                // queue position is ours to add.
                //
                // Its own configuration's, read with `configured` under one
                // lock, or the default. The address is kept from whichever
                // player was configured last, which is not this one until its
                // configuration lands, and never when that fails.
                let base = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing
                        .queue_menu_uri
                        .clone()
                        .filter(|_| browsing.configured == Some(id))
                }
                .unwrap_or_else(|| "/ui/queueItemCM".to_owned());
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.screen(&format!("{base}?id={song}")).await {
                        Ok(menu) => put_up_menu(&backend, id, selection, menu),
                        Err(e) => {
                            tracing::debug!(%id, "no menu for queue item {song}: {e}");
                            say(&backend.ui, "The player offers nothing for that track");
                        }
                    }
                });
                continue;
            }

            Command::PlaylistPress(index) => {
                let chosen = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let Pane::Playlists(page) = &mut browsing.pane else {
                        continue;
                    };
                    match playlist_at(&page.options, index) {
                        // The line that makes one. It becomes a field rather
                        // than acting, because a new playlist needs a name.
                        Some((_, None)) => {
                            page.naming = true;
                            None
                        }
                        Some((service, Some(playlist))) => Some((
                            page.opened,
                            service,
                            bluos::client::PlaylistTarget::Existing {
                                name: playlist.name,
                                id: playlist.id,
                            },
                        )),
                        None => None,
                    }
                };

                match chosen {
                    Some((opened, service, target)) => {
                        let _ = backend
                            .commands
                            .send(Command::PlaylistAdd(opened, service, target));
                    }
                    None => backend.publish_pane(),
                }
                continue;
            }

            Command::PlaylistNamed(name) => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    continue;
                }
                // The first group that will make one. There is only ever a
                // choice of service when the player offers several, and the
                // field belongs to whichever offered it.
                let (opened, service) = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Pane::Playlists(page) = &browsing.pane else {
                        continue;
                    };
                    (
                        page.opened,
                        page.options
                            .groups
                            .iter()
                            .find(|group| group.can_create)
                            .and_then(|group| group.service.clone()),
                    )
                };
                let _ = backend.commands.send(Command::PlaylistAdd(
                    opened,
                    service,
                    bluos::client::PlaylistTarget::New(name),
                ));
                continue;
            }

            Command::PlaylistAdd(opened, service, target) => {
                // The player the options came from, not whichever is selected
                // now: they describe the track in that player's own terms.
                //
                // And only from the page the choice was made on. The press
                // came back through the channel, and another player's
                // playlists put up in between would lend this service and
                // playlist id to that player's track.
                let (id, options) = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Pane::Playlists(page) = &browsing.pane else {
                        continue;
                    };
                    if page.opened != opened {
                        continue;
                    }
                    (page.device, page.options.clone())
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };

                let named = match &target {
                    bluos::client::PlaylistTarget::New(name) => name.clone(),
                    bluos::client::PlaylistTarget::Existing { name, .. } => name.clone(),
                };

                // Off the loop, for the reason spelled out on `BrowseHome`.
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client
                        .add_to_playlist(&options, service.as_deref(), &target)
                        .await
                    {
                        Ok(()) => {
                            // Closes its own page and nothing else. Whatever
                            // was opened while the add was out — Settings, an
                            // alarm half edited, a form half typed — is where
                            // the user went since.
                            let closed = {
                                let mut browsing = backend.browsing.lock().unwrap();
                                let own = matches!(
                                    &browsing.pane,
                                    Pane::Playlists(page) if page.opened == opened
                                );
                                if own {
                                    browsing.pane = Pane::Browse;
                                }
                                own
                            };
                            if closed {
                                backend.publish_pane();
                            }
                            say(&backend.ui, format!("Added to {named}"));
                        }
                        Err(e) => {
                            tracing::warn!(%id, "could not add to {named}: {e}");
                            say(&backend.ui, format!("Could not add to {named}"));
                        }
                    }
                });
                continue;
            }

            Command::OpenAlarms(id) => {
                let selection = backend.selection_of(id);
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.alarms().await {
                        Ok(list) => {
                            {
                                let mut browsing = backend.browsing.lock().unwrap();
                                // Answered after the settings page it was
                                // chosen on has gone, and another player chosen:
                                // its alarms would replace that player's Home.
                                if !backend.may_open_for(&browsing, id, selection) {
                                    return;
                                }
                                browsing.pane = Pane::Alarms(Box::new(AlarmsPage {
                                    device: id,
                                    list,
                                    editing: None,
                                    opened: 0,
                                    picking: Vec::new(),
                                }));
                                browsing.highlighted.clear();
                            }
                            backend.publish_sidebar();
                            backend.publish_pane();
                        }
                        Err(e) => {
                            tracing::warn!(%id, "could not read the alarms: {e}");
                            say(&backend.ui, format!("could not read the alarms: {e}"));
                        }
                    }
                });
                continue;
            }

            Command::AlarmArm(id, alarm, on) => {
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                // In order, for the reason on the queue's edits: two of these
                // in flight at once are two writes to one list.
                let mut ticket = backend.writes.enter();
                let backend = backend.clone();
                tokio::spawn(async move {
                    ticket.wait().await;
                    match client.arm_alarm(alarm, on).await {
                        // The reply is the whole list, so the screen is redrawn
                        // from what the player now holds rather than from what
                        // was asked for.
                        Ok(list) => {
                            if replace_alarms(&backend, id, list) {
                                backend.publish_pane();
                            }
                        }
                        Err(e) => say(&backend.ui, format!("could not change the alarm: {e}")),
                    }
                });
                continue;
            }

            Command::AlarmOpen(which) => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let Pane::Alarms(page) = &mut browsing.pane else {
                        continue;
                    };
                    let Some(found) = page.list.alarms.iter().find(|a| a.id == which) else {
                        continue;
                    };
                    page.editing = Some(found.clone());
                    page.opened = backend.openings.fetch_add(1, Ordering::Relaxed) + 1;
                }
                backend.publish_pane();
                continue;
            }

            Command::AlarmNew { schedule } => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let Pane::Alarms(page) = &mut browsing.pane else {
                        continue;
                    };
                    page.opened = backend.openings.fetch_add(1, Ordering::Relaxed) + 1;
                    page.editing = Some({
                        // The controller's own defaults: seven in the morning,
                        // a quarter of an hour, and a volume that does not
                        // depend on where the speaker was left.
                        let mut fresh = bluos::alarms::Alarm {
                            hour: 7,
                            minute: 0,
                            duration: DURATIONS[0],
                            volume: 25,
                            enabled: true,
                            // Falling back to a tone is for an alarm, which has
                            // one job. A schedule that cannot reach its source
                            // should stay quiet rather than start beeping in
                            // the middle of the afternoon.
                            use_backup: !schedule,
                            ..Default::default()
                        };
                        if schedule {
                            // An hour after it starts, which is where the
                            // official controller opens the field. It is only
                            // a starting point; the second time field is the
                            // first thing on the page.
                            fresh.end = Some(format!("{:02}00", (fresh.hour + 1).min(23)));
                        }
                        fresh
                    });
                }
                backend.publish_pane();
                continue;
            }

            Command::AlarmEdit(change) => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let Pane::Alarms(page) = &mut browsing.pane else {
                        continue;
                    };
                    let Some(alarm) = page.editing.as_mut() else {
                        continue;
                    };
                    match change {
                        AlarmField::Start(hour, minute) => {
                            alarm.hour = hour;
                            alarm.minute = minute;
                        }
                        AlarmField::End(hour, minute) => {
                            alarm.end = Some(format!("{hour:02}{minute:02}"));
                        }
                        // The two are exclusive on the wire, so switching is
                        // giving one of them up.
                        AlarmField::Schedule(on) => {
                            alarm.end = on.then(|| {
                                let (hour, minute) = schedule_end(alarm);
                                format!("{hour:02}{minute:02}")
                            });
                        }
                        AlarmField::Day(at) => {
                            if let Some(day) = alarm.days.get_mut(at) {
                                *day = !*day;
                            }
                        }
                        AlarmField::Duration(minutes) => {
                            alarm.end = None;
                            alarm.duration = minutes;
                        }
                        AlarmField::Volume(v) => alarm.volume = v.min(100),
                        AlarmField::FadeIn(on) => alarm.fade_in = on,
                        AlarmField::Shuffle(on) => alarm.shuffle = on,
                        AlarmField::Backup(on) => alarm.use_backup = on,
                    }
                }
                backend.publish_pane();
                continue;
            }

            Command::AlarmPick => {
                // The tree is the alarm's player's, not the selected one's:
                // what is chosen from it is saved into that player's alarm.
                let at = match &backend.browsing.lock().unwrap().pane {
                    Pane::Alarms(page) => Some((page.device, page.top())),
                    _ => None,
                };
                let Some((id, top)) = at else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                let backend = backend.clone();
                tokio::spawn(async move {
                    let path = bluos::client::Client::station_root().to_owned();
                    open_picker(&backend, &client, id, top, "Plays".to_owned(), path).await;
                });
                continue;
            }

            Command::AlarmPickRow(at) => {
                // The row, and the owner and level it was pressed on, from one
                // look at the page.
                let chosen = {
                    let browsing = backend.browsing.lock().unwrap();
                    match &browsing.pane {
                        Pane::Alarms(page) => page.picking.last().and_then(|level| {
                            let row = level.rows.rows.get(at).cloned()?;
                            Some((page.device, page.top(), row))
                        }),
                        _ => None,
                    }
                };
                let Some((id, top, row)) = chosen else {
                    continue;
                };

                // A leaf is the answer: it goes into the working copy and the
                // picker comes down, all of it in the app. Nothing is sent to
                // the player until the alarm itself is saved.
                let Some(path) = row.into_path() else {
                    {
                        let mut browsing = backend.browsing.lock().unwrap();
                        if let Pane::Alarms(page) = &mut browsing.pane {
                            if let Some(alarm) = page.editing.as_mut() {
                                alarm.source = Some(row.text.clone());
                                alarm.service = row.service.clone();
                                alarm.url = row.url.clone();
                                alarm.image = row.image.clone();
                                // The player decides what can be shuffled, and
                                // a source that cannot must not stay switched
                                // on from whatever was picked before.
                                alarm.can_shuffle = false;
                                alarm.shuffle = false;
                            }
                            page.picking.clear();
                        }
                    }
                    backend.publish_pane();
                    continue;
                };

                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                let backend = backend.clone();
                tokio::spawn(async move {
                    open_picker(&backend, &client, id, top, row.text.clone(), path).await;
                });
                continue;
            }

            Command::AlarmSave => {
                // The copy, its owner and which opening of the editor it was
                // taken from, all from one lock: the alarm cannot be read off
                // one player's page and sent to the next one's, and the reply
                // knows which editor it is the answer to.
                let editing = {
                    let browsing = backend.browsing.lock().unwrap();
                    match &browsing.pane {
                        Pane::Alarms(page) => {
                            page.editing.clone().map(|a| (page.device, page.opened, a))
                        }
                        _ => None,
                    }
                };
                let Some((id, opened, alarm)) = editing else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                let mut ticket = backend.writes.enter();
                let backend = backend.clone();
                tokio::spawn(async move {
                    ticket.wait().await;
                    // A save always sends `enable=1`, because the route has no
                    // way to write a disarmed alarm — [`Client::save_alarm`]
                    // says so and names this second call as the other half.
                    // Without it, editing an alarm the user had switched off
                    // switched it back on again, and the first they knew was
                    // the toggle flipping back in the refreshed list.
                    //
                    // A new alarm is exempt: it is created armed, and its id
                    // is not known until the save has answered.
                    let saved = match client.save_alarm(&alarm).await {
                        Ok(list) if alarm.enabled || alarm.id == 0 => Ok(list),
                        Ok(_) => client.arm_alarm(alarm.id, false).await,
                        Err(e) => Err(e),
                    };
                    match saved {
                        Ok(list) => {
                            // Saved either way; the page is only put back if
                            // it is still this player's.
                            {
                                let mut browsing = backend.browsing.lock().unwrap();
                                if let Pane::Alarms(page) = &mut browsing.pane
                                    && page.device == id
                                {
                                    // A refreshed list is the player's
                                    // whichever editor is up over it.
                                    page.list = list;
                                    // Back to the list: the alarm is the
                                    // player's now, not the copy's. Only the
                                    // editor that was saved, though — the save
                                    // waits its turn in the writes lane and
                                    // then makes a round trip, and an editor
                                    // opened since is half a typed alarm the
                                    // user is still in, not this reply's to
                                    // throw away. The picker goes with it, or
                                    // a level would be left drawn over no
                                    // editor, and the source chosen on it
                                    // dropped.
                                    if page.opened == opened {
                                        page.editing = None;
                                        page.picking.clear();
                                    }
                                }
                            }
                            say(&backend.ui, "Alarm saved");
                            backend.publish_pane();
                        }
                        Err(e) => say(&backend.ui, format!("could not save the alarm: {e}")),
                    }
                });
                continue;
            }

            Command::AlarmDelete => {
                // The alarm, its owner and its opening from one lock, as Save
                // does.
                let editing = {
                    let browsing = backend.browsing.lock().unwrap();
                    match &browsing.pane {
                        Pane::Alarms(page) => page
                            .editing
                            .as_ref()
                            .map(|a| (page.device, page.opened, a.id)),
                        _ => None,
                    }
                };
                // Nothing to delete on one that was never saved; closing the
                // editor is the whole of it.
                let Some((id, opened, alarm)) = editing.filter(|(.., alarm)| *alarm != 0) else {
                    if let Pane::Alarms(page) = &mut backend.browsing.lock().unwrap().pane {
                        page.editing = None;
                    }
                    backend.publish_pane();
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                let mut ticket = backend.writes.enter();
                let backend = backend.clone();
                tokio::spawn(async move {
                    ticket.wait().await;
                    match client.delete_alarm(alarm).await {
                        Ok(list) => {
                            {
                                let mut browsing = backend.browsing.lock().unwrap();
                                if let Pane::Alarms(page) = &mut browsing.pane
                                    && page.device == id
                                {
                                    page.list = list;
                                    // Only the editor the delete was pressed
                                    // on, for the reason Save's reply gives.
                                    if page.opened == opened {
                                        page.editing = None;
                                        page.picking.clear();
                                    }
                                }
                            }
                            say(&backend.ui, "Alarm deleted");
                            backend.publish_pane();
                        }
                        Err(e) => say(&backend.ui, format!("could not delete the alarm: {e}")),
                    }
                });
                continue;
            }

            Command::DialogPress(at) => {
                // Taken, not read: the question is answered either way, and
                // leaving it up while its action runs would let it be pressed
                // twice.
                let chosen = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    browsing
                        .dialog
                        .take()
                        .and_then(|(id, d)| d.choices.into_iter().nth(at).map(|c| (id, c)))
                };
                backend.publish_dialog();

                // Dismissed, or a button that only closes. The player writes
                // Cancel as an action of type `nil`, so there is nothing to
                // send and nothing to report.
                //
                // Run on the player that asked, not the one selected now: the
                // button is that player's request.
                let Some((id, choice)) = chosen.filter(|(_, c)| !c.is_cancel()) else {
                    continue;
                };
                let Some(action) = choice.action else {
                    continue;
                };
                // Off the loop, like everything else that talks to a player.
                let backend = backend.clone();
                tokio::spawn(run_action(backend, id, action, Arrive::Deeper));
                continue;
            }

            Command::ConfirmInput(go) => {
                // On the player whose input it is, which is the one the
                // question was about.
                let pending = backend.browsing.lock().unwrap().pending_input.take();
                if let (true, Some((id, action))) = (go, pending) {
                    tokio::spawn(run_action(backend.clone(), id, action, Arrive::Deeper));
                }
                continue;
            }

            Command::QueueMenuAction(at) => {
                // The player the menu came from, not whichever is selected
                // now: the lines on it carry that player's own paths.
                let Some(id) = backend.browsing.lock().unwrap().queue_menu_owner else {
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
                        // Favoriting and deleting both change the queue, and
                        // the player announces neither.
                        fetch_queue(backend, id).await;
                    });
                }
                continue;
            }

            Command::OpenSettings(page, step) => {
                // A fresh pane is the selected player's; a step from a page
                // is the player that page belongs to, whoever is selected now.
                // The selection is numbered either way, because a fresh pane
                // arriving after another player was chosen is dropped.
                let Some((selected, selection)) = backend.selection() else {
                    continue;
                };
                let id = match step {
                    Step::Root => selected,
                    Step::Deeper(owner) | Step::Reload(owner) => owner,
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                // Off the loop, for the reason spelled out on `BrowseHome`:
                // awaiting the round trip here stops the loop reading its
                // channel, so a player that has gone away takes the whole
                // timeout with the window still painting and answering
                // nothing. Two presses in flight at once can now land out of
                // order, which is worth less than a window that responds.
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.settings(page.as_deref()).await {
                        Ok(page) => {
                            let mut browsing = backend.browsing.lock().unwrap();
                            match (&mut browsing.pane, step) {
                                // Deeper and reload both keep whatever is under
                                // them; only the top of the trail differs.
                                //
                                // And only on the trail they came from. The
                                // pane may have been left, or replaced by
                                // another player's settings, while the page
                                // was on its way; either way there is nothing
                                // left for it to join, and opening a pane for
                                // it would put one player's rows under
                                // another's name.
                                (Pane::Settings(owner, trail), Step::Deeper(from))
                                    if *owner == from =>
                                {
                                    trail.push(page)
                                }
                                (Pane::Settings(owner, trail), Step::Reload(from))
                                    if *owner == from && !trail.is_empty() =>
                                {
                                    let top = trail.len() - 1;
                                    trail[top] = page;
                                }
                                (_, Step::Deeper(_) | Step::Reload(_)) => return,
                                // Checked under the same lock as the write, for
                                // the reason on `Backend::still_selected`.
                                (_, Step::Root) if !backend.still_selected(selection) => {
                                    return;
                                }
                                (_, Step::Root) => browsing.pane = Pane::Settings(id, vec![page]),
                            }
                            // Settings and Help are rows of their own below the
                            // list, so nothing in the list is where you are any
                            // more. Leaving the last screen lit says you are on a
                            // screen you are not looking at.
                            browsing.highlighted.clear();
                            drop(browsing);
                            backend.publish_sidebar();
                            // See the note on `OpenHelp`: the pane flags are
                            // answered by `publish_pane` and by nothing else.
                            backend.publish_pane();
                        }
                        Err(e) => {
                            tracing::warn!(%id, "could not read settings: {e}");
                            say(&backend.ui, format!("could not read settings: {e}"));
                        }
                    }
                });
                continue;
            }

            // While the alarms screen is up, its rows are alarms rather than
            // settings, so the presses the settings pane produces are turned
            // into alarm commands before the settings arm ever sees them.
            Command::OpenStations => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    browsing.pane = Pane::Stations(Box::new(StationsPage {
                        stations: custom::load(),
                        ..StationsPage::default()
                    }));
                }
                // Through publish_pane, never the specific publisher: it is
                // what answers every "this pane is showing" flag, and a pane
                // opened past it comes up underneath whatever is already on
                // top of it.
                backend.publish_pane();
                continue;
            }

            Command::TrackAction(name) => {
                let Some((id, selection)) = backend.selection() else {
                    continue;
                };
                // Read the URL now rather than carrying it from when the
                // button was drawn: a rating flips the player's own state, and
                // the URL that unsets a Love is not the one that set it.
                let found = backend.with_entry(id, |entry| {
                    entry.status.as_ref().and_then(|status| {
                        status
                            .action(&name)
                            .and_then(|action| action.url.clone())
                            .zip(Some(entry.client.clone()))
                    })
                });
                let Some(Some((url, client))) = found else {
                    tracing::debug!(%id, "no {name} action on this track any more");
                    continue;
                };

                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.follow(&url).await {
                        // Some of these open a shop, and the player asks
                        // before it sends anybody anywhere.
                        Ok(Some(dialog)) => offer_dialog(&backend, id, selection, dialog),
                        // Nothing said: the next status carries the new state
                        // and the button redraws itself from it.
                        Ok(None) => {}
                        Err(e) => say(&backend.ui, format!("could not do that: {e}")),
                    }
                });
                continue;
            }

            Command::StationsEdit => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    if let Pane::Stations(page) = &mut browsing.pane {
                        page.editing = !page.editing;
                    }
                }
                backend.publish_stations();
                continue;
            }

            Command::StationAdd(name, url) => {
                let said = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let Pane::Stations(page) = &mut browsing.pane else {
                        continue;
                    };
                    let url = url.trim();
                    if url.is_empty() {
                        None
                    } else if !custom::playable(url) {
                        Some("A station needs an http or https address".to_owned())
                    } else if custom::remember(&mut page.stations, &name, url) {
                        custom::save(&page.stations);
                        None
                    } else {
                        Some("That station is already here".to_owned())
                    }
                };
                backend.publish_stations();
                if let Some(line) = said {
                    say(&backend.ui, line);
                }
                continue;
            }

            Command::StationRename(at, name) => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let Pane::Stations(page) = &mut browsing.pane else {
                        continue;
                    };
                    let Some(station) = page.stations.get_mut(at) else {
                        continue;
                    };
                    // Tidied here and not only on the way to the file: the
                    // window draws this model, so an untidied name would be
                    // on screen until something reloaded the list.
                    station.name = custom::tidy(&name);
                    custom::save(&page.stations);
                }
                // Deliberately no republish: the row that reported this is the
                // field being typed into, and replacing the model under it
                // would take the cursor with it.
                continue;
            }

            Command::StationRemove(at) => {
                let gone = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let Pane::Stations(page) = &mut browsing.pane else {
                        continue;
                    };
                    if at >= page.stations.len() {
                        continue;
                    }
                    let gone = page.stations.remove(at);
                    if page.stations.is_empty() {
                        // Nothing left to edit, so the mode has nothing to be
                        // about — and the button that leaves it is gone too.
                        page.editing = false;
                    }
                    custom::save(&page.stations);
                    gone
                };
                backend.publish_stations();
                say(
                    &backend.ui,
                    format!(
                        "Removed {}",
                        if gone.name.is_empty() {
                            gone.url
                        } else {
                            gone.name
                        }
                    ),
                );
                continue;
            }

            Command::StationMove(from, to) => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let Pane::Stations(page) = &mut browsing.pane else {
                        continue;
                    };
                    if from >= page.stations.len() || to >= page.stations.len() || from == to {
                        continue;
                    }
                    let moved = page.stations.remove(from);
                    page.stations.insert(to, moved);
                    custom::save(&page.stations);
                }
                backend.publish_stations();
                continue;
            }

            Command::StationPlay(at) => {
                let station = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Pane::Stations(page) = &browsing.pane else {
                        continue;
                    };
                    match page.stations.get(at) {
                        Some(station) => station.clone(),
                        None => continue,
                    }
                };
                let Some((id, selection)) = backend.selection() else {
                    say(&backend.ui, "No player is selected");
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.play_url(&station.url).await {
                        // The player asks before discarding a queue, and the
                        // question has to reach somebody: answering it either
                        // way from here is deciding for them.
                        Ok(Some(dialog)) => offer_dialog(&backend, id, selection, dialog),
                        Ok(None) => say(
                            &backend.ui,
                            format!(
                                "Playing {}",
                                if station.name.is_empty() {
                                    &station.url
                                } else {
                                    &station.name
                                }
                            ),
                        ),
                        Err(e) => say(&backend.ui, format!("could not play it: {e}")),
                    }
                });
                continue;
            }

            Command::SettingEdit(_, Edit::Text(name))
                if matches!(backend.browsing.lock().unwrap().pane, Pane::EditPreset(_)) =>
            {
                // Sent to the player the preset belongs to. A rename is a
                // whole-slot replace, so on any other player it would overwrite
                // a preset rather than rename one.
                let (id, asked) = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Pane::EditPreset(page) = &browsing.pane else {
                        continue;
                    };
                    let mut preset = page.preset.clone();
                    preset.name = name.trim().to_owned();
                    (page.device, (page.slot, preset))
                };

                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };

                backend.browsing.lock().unwrap().pane = Pane::Browse;
                backend.publish_pane();
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.edit_preset(asked.0, &asked.1).await {
                        Ok(()) => {
                            say(&backend.ui, "Preset renamed");
                            refresh_current(backend).await;
                        }
                        Err(e) => say(&backend.ui, format!("could not rename it: {e}")),
                    }
                });
                continue;
            }

            Command::SettingAction(index) | Command::SettingEdit(index, _)
                if matches!(backend.browsing.lock().unwrap().pane, Pane::Alarms(_)) =>
            {
                let edit = match command {
                    Command::SettingEdit(_, edit) => Some(edit),
                    _ => None,
                };
                if let Some(next) = alarm_command(&backend, index, edit) {
                    let _ = backend.commands.send(next);
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
                    Pane::Web(..) => {
                        let _ = backend.commands.send(Command::WebAction(index));
                        continue;
                    }
                    Pane::Form(_) => {
                        let _ = backend.commands.send(Command::FormPress(index));
                        continue;
                    }
                    Pane::Playlists(_) => {
                        let _ = backend.commands.send(Command::PlaylistPress(index));
                        continue;
                    }
                    _ => {}
                }

                // The row index counts groups-that-are-links and settings, in
                // the order settings_rows walks them, so the same walk finds it.
                //
                // The player comes off the pane with the page, under the same
                // lock: the rows are that player's, and so is every write and
                // every step deeper that a press on one of them makes.
                let owned = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing
                        .pane
                        .settings_owned()
                        .map(|(owner, page)| (owner, page.clone(), pick(page, index)))
                };
                let Some((id, page, chosen)) = owned else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };

                match chosen {
                    Some(Chosen::Page(deeper)) => {
                        let _ = backend
                            .commands
                            .send(Command::OpenSettings(Some(deeper), Step::Deeper(id)));
                    }
                    Some(Chosen::Alarms) => {
                        let _ = backend.commands.send(Command::OpenAlarms(id));
                    }
                    Some(Chosen::Sleep) => {
                        let _ = backend.commands.send(Command::Player(id, Action::Sleep));
                    }
                    Some(Chosen::Web(url)) => {
                        // Resolved against the player's own web UI and held to
                        // its host, the way OpenForm's fallback is. The
                        // attribute is the player's to write, and it went to
                        // the desktop's opener as written: a relative path
                        // opened nothing, and a rogue device could name
                        // anything the desktop has a handler for.
                        let url = match client.web_url(&url) {
                            Ok(resolved) => resolved,
                            Err(e) => {
                                tracing::warn!(%id, "refusing to open {url}: {e}");
                                say(&backend.ui, "That page is not on this player");
                                continue;
                            }
                        };
                        // Except the share configuration, which is a list of
                        // what is mounted and a button to unmount it. That is
                        // worth drawing; the page that adds one asks for a
                        // password and is not.
                        //
                        // Both of these are this page's player's, like the
                        // URL they came from: resolved through its client,
                        // then read from whoever is selected, they were a
                        // second player's shares and wireless form under the
                        // first player's settings.
                        if url.contains("/sharecfg") {
                            let _ = backend.commands.send(Command::OpenShares {
                                device: id,
                                reload: false,
                            });
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
                                device: id,
                                title: "WiFi".to_owned(),
                                path: format!("/{path}"),
                            });
                            continue;
                        }
                        open_in_browser(&backend.ui, url).await;
                    }
                    Some(Chosen::Write(setting, value)) => {
                        // Off the loop and in order, for the reason on
                        // `SettingEdit`.
                        let mut ticket = backend.writes.enter();
                        let backend = backend.clone();
                        tokio::spawn(async move {
                            ticket.wait().await;
                            match client.write_setting(&setting, &value).await {
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
                                        Step::Reload(id),
                                    ));
                                }
                                Err(e) => {
                                    say(&backend.ui, format!("{}: {e}", setting.label()));
                                }
                            }
                        });
                    }
                    None => {}
                }
                continue;
            }

            Command::OpenHelp => {
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    browsing.pane = Pane::Help;
                    browsing.highlighted.clear();
                }
                backend.publish_sidebar();
                // `publish_pane`, not `publish_help`: it draws the same rows
                // by way of `Showing::Help`, and on the way it answers the
                // flags that say which pane is up. Calling the publisher
                // directly left `now_playing` set, so opening Help from the
                // sidebar while the sleeve was up changed the pane underneath
                // an overlay that stayed on top of it — the press looked
                // ignored when it had in fact worked.
                backend.publish_pane();
                continue;
            }

            Command::HelpAction(index) if index == UPGRADE_ROW => {
                // Off the page pressed, which says whose check found the
                // update. A page without an offer has no Install to press.
                let offer = match &backend.browsing.lock().unwrap().pane {
                    Pane::HelpDetail(_, _, _, offer) => *offer,
                    _ => None,
                };
                if let Some(id) = offer {
                    let _ = backend.commands.send(Command::UpgradeAsk(id));
                }
                continue;
            }

            Command::HelpAction(index) => {
                let Some((_, kind, _, _)) = HELP_ENTRIES.get(index) else {
                    // The About row, which is text rather than a link.
                    continue;
                };
                // The selection out first; see the note on `Backend`.
                //
                // Numbered, because both pages below land a round trip or two
                // after the press and Help stays up when another player is
                // chosen. Put up then, the first player's version and update
                // status sat under the second one's name, with an Install that
                // would start the second player's upgrade.
                let selection = backend.selection();
                let selected = selection.map(|(id, _)| id);
                let client = selected.and_then(|id| backend.with_entry(id, |e| e.client.clone()));

                // Off the loop, for the reason spelled out on `BrowseHome`.
                // Two of the three below ask the player something, and the
                // upgrade check in particular is the slowest request it
                // answers.
                let backend = backend.clone();
                tokio::spawn(async move {
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
                            let Some(client) = client else { return };
                            match client.diagnostics().await {
                                Ok(facts) if !facts.is_empty() => {
                                    {
                                        let mut browsing = backend.browsing.lock().unwrap();
                                        // With the lock held, for the reason
                                        // on `Backend::still_selected`.
                                        if !selection
                                            .is_some_and(|(_, n)| backend.still_selected(n))
                                        {
                                            return;
                                        }
                                        browsing.pane = Pane::HelpDetail(
                                            "Diagnostics".to_owned(),
                                            facts,
                                            Whence::Help,
                                            None,
                                        );
                                    }
                                    // As above: asking the player takes as
                                    // long as it takes, and the sidebar is
                                    // live throughout.
                                    backend.publish_pane();
                                }
                                // The page is the player's own HTML, so it can
                                // change under us; offer it rather than nothing.
                                _ => {
                                    // Held to the rule the page above is, as
                                    // the services fallback is: a browser is a
                                    // window too, and this is the first
                                    // player's page over the second's name.
                                    if !selection.is_some_and(|(_, n)| backend.still_selected(n)) {
                                        return;
                                    }
                                    let url = client.image_url("/redirectToCp?href=/diagnostics");
                                    open_in_browser(&backend.ui, url).await;
                                }
                            }
                        }
                        HelpKind::Upgrade => {
                            let Some(client) = client else { return };

                            // Two answers, from two routes. The page is the
                            // player's own words and is what a person reads;
                            // the check is the machine-readable one, and is
                            // what decides whether installing is offered.
                            let page = client.upgrade_check().await;
                            // Routed the same way the install will be, so the
                            // page cannot offer an Install for a player the
                            // start would then refuse to reach.
                            let ready = match selected {
                                Some(id) => ask_about_firmware(&backend, id)
                                    .await
                                    .map(|(_, _, ready)| ready),
                                None => client.upgrade_available().await,
                            };

                            let mut facts = match &page {
                                Ok((status, _)) => vec![(
                                    "Status".to_owned(),
                                    status.clone().unwrap_or_else(|| {
                                        "The player did not answer clearly".to_owned()
                                    }),
                                )],
                                Err(e) => {
                                    vec![("Status".to_owned(), format!("could not ask: {e}"))]
                                }
                            };

                            // What it runs now, which the player states on
                            // /SyncStatus and is the one version known for
                            // certain. Useful on its own: "up to date" means
                            // more beside a number than without one.
                            if let Some(installed) = selected
                                .and_then(|id| {
                                    backend.with_entry(id, |e| {
                                        e.sync.as_ref().and_then(|s| s.version.clone())
                                    })
                                })
                                .flatten()
                            {
                                facts.push(("Installed".to_owned(), installed));
                            }

                            // Only where the player names one. Nothing is
                            // known to send it — the official controller reads
                            // the two flags and discards the rest — so this is
                            // shown when it turns up and absent otherwise
                            // rather than reported as missing.
                            if let Ok(ready) = &ready
                                && let Some(offered) = &ready.version
                            {
                                facts.push(("New version".to_owned(), offered.clone()));
                            }

                            match &ready {
                                Ok(ready) if ready.in_progress => {
                                    facts.push((
                                        "Installing".to_owned(),
                                        "An upgrade is running on this player now".to_owned(),
                                    ));
                                }
                                // Nothing said here when one is available: the
                                // Install row below says it, and the player's
                                // own status line above has already said it
                                // too. A third telling is furniture.
                                Ok(ready) if ready.available => {}
                                Ok(_) => facts.push((
                                    "Update".to_owned(),
                                    "The player is up to date".to_owned(),
                                )),
                                Err(e) => {
                                    facts.push(("Update".to_owned(), format!("could not ask: {e}")))
                                }
                            }

                            let offer = matches!(&ready, Ok(r) if r.available && !r.in_progress);

                            {
                                let mut browsing = backend.browsing.lock().unwrap();
                                // As for Diagnostics above.
                                if !selection.is_some_and(|(_, n)| backend.still_selected(n)) {
                                    return;
                                }
                                // The offer goes up as part of the page and
                                // names the player checked, so it cannot
                                // outlive this page or be read for another.
                                browsing.pane = Pane::HelpDetail(
                                    "Upgrade Check".to_owned(),
                                    facts,
                                    Whence::Help,
                                    selected.filter(|_| offer),
                                );
                            }
                            // And this one waits on two round trips.
                            backend.publish_pane();
                        }
                    }
                });
                continue;
            }

            Command::SettingEdit(index, edit)
                if matches!(backend.browsing.lock().unwrap().pane, Pane::Form(_)) =>
            {
                let _ = backend.commands.send(Command::FormEdit(index, edit));
                continue;
            }

            Command::OpenForm {
                device: id,
                title,
                path,
            } => {
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                // Up before the request, not after it. Asking the player for
                // its wireless page makes it scan for access points first, and
                // that is three and a half seconds of a window that looks like
                // it ignored the press.
                //
                // Over the page it was chosen on and nothing else; if that has
                // gone there is no press left to answer.
                let Some(opened) = show_form(
                    &backend,
                    id,
                    Replacing::Page,
                    title.clone(),
                    bluos::forms::Form::default(),
                    "Reading the page…".to_owned(),
                ) else {
                    continue;
                };

                // Off the loop, for the reason spelled out on `BrowseHome`:
                // awaiting the round trip here stops the loop reading its
                // channel, so a player that has gone away takes the whole
                // timeout with the window still painting and answering
                // nothing. Two presses in flight at once can now land out of
                // order, which is worth less than a window that responds.
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.web_form(&path).await {
                        Ok(Some(form)) => {
                            show_form(
                                &backend,
                                id,
                                Replacing::Form(opened),
                                title,
                                form,
                                String::new(),
                            );
                        }
                        // No form on it, or a shape this crate cannot read. The
                        // page itself still works, so offer that rather than
                        // nothing.
                        other => {
                            if let Err(e) = other {
                                tracing::debug!(%id, "could not read {path}: {e}");
                            }
                            // Nothing here to draw, so put the page itself up and
                            // leave the placeholder behind — if it is still the
                            // pane showing, and back on the page it was opened
                            // from. Whatever replaced it in the meantime is not
                            // this press's to take away, and neither is it to
                            // open a browser over.
                            if !close_form(&backend, id, opened) {
                                return;
                            }
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
                });
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
                // Submitted to the player that served the form, read off the
                // page under the same lock as what is sent: see
                // `FormPage::device`.
                let sending = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Some(page) = browsing.pane.form() else {
                        continue;
                    };
                    let first = usize::from(!page.note.is_empty()) + page.form.fields.len();
                    page.form.submits.get(at.wrapping_sub(first)).map(|submit| {
                        (
                            page.device,
                            page.opened,
                            page.title.clone(),
                            page.form.clone(),
                            page.values.clone(),
                            submit.clone(),
                        )
                    })
                };
                let Some((id, opened, title, form, values, submit)) = sending else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };

                // Off the loop, but in order: a form is a sequence of steps,
                // and step two is submitted against what step one left on the
                // player. See `lane`.
                let mut ticket = backend.writes.enter();
                let backend = backend.clone();
                tokio::spawn(async move {
                    ticket.wait().await;
                    match client.submit_form(&form, &values, &submit).await {
                        Ok(body) => {
                            // The answer is another page: the same form with a
                            // message when something was wrong, the next step's
                            // form when it was right. Following it is what lets one
                            // screen lead to another without knowing the route.
                            match bluos::forms::parse(&body).into_iter().next() {
                                Some(next) => {
                                    show_form(
                                        &backend,
                                        id,
                                        Replacing::Form(opened),
                                        title,
                                        next,
                                        bluos::reports::message(&body),
                                    );
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
                                    // Off the form it answered, and only that,
                                    // back to the page it was opened from —
                                    // read again, since the submit changed it.
                                    close_answered_form(&backend, id, opened);
                                    backend.publish_pane();
                                }
                            }
                        }
                        Err(e) => say(&backend.ui, format!("{}: {e}", submit.label)),
                    }
                });
                continue;
            }

            Command::SettingEdit(index, edit) => {
                // The naming line on the "add to playlist" list is a settings
                // text row like any other, but there is no setting behind it
                // to write — the name is the whole request.
                if matches!(backend.browsing.lock().unwrap().pane, Pane::Playlists(_)) {
                    if let Edit::Text(name) = edit {
                        let _ = backend.commands.send(Command::PlaylistNamed(name));
                    }
                    continue;
                }

                // The player the page came from, read with the page: the
                // setting names its own URL and field, and they are that
                // player's.
                let owned = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing
                        .pane
                        .settings_owned()
                        .map(|(owner, page)| (owner, page.clone()))
                };
                let Some((id, page)) = owned else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                let Some(setting) = setting_at(&page, index) else {
                    continue;
                };
                // Not written while the player has it dimmed, for the reason
                // on `pick`: the disabled control is one way in, not the only
                // one.
                if !page.is_available(&setting) {
                    continue;
                }

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

                // Off the loop, but in order: dragging a slider sends a write
                // per step, and the value the player is left holding has to be
                // the last one asked for, not whichever reply came back last.
                // See `lane`.
                let mut ticket = backend.writes.enter();
                let backend = backend.clone();
                tokio::spawn(async move {
                    ticket.wait().await;
                    match client.write_setting(&setting, &value).await {
                        Ok(()) => {
                            // Re-read rather than assume: a write can move more
                            // than the one value — turning tone controls on
                            // brings treble and bass to life — and only the
                            // player knows.
                            let _ = backend.commands.send(Command::OpenSettings(
                                page.page_id.clone(),
                                Step::Reload(id),
                            ));
                        }
                        Err(e) => say(&backend.ui, format!("{}: {e}", setting.label())),
                    }
                });
                continue;
            }

            Command::Sidebar(kind, index) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                // Every entry below is read out of the player's configuration
                // — the screens it lists, the rows of its Sources screen — and
                // run on `id`. Those are the last player's until this one's
                // configuration arrives, and for as long as it is chosen when
                // it cannot give one: pressing an input there sent the other
                // player's capture path to this speaker and stopped it.
                //
                // Refused, and the configuration asked for again. A player
                // that was waking up may well answer the second time, and one
                // request per press cannot become a loop.
                let configured = backend.browsing.lock().unwrap().configured == Some(id);
                if !configured {
                    say(&backend.ui, "This player has not said what it offers yet");
                    let _ = backend.commands.send(Command::BrowseHome);
                    continue;
                }
                if kind != 2 {
                    backend.browsing.lock().unwrap().light(id, (kind, index));
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
                    // An input is the one thing here that stops the music: a
                    // service opens a screen, an input sends a play command
                    // that takes the speaker away from whatever it was doing.
                    // The two are told apart by which action the item carries.
                    let (action, label) = {
                        let browsing = backend.browsing.lock().unwrap();
                        browsing
                            .sources
                            .as_ref()
                            .and_then(|screen| screen.items().nth(index.max(0) as usize))
                            .map(|item| {
                                (
                                    item.action.clone().or_else(|| item.play_action.clone()),
                                    item.label().unwrap_or("that input").to_owned(),
                                )
                            })
                            .unwrap_or((None, String::new()))
                    };

                    let Some(action) = action else { continue };

                    if !ask_before_input(&backend, id, &action, &label) {
                        tokio::spawn(run_action(backend.clone(), id, action, Arrive::Deeper));
                    }
                }
                continue;
            }

            Command::BrowseSearch(query) => {
                // Typing is the search; there is no Enter to wait for. Held
                // back until the typing settles so a word costs one request,
                // and numbered so that only the last one asked for lands.
                //
                // The selection is read now, as the key is typed, rather than
                // once the typing settles; see `run_search`.
                let selection = backend.selection();
                let generation = backend.searches.fetch_add(1, Ordering::Relaxed) + 1;
                let backend = backend.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(SEARCH_SETTLE).await;
                    if backend.searches.load(Ordering::Relaxed) != generation {
                        return;
                    }
                    run_search(backend, query, selection).await;
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
                // Off the loop, but in order: `from` and `to` are positions
                // in a list each of these requests rewrites, so two of them
                // overtaking each other move the wrong track. See `lane`.
                let mut ticket = backend.writes.enter();
                let backend = backend.clone();
                tokio::spawn(async move {
                    ticket.wait().await;
                    match client.move_queue_item(from, to).await {
                        // The player does not announce a reorder — the queue's
                        // own id does not change — so the list is re-read
                        // rather than waited for. Awaited rather than spawned,
                        // so the next reorder sees the list this one made.
                        Ok(()) => fetch_queue(backend.clone(), id).await,
                        Err(e) => say(&backend.ui, format!("could not move the track: {e}")),
                    }
                });
                continue;
            }

            Command::QueueRemove(index) => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                // Off the loop, but in order: `index` is a position in a list
                // each removal shortens. See `lane`.
                let mut ticket = backend.writes.enter();
                let backend = backend.clone();
                tokio::spawn(async move {
                    ticket.wait().await;
                    match client.delete_queue_item(index).await {
                        Ok(()) => fetch_queue(backend.clone(), id).await,
                        Err(e) => say(&backend.ui, format!("could not remove the track: {e}")),
                    }
                });
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
                // Off the loop, and in the same line as the edits above: a
                // save has to see the queue those left behind. See `lane`.
                let mut ticket = backend.writes.enter();
                let backend = backend.clone();
                tokio::spawn(async move {
                    ticket.wait().await;
                    match client.save_queue(&name).await {
                        Ok(()) => {
                            say(&backend.ui, format!("Saved as \"{name}\""));
                            fetch_queue(backend.clone(), id).await;
                        }
                        Err(e) => say(&backend.ui, format!("could not save the queue: {e}")),
                    }
                });
                continue;
            }

            Command::QueueButton(at) => {
                let selected = *backend.selected.lock().unwrap();
                // The button is run on the player whose document it came
                // from, and only while that is the queue on screen. Otherwise
                // the press was on a row drawn before the selection moved, and
                // its index means nothing in whatever is there now.
                let Some((id, action)) = backend
                    .browsing
                    .lock()
                    .unwrap()
                    .queue_screen
                    .as_ref()
                    .filter(|(owner, _)| Some(*owner) == selected)
                    .map(|(owner, screen)| {
                        let action = screen
                            .buttons
                            .get(at)
                            .and_then(|button| button.action.clone());
                        (*owner, action)
                    })
                else {
                    continue;
                };

                // Queue Builder Mode is kept here, not on the player. The call
                // still goes out — another controller may be listening for it
                // and it costs nothing — but the behavior it names is this
                // app's: while it is on, pressing a track appends instead of
                // playing. See `screen::appending`.
                let toggled = action
                    .as_ref()
                    .and_then(|a| a.uri.as_deref())
                    .is_some_and(|uri| uri.contains("CBQ="));
                if toggled {
                    let now = {
                        let mut browsing = backend.browsing.lock().unwrap();
                        browsing.queue_building = !browsing.queue_building;
                        browsing.queue_building
                    };
                    say(
                        &backend.ui,
                        if now {
                            "Queue builder mode on — tracks are added to the end"
                        } else {
                            "Queue builder mode off"
                        },
                    );
                    backend.publish_queue();
                }
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

            Command::OpenServices { device: id, reload } => {
                let selection = backend.selection_of(id);
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                // Off the loop, for the reason spelled out on `BrowseHome`:
                // awaiting the round trip here stops the loop reading its
                // channel, so a player that has gone away takes the whole
                // timeout with the window still painting and answering
                // nothing. Two presses in flight at once can now land out of
                // order, which is worth less than a window that responds.
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.services().await {
                        Ok(services) if !services.is_empty() => {
                            let mut browsing = backend.browsing.lock().unwrap();
                            // See `Backend::may_open_for`. A sign-in form
                            // opened off this list goes to the player it is
                            // drawn for, so it must not be drawn over another.
                            // A reload only over the list it is reading again.
                            let may = if reload {
                                matches!(
                                    &browsing.pane,
                                    Pane::Web(owner, WebPage::Services(_)) if *owner == id
                                )
                            } else {
                                backend.may_open_for(&browsing, id, selection)
                            };
                            if !may {
                                return;
                            }
                            browsing.pane = Pane::Web(id, WebPage::Services(services));
                            browsing.highlighted.clear();
                            drop(browsing);
                            backend.publish_sidebar();
                            // `publish_pane`, not `publish_web`: this lands
                            // after a round trip the loop went on reading
                            // through, so the pane it is opening over may not
                            // be the one that was up when the press was made.
                            backend.publish_pane();
                        }
                        // The page is the player's own HTML and a firmware update
                        // could change its shape. Falling back to opening it beats
                        // showing an empty list and claiming there are no services.
                        other => {
                            if let Err(e) = other {
                                tracing::debug!(%id, "could not read the services page: {e}");
                            }
                            // Not for a reload, which nobody asked a window
                            // for. The list already up stays as it is.
                            if reload {
                                return;
                            }
                            // Held to the same rule as the list it stands in
                            // for. A browser is a window too, and one opened
                            // on this player's page after another has been
                            // chosen is this player's page over that one's.
                            let may = {
                                let browsing = backend.browsing.lock().unwrap();
                                backend.may_open_for(&browsing, id, selection)
                            };
                            if !may {
                                return;
                            }
                            // A constant, so this cannot be off-player.
                            if let Ok(url) = client.web_url("/services?noheader=1") {
                                open_in_browser(&backend.ui, url).await;
                            }
                        }
                    }
                });
                continue;
            }

            Command::OpenShares { device: id, reload } => {
                let selection = backend.selection_of(id);
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                // Off the loop, for the reason spelled out on `BrowseHome`:
                // awaiting the round trip here stops the loop reading its
                // channel, so a player that has gone away takes the whole
                // timeout with the window still painting and answering
                // nothing. Two presses in flight at once can now land out of
                // order, which is worth less than a window that responds.
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.shares().await {
                        Ok((action, shares)) => {
                            let mut browsing = backend.browsing.lock().unwrap();
                            // As for the services page above.
                            let may = if reload {
                                matches!(
                                    &browsing.pane,
                                    Pane::Web(owner, WebPage::Shares { .. }) if *owner == id
                                )
                            } else {
                                backend.may_open_for(&browsing, id, selection)
                            };
                            if !may {
                                return;
                            }
                            browsing.pane = Pane::Web(id, WebPage::Shares { action, shares });
                            browsing.highlighted.clear();
                            drop(browsing);
                            backend.publish_sidebar();
                            // As for the services page above.
                            backend.publish_pane();
                        }
                        Err(e) => {
                            tracing::debug!(%id, "could not read the shares page: {e}");
                            // As for the services page above.
                            if reload {
                                return;
                            }
                            let may = {
                                let browsing = backend.browsing.lock().unwrap();
                                backend.may_open_for(&browsing, id, selection)
                            };
                            if !may {
                                return;
                            }
                            // A constant, so this cannot be off-player.
                            if let Ok(url) = client.web_url("/sharecfg?noheader=1") {
                                open_in_browser(&backend.ui, url).await;
                            }
                        }
                    }
                });
                continue;
            }

            Command::WebAction(at) => {
                enum Press {
                    /// A page with a form on it, filled in here.
                    Form(String, String),
                    Remove(String, String),
                }

                // The player the page was read from, with the row, under one
                // lock: a share's field is that player's, and so is the form
                // behind a service.
                let (id, press) = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Some(id) = browsing.pane.owner() else {
                        continue;
                    };
                    let press = match browsing.pane.web() {
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
                    };
                    (id, press)
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };

                // Off the loop, but in order: unmounting two shares one after
                // the other sends the second against the list the first left.
                // See `lane`.
                let mut ticket = backend.writes.enter();
                let backend = backend.clone();
                tokio::spawn(async move {
                    ticket.wait().await;
                    match press {
                        Some(Press::Form(title, path)) => {
                            let _ = backend.commands.send(Command::OpenForm {
                                device: id,
                                title,
                                path,
                            });
                        }
                        Some(Press::Remove(action, field)) => {
                            match client
                                .remove_shares(&action, std::slice::from_ref(&field))
                                .await
                            {
                                Ok(()) => {
                                    say(&backend.ui, "Share removed");
                                    let _ = backend.commands.send(Command::OpenShares {
                                        device: id,
                                        reload: true,
                                    });
                                }
                                Err(e) => {
                                    say(&backend.ui, format!("could not remove {field}: {e}"))
                                }
                            }
                        }
                        None => {}
                    }
                });
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
                //
                // This player's own address or the default, as for a queue
                // row's menu.
                let uri = {
                    let browsing = backend.browsing.lock().unwrap();
                    browsing
                        .now_playing_menu
                        .clone()
                        .filter(|_| browsing.configured == Some(id))
                }
                .unwrap_or_else(|| "/ui/nowPlayingCM".to_owned());

                // Off the loop, for the reason spelled out on `BrowseHome`.
                // This one is two round trips deep — the menu, then whatever
                // it points at — so it is the worst of them to await here.
                let backend = backend.clone();
                tokio::spawn(async move {
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
                                None => {
                                    say(&backend.ui, "The player offers nothing about this track")
                                }
                            }
                        }
                        Err(e) => tracing::debug!(%id, "no now-playing menu: {e}"),
                    }
                });
                continue;
            }

            Command::ClearRecent => {
                backend.browsing.lock().unwrap().recent.clear();
                // Emptied on disk too. A list that came back at the next
                // startup would make "Clear" mean "until you restart".
                tokio::task::spawn_blocking(|| searches::save(&[]));
                backend.publish_browse();
                continue;
            }

            Command::QueueMore => {
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                // How much is held, how much there is, and which queue it is.
                // The identity matters: a queue replaced while the request is
                // out is a different list, and grafting a window of it onto
                // the rows already drawn would interleave two playlists.
                let held = backend
                    .with_entry(id, |e| {
                        e.queue
                            .as_ref()
                            .map(|q| (q.songs.len() as u32, q.length, q.id))
                    })
                    .flatten();
                let Some((held, total, pid)) = held else {
                    continue;
                };
                if held >= total {
                    continue;
                }
                {
                    let mut browsing = backend.browsing.lock().unwrap();
                    if browsing.fetching_queue {
                        continue;
                    }
                    browsing.fetching_queue = true;
                }
                tokio::spawn(fetch_more_queue(backend.clone(), id, held, pid));
                continue;
            }

            Command::BrowseEnds(to_end) => {
                // A paged list holds thirty rows of two thousand, so its end is
                // a fetch and not a scroll — the same one request a letter is,
                // aimed at the last window instead of a letter's. A list held
                // whole never gets here: the window scrolls itself, because it
                // is the only one that knows how tall the pane is.
                let target = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let offset = browsing
                        .trail
                        .last()
                        .and_then(|crumb| end_offset(crumb, to_end));
                    match offset {
                        None => None,
                        Some(offset) => {
                            let era = browsing.era;
                            browsing.jumps += 1;
                            let jump = browsing.jumps;
                            browsing.trail.last().and_then(|crumb| {
                                browsing
                                    .device
                                    .map(|id| (id, crumb.uri.clone(), offset, era, jump))
                            })
                        }
                    }
                };
                let Some((id, uri, offset, era, jump)) = target else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                tokio::spawn(jump_to_letter(
                    backend.clone(),
                    client,
                    uri,
                    offset,
                    era,
                    jump,
                    to_end,
                ));
                continue;
            }

            Command::OpenSearch => {
                // Read and let go before the trail is taken, as everywhere.
                let Some(id) = *backend.selected.lock().unwrap() else {
                    continue;
                };
                // The address is the configured player's, and it is opened on
                // that player, so it has to be the one selected. Refused the
                // way a sidebar press is, and before anything is lit: lighting
                // the last player's Search and then leaving `open_screen` to
                // decline left the entry lit over that player's Home, and
                // still lit when it was chosen back.
                let configured = backend.browsing.lock().unwrap().configured == Some(id);
                if !configured {
                    say(&backend.ui, "This player has not said what it offers yet");
                    let _ = backend.commands.send(Command::BrowseHome);
                    continue;
                }
                let target = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let uri = browsing
                        .search_uri
                        .clone()
                        .filter(|_| browsing.configured == Some(id));
                    // Light its row, exactly as pressing it would. Reaching a
                    // screen by keyboard and by hand has to leave the sidebar
                    // saying the same thing, or it goes on pointing at Home
                    // while Search is what is on the screen.
                    //
                    // Lit for and opened on `id`, the player whose address
                    // this is, as a sidebar press is — not on the player whose
                    // screens are showing. The two differ from the moment one
                    // player's configuration lands until its Home does, and
                    // for good if that Home never comes: lighting the last
                    // player's entry with this one's position lit Search over
                    // that player's Home once it was chosen back, and opening
                    // on it was declined, so the key did nothing at all.
                    if let Some(at) = uri
                        .as_ref()
                        .and_then(|uri| browsing.screens.iter().position(|(_, u)| u == uri))
                    {
                        browsing.light(id, (0, at as i32));
                    }
                    uri
                };
                if let Some(uri) = target {
                    backend.publish_sidebar();
                    // Deeper, not Root. Pressing Search in the sidebar means
                    // "start here" and clears the trail, but reaching it with a
                    // key from the middle of a library is a detour: measured
                    // with a probe, `can-go-back` went true to false the moment
                    // this opened, so Escape out of the box then had nowhere to
                    // go and looked broken. This way Back returns to whatever
                    // was being browsed when search was asked for.
                    tokio::spawn(open_screen(backend.clone(), id, uri, Arrive::Deeper));
                }
                continue;
            }

            Command::BrowseJump(key) => {
                // A list held whole scrolls to the letter; nothing is asked
                // for, because there is nothing that is not already here.
                let scrolled = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let stops = stops_derived(&browsing.derived, &key);
                    browsing.next_stop(&key, stops.len()).map(|at| stops[at])
                };
                if let Some(y) = scrolled {
                    backend.scroll_browse_to(y);
                    continue;
                }

                // Otherwise the offsets are the player's and live on the
                // screen; the window only ever sent the letter.
                let target = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    let stops = browsing
                        .trail
                        .last()
                        .map(|crumb| stops_sent(&crumb.screen.index, &key))
                        .unwrap_or_default();
                    match browsing.next_stop(&key, stops.len()) {
                        // A letter this list does not have. Nothing is claimed
                        // and nothing is canceled: bumping the counter here
                        // would abandon a jump still in flight because someone
                        // leaned on a key.
                        None => None,
                        Some(at) => {
                            let era = browsing.era;
                            // Claimed before the request goes out, so a jump
                            // already in flight knows it has been overtaken.
                            browsing.jumps += 1;
                            let jump = browsing.jumps;
                            let offset = stops[at];
                            browsing.trail.last().and_then(|crumb| {
                                browsing
                                    .device
                                    .map(|id| (id, crumb.uri.clone(), offset, era, jump))
                            })
                        }
                    }
                };
                let Some((id, uri, offset, era, jump)) = target else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    continue;
                };
                tokio::spawn(jump_to_letter(
                    backend.clone(),
                    client,
                    uri,
                    offset,
                    era,
                    jump,
                    false,
                ));
                continue;
            }

            Command::BrowseMore => {
                let asking = {
                    let mut browsing = backend.browsing.lock().unwrap();
                    if browsing.fetching_more {
                        continue;
                    }
                    let era = browsing.era;
                    let jump = browsing.jumps;
                    let next = browsing
                        .current()
                        .and_then(|screen| screen.next.clone())
                        .zip(browsing.device)
                        .map(|(next, id)| (next, id, era, jump));
                    if next.is_some() {
                        browsing.fetching_more = true;
                    }
                    next
                };
                let Some((next, id, era, jump)) = asking else {
                    continue;
                };
                let Some(client) = backend.with_entry(id, |e| e.client.clone()) else {
                    backend.browsing.lock().unwrap().fetching_more = false;
                    continue;
                };

                // Off the loop, for the reason spelled out on `BrowseHome`.
                // `fetching_more` already keeps two of these from being in
                // flight at once, and `era` throws away a page that outlived
                // the screen it belonged to.
                let backend = backend.clone();
                tokio::spawn(async move {
                    match client.screen(&next).await {
                        Ok(mut more) => {
                            // The screen this page belongs to may be gone: the
                            // request went out off the loop, and a press or a
                            // staleness refresh in the meantime pushes a crumb of
                            // its own. Grafted on regardless, page two of the
                            // artists appeared under an album, and the album's
                            // own cursor was replaced by the artists' next one, so
                            // scrolling it kept paging the wrong list in.
                            // How much of the screen was already there, so the
                            // artwork walk below can start where this page does.
                            let had;
                            {
                                let mut browsing = backend.browsing.lock().unwrap();
                                browsing.fetching_more = false;
                                // `jumps` as well as `era`: this page continues
                                // the window that was on screen when it was
                                // asked for, and a jump has since replaced that
                                // window with a different part of the same list.
                                if browsing.era != era || browsing.jumps != jump {
                                    return;
                                }
                                let Some(crumb) = browsing.trail.last_mut() else {
                                    return;
                                };
                                // Where the new rows will start, which is the
                                // end of the list rather than of the screen
                                // when a shelf follows it.
                                let window = crumb.screen.window();
                                had = window.map_or(0, |at| {
                                    crumb.screen.sections[..=at]
                                        .iter()
                                        .map(|section| section.items.len())
                                        .sum()
                                });

                                // Worked out before the page is taken apart,
                                // because it needs the page's own rows.
                                //
                                // A list that counts rather than points — a
                                // library's Songs, 2062 of them — gives a
                                // total and no cursor, so the next request is
                                // arithmetic rather than something to follow.
                                // One that points is left to its own cursor.
                                let pointed = more.next.clone();
                                let counted = bluos::screen::continuation(&crumb.uri, &more);

                                // Added to the section already on screen rather
                                // than as one of its own: it is the same list,
                                // continued, and a heading between page one and
                                // page two would be inventing a division the
                                // player never made.
                                //
                                // Only the list's rows, and into the list: a
                                // page asked for by number repeats the sort
                                // options above it, and a shelf after the list
                                // is not where the list goes on.
                                let arriving = more.take_page();
                                let brought = arriving.len();
                                put_page(&mut crumb.screen, arriving, false);
                                let onward = next_after_page(&next, pointed.or(counted), brought);
                                tracing::debug!(
                                    %id,
                                    brought,
                                    total = had + brought,
                                    stopping = onward.is_none(),
                                    "grew the screen"
                                );
                                crumb.screen.next = onward;
                            }
                            backend.publish_browse();
                            tokio::spawn(load_browse_thumbnails(backend.clone(), id, had));
                        }
                        Err(e) => {
                            // Take the cursor away, or the list asks for the
                            // same page forever.
                            //
                            // Clearing `fetching_more` and leaving `next` alone
                            // was survivable while only a scroll could ask: one
                            // gesture, one failed fetch, and a person who gives
                            // up. Anything that re-asks on its own turns it into
                            // a hot loop against the player — 94 requests for
                            // one unparseable continuation, measured.
                            //
                            // Cleared on any error, not only a parse failure. A
                            // timeout might have succeeded on a retry, and the
                            // price of not retrying is that the rest of this
                            // list waits until it is opened again; the price of
                            // retrying blind is a loop nobody can see.
                            let mut browsing = backend.browsing.lock().unwrap();
                            browsing.fetching_more = false;
                            if browsing.era == era
                                && browsing.jumps == jump
                                && let Some(crumb) = browsing.trail.last_mut()
                            {
                                crumb.screen.next = None;
                            }
                            drop(browsing);
                            tracing::debug!(%id, "could not read {next}, so no more of it: {e}");
                        }
                    }
                });
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
                // "Play all" plays on the player browsed, so only while that is
                // the one selected: see `Backend::browsing_selected`.
                let selection = backend.selection();
                let found = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Some(id) = backend.browsing_selected(&browsing, selection) else {
                        let showing = browsing.device.is_some();
                        drop(browsing);
                        if showing {
                            refuse_other_players_screen(&backend);
                        }
                        continue;
                    };
                    browsing
                        .current()
                        .and_then(|screen| screen.header.as_ref())
                        .and_then(|header| header.buttons.get(at))
                        .and_then(|button| button.action.clone())
                        .map(|action| (id, action))
                };
                if let Some((id, action)) = found {
                    tokio::spawn(run_action(backend.clone(), id, action, Arrive::Deeper));
                }
                continue;
            }

            Command::BrowseSection(at) => {
                // As for the header's buttons above.
                let selection = backend.selection();
                let found = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Some(id) = backend.browsing_selected(&browsing, selection) else {
                        let showing = browsing.device.is_some();
                        drop(browsing);
                        if showing {
                            refuse_other_players_screen(&backend);
                        }
                        continue;
                    };
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
                        })
                        .map(|action| (id, action))
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
                // One lock after the other rather than one inside the other:
                // nothing here needs both at once, so this need not join the
                // `arrangement` exception noted on `Backend`.
                let arranged = {
                    let browsing = backend.browsing.lock().unwrap();
                    let Pane::Customise(page) = &browsing.pane else {
                        continue;
                    };
                    let ids: Vec<String> = page.rows.iter().map(|(id, _)| id.clone()).collect();
                    (page.screen.clone(), ids, page.presets)
                };

                // The presets are the player's, so their new order is a
                // request rather than a file.
                if let Some((owner, prid)) = arranged.2 {
                    let slots: Vec<u32> =
                        arranged.1.iter().filter_map(|id| id.parse().ok()).collect();
                    let moves = bluos::screen::reordering(&slots);

                    // To the player the slots were read from, not whichever
                    // is selected now: the permutation only means anything
                    // against the list it was made from.
                    let addressed = (slots.len() == arranged.1.len())
                        .then(|| backend.with_entry(owner, |e| e.client.clone()))
                        .flatten();
                    let Some(client) = addressed else {
                        // Refusing rather than sending part of it: every slot
                        // has to be in the permutation or the ones left out
                        // are deleted.
                        say(&backend.ui, "Could not work out the new order");
                        continue;
                    };

                    backend.browsing.lock().unwrap().pane = Pane::Browse;
                    backend.publish_pane();
                    let backend = backend.clone();
                    tokio::spawn(async move {
                        match client.reorder_presets(prid, &moves).await {
                            Ok(()) => {
                                say(&backend.ui, "Presets reordered");
                                refresh_current(backend).await;
                            }
                            Err(e) => say(&backend.ui, format!("could not reorder them: {e}")),
                        }
                    });
                    continue;
                }
                // What the file can hold of this, worked out before the map
                // takes it so there is something to say about it. The whole
                // arrangement goes into the map either way: it is what was
                // just asked for, and it holds for as long as the window is
                // open whether or not it can be written down.
                let kept = order::savable(&arranged.0, &arranged.1);
                let saved = {
                    let mut orders = backend.orders.lock().unwrap();
                    orders.insert(arranged.0, arranged.1.clone());
                    orders.clone()
                };
                // Written on the spot rather than at exit: the app is a
                // long-running window and there is no other moment that
                // reliably arrives.
                order::save(&saved);

                backend.browsing.lock().unwrap().pane = Pane::Browse;
                backend.publish_pane();
                // Section ids come from the player, and one carrying this
                // file's own structure cannot be written down. Saying it was
                // saved when it was not is worse than it not lasting: the
                // screen comes back the player's way at the next start with
                // nothing to explain it.
                say(
                    &backend.ui,
                    match kept {
                        Some(rows) if rows == arranged.1 => "Home rearranged",
                        Some(_) => "Home rearranged, but not all of it can be saved",
                        None => "Home rearranged, but it cannot be saved",
                    },
                );
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
                    // An upgrading player is not paused, for the same reason
                    // it is not played: it is not listening.
                    .filter(|e| !e.upgrading)
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

        // Every transport, volume, mute, shuffle, repeat, seek and sleep press
        // resolves its player here, which is why the check is here and not on
        // each of them. A player rewriting its own firmware answers none of
        // them: the request either times out against a rebooting speaker or is
        // refused, and either way the press did nothing while looking as
        // though it had.
        //
        // The official controller reaches the same end by keeping an upgrading
        // player out of the group list, so its `currentGroup` goes undefined
        // and every control reading it dies. This app has one funnel rather
        // than that structure, so the funnel is where it goes.
        let Some(client) = backend
            .with_entry(id, |e| (!e.upgrading).then(|| e.client.clone()))
            .flatten()
        else {
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
    // Web pages and nothing else. `open` hands whatever it is given to the
    // desktop's handler for that scheme, so `file://` opens a local file,
    // `smb://` mounts a share, and every other registered protocol is a
    // program started on a player's say-so. Checked here rather than at each
    // call site, because a call site that forgot was exactly how a settings
    // row came to reach this with whatever its `<webview>` said.
    let Some(parsed) = web_page(&url) else {
        tracing::warn!("refusing to open {url}: not a web page");
        say(ui, "That link is not a web page");
        return;
    };
    tracing::info!("opening {url} in a browser");

    // Named in the message rather than left to the user to guess. Several of
    // these call sites carry a URL the player chose — the Info link and the
    // webpage action anywhere at all, a settings `<webview>` only on the
    // player — and a controller sending a browser somewhere on an adopted
    // player's say-so should at least say where. Sign-in pages are genuinely
    // on someone else's site, so refusing off-player destinations outright
    // would break what this is for; showing them costs nothing.
    let host = parsed.host_str().map(str::to_owned);

    match tokio::task::spawn_blocking(move || launch_browser(url)).await {
        Ok(Ok(())) => match host {
            Some(host) => say(ui, format!("Opened {host} in your browser")),
            None => say(ui, "Opened in your browser"),
        },
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

/// Hand a checked web page to the desktop.
#[cfg(not(test))]
fn launch_browser(url: String) -> std::io::Result<()> {
    open::that_detached(url)
}

/// The pages tests would have opened in a browser.
///
/// Written down instead of opened. A test that reaches a browser fallback must
/// not start a browser on the machine running it, and a fallback taken where
/// it should not be is what some of them look for.
#[cfg(test)]
static OPENED_IN_BROWSER: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[cfg(test)]
fn launch_browser(url: String) -> std::io::Result<()> {
    OPENED_IN_BROWSER.lock().unwrap().push(url);
    Ok(())
}

/// The URL, if it is something a browser is for: http or https, with a host.
fn web_page(url: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
}

#[cfg(test)]
mod web_page_tests {
    use super::web_page;

    /// What reaches the desktop's opener. A player picks several of these
    /// URLs, and the opener starts whatever handles the scheme.
    #[test]
    fn only_a_web_page_goes_to_the_browser() {
        // Off the player is fine: sign-in pages live on the service's site.
        assert!(web_page("https://accounts.example.com/signin").is_some());
        assert!(web_page("http://10.0.0.155/sharecfg?noheader=1").is_some());

        for url in [
            "file:///etc/passwd",
            "smb://10.0.0.155/share",
            "ms-settings:network",
            "javascript:alert(1)",
            "/sharecfg?noheader=1",
            "http://",
            "",
        ] {
            assert!(web_page(url).is_none(), "{url:?} would have been opened");
        }
    }
}

/// Which playlist a row index means, and which service it belongs to.
///
/// The rows are a flattened walk of the groups, so this walks them the same
/// way. Returns `None` for the row that makes a new one, which the caller
/// distinguishes from a miss by the index still being in range.
fn playlist_at(
    options: &bluos::playlists::AddToPlaylist,
    want: usize,
) -> Option<(Option<String>, Option<bluos::playlists::Playlist>)> {
    let mut at = 0;
    for group in &options.groups {
        for playlist in &group.playlists {
            if at == want {
                return Some((group.service.clone(), Some(playlist.clone())));
            }
            at += 1;
        }
        if group.can_create {
            if at == want {
                return Some((group.service.clone(), None));
            }
            at += 1;
        }
    }
    None
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
mod form_value_tests {
    use super::{drawn_choice, form_values};

    #[test]
    fn a_form_is_seeded_with_what_the_page_showed() {
        let form = bluos::forms::parse(
            r#"<form action="/x">
                 <input type="checkbox" name="off" value="1"/>
                 <input type="checkbox" name="ticked" value="1" checked/>
                 <select name="essid"><option value="one">one</option><option value="two">two</option></select>
                 <input type="password" name="key" value="kept"/>
               </form>"#,
        )
        .pop()
        .expect("a form");
        let values = form_values(&form);

        // A box's own value is what it sends, not whether it is ticked.
        assert_eq!(values.get("off").map(String::as_str), Some(""));
        assert_eq!(values.get("ticked").map(String::as_str), Some("on"));
        // Nothing selected is the first network, as drawn.
        assert_eq!(values.get("essid").map(String::as_str), Some("one"));
        assert!(!values.contains_key("key"), "a password is never seeded");
    }

    /// A radio group with nothing checked sends nothing, so it is drawn with
    /// nothing chosen rather than on a first choice the form will not post.
    #[test]
    fn a_radio_group_with_nothing_checked_is_drawn_on_nothing() {
        let form = bluos::forms::parse(
            r#"<form action="/x">
                 <input type="radio" name="quality" value="MP3"/>
                 <input type="radio" name="quality" value="CD"/>
                 <select name="essid"><option value="one">one</option><option value="two">two</option></select>
                 <input type="radio" name="rate" value="44"/>
                 <input type="radio" name="rate" value="48" checked/>
               </form>"#,
        )
        .pop()
        .expect("a form");
        let values = form_values(&form);
        let drawn = |name: &str| {
            let field = form.fields.iter().find(|f| f.name == name).expect(name);
            drawn_choice(field, values.get(name).map(String::as_str))
        };

        assert!(
            !values.contains_key("quality"),
            "nothing checked holds nothing"
        );
        assert_eq!(drawn("quality"), -1);
        assert_eq!(drawn("essid"), 0);
        assert_eq!(drawn("rate"), 1);
    }

    /// A radio whose own value is empty is still a radio nobody checked. Held
    /// as an empty string, the group was drawn on it and posted `mode=`.
    #[test]
    fn an_empty_valued_radio_is_not_chosen_until_it_is_picked() {
        let form = bluos::forms::parse(
            r#"<form action="/x">
                 <input type="radio" name="mode" value=""/>
                 <input type="radio" name="mode" value="x"/>
                 <input type="radio" name="kept" value="" checked/>
                 <input type="radio" name="kept" value="y"/>
               </form>"#,
        )
        .pop()
        .expect("a form");
        let mut values = form_values(&form);
        let field = |name: &str| form.fields.iter().find(|f| f.name == name).expect(name);

        assert!(!values.contains_key("mode"));
        assert_eq!(
            drawn_choice(field("mode"), values.get("mode").map(String::as_str)),
            -1
        );
        // Checked, the empty radio is a choice like any other.
        assert_eq!(values.get("kept").map(String::as_str), Some(""));
        assert_eq!(
            drawn_choice(field("kept"), values.get("kept").map(String::as_str)),
            0
        );

        // Picking it is what `Edit::Choose` does: the value goes into the map.
        values.insert("mode".to_owned(), String::new());
        assert_eq!(
            drawn_choice(field("mode"), values.get("mode").map(String::as_str)),
            0
        );
    }
}

#[cfg(test)]
mod queue_paging_tests {
    use super::page_belongs;

    #[test]
    fn the_next_window_of_the_same_queue_belongs() {
        assert!(page_belongs(Some(856), 500, Some(856), Some(856), 500, 500));
    }

    #[test]
    fn a_page_of_a_queue_that_has_been_replaced_does_not() {
        // The rows held are the new queue, the page is the old one. Grafting
        // it would put one playlist under another.
        assert!(!page_belongs(
            Some(857),
            500,
            Some(856),
            Some(856),
            500,
            500
        ));
        // And the other way about: the page is for a queue nobody holds.
        assert!(!page_belongs(
            Some(856),
            500,
            Some(856),
            Some(857),
            500,
            500
        ));
    }

    #[test]
    fn a_queue_with_no_id_is_never_grown() {
        // Two queues with no id are indistinguishable, so there is no way to
        // know whether a page belongs to the one on screen.
        assert!(!page_belongs(None, 500, None, None, 500, 500));
    }

    #[test]
    fn a_page_that_starts_somewhere_else_does_not_belong() {
        // Something already grew the list past where this page begins, so it
        // would duplicate rows.
        assert!(!page_belongs(
            Some(856),
            700,
            Some(856),
            Some(856),
            500,
            500
        ));
        // Or the list shrank, and it would leave a hole.
        assert!(!page_belongs(
            Some(856),
            300,
            Some(856),
            Some(856),
            500,
            500
        ));
    }

    #[test]
    fn an_empty_page_is_the_end() {
        assert!(!page_belongs(Some(856), 500, Some(856), Some(856), 0, 500));
    }

    #[test]
    fn the_first_growth_starts_where_the_first_window_ended() {
        assert!(page_belongs(Some(1), 500, Some(1), Some(1), 42, 500));
    }
}

#[cfg(test)]
mod index_tests {
    use super::*;

    /// A plain list of rows, written out rather than derived from `Default`:
    /// only the title and `heading` matter here, and spelling the rest out
    /// keeps the derive off a type the window depends on.
    fn row_of(title: &str) -> BrowseData {
        BrowseData {
            index: 0,
            plays: false,
            title: title.to_owned(),
            subtitle: String::new(),
            track: String::new(),
            quality: String::new(),
            action: String::new(),
            cover: None,
            glyph: None,
            heading: false,
            actionable: true,
            playing: false,
            selected: false,
            has_menu: false,
        }
    }

    fn one_block(titles: &[String]) -> Vec<BlockData> {
        vec![BlockData {
            kind: 0,
            title: String::new(),
            action: String::new(),
            section: 0,
            rows: titles.iter().map(|t| row_of(t)).collect(),
        }]
    }

    /// Fifty-odd sorted names, the shape of the Genres list: a number first,
    /// then two of every letter.
    fn genres() -> Vec<String> {
        let mut out = vec!["80s".to_owned(), "A Cappella".to_owned()];
        for letter in 'B'..='Z' {
            out.push(format!("{letter}and"));
            out.push(format!("{letter}lues"));
        }
        out
    }

    #[test]
    fn a_straggler_joins_the_bottom_of_its_own_letter() {
        // The player files by character code, so these land after Z.
        let mut names = genres();
        names.push("classical".to_owned());
        names.push("\u{C9}lectronique".to_owned());

        let mut blocks = one_block(&names);
        collate(&mut blocks);
        let after: Vec<&str> = blocks[0].rows.iter().map(|r| r.title.as_str()).collect();

        // "classical" now sits with the Cs, after the ones already there...
        let c_at = after
            .iter()
            .position(|t| *t == "classical")
            .expect("classical");
        assert_eq!(after[c_at - 1], "Clues");
        assert_eq!(after[c_at + 1], "Dand");

        // ...and the accented entry with the Es rather than at the very end.
        let e_at = after
            .iter()
            .position(|t| *t == "\u{C9}lectronique")
            .expect("it");
        assert_eq!(after[e_at - 1], "Elues");
        assert_eq!(after[e_at + 1], "Fand");
        assert_ne!(after.last(), Some(&"\u{C9}lectronique"));

        // Nothing was lost or duplicated.
        assert_eq!(after.len(), names.len());
    }

    #[test]
    fn a_list_that_is_not_alphabetical_is_never_reordered() {
        // An album's tracks are in track order. Alphabetizing forty of them
        // would be vandalism, so the rule is tested before anything moves.
        let names = genres();
        let mut shuffled = Vec::new();
        let (mut front, mut back) = (0, names.len() - 1);
        while front <= back {
            shuffled.push(names[back].clone());
            if front != back {
                shuffled.push(names[front].clone());
            }
            front += 1;
            back -= 1;
        }
        let before = shuffled.clone();
        let mut blocks = one_block(&shuffled);
        collate(&mut blocks);
        let after: Vec<String> = blocks[0].rows.iter().map(|r| r.title.clone()).collect();
        assert_eq!(after, before);
    }

    #[test]
    fn a_short_list_and_a_shelf_are_left_alone() {
        let short: Vec<String> = ["zebra", "apple", "mango"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let mut blocks = one_block(&short);
        collate(&mut blocks);
        assert_eq!(blocks[0].rows[0].title, "zebra");

        // A shelf of tiles has no single column to file rows into.
        let names = genres();
        let mut shelf = one_block(&names);
        shelf[0].kind = 1;
        let first = shelf[0].rows[0].title.clone();
        collate(&mut shelf);
        assert_eq!(shelf[0].rows[0].title, first);
    }

    #[test]
    fn a_list_with_a_heading_is_left_alone() {
        let mut names = genres();
        names.push("classical".to_owned());
        let mut blocks = one_block(&names);
        blocks[0].rows[0].heading = true;
        collate(&mut blocks);
        // The straggler stayed put rather than the heading being sorted away.
        assert_eq!(
            blocks[0].rows.last().expect("a last row").title,
            "classical"
        );
    }

    #[test]
    fn collating_puts_the_letters_back_in_one_piece() {
        // After collation the bar has one stop per letter again, because the
        // stragglers are no longer stranded past Z.
        let mut names = genres();
        names.push("classical".to_owned());
        names.push("\u{C9}lectronique".to_owned());
        let mut blocks = one_block(&names);
        collate(&mut blocks);

        let index = derived_index(&blocks);
        let c = &index.iter().find(|(k, _)| k == "C").expect("a C").1;
        let e = &index.iter().find(|(k, _)| k == "E").expect("an E").1;
        assert_eq!(c.len(), 1);
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn a_long_unindexed_list_gets_letters_off_its_rows() {
        let index = derived_index(&one_block(&genres()));

        // A digit files under #, exactly as the player's own indexes do.
        assert_eq!(index[0].0, "#");
        assert_eq!(index[1].0, "A");
        assert_eq!(index[2].0, "B");
        // Each letter named once: # and A, then B through Z.
        assert_eq!(index.len(), 27);
    }

    #[test]
    fn a_letter_points_at_the_row_it_starts_on() {
        let names = genres();
        let index = derived_index(&one_block(&names));

        // Row 0 is "80s", row 1 "A Cappella", row 2 the first B.
        assert_eq!(index[0].1, vec![0.0]);
        assert_eq!(index[1].1, vec![ROW_PX]);
        assert_eq!(index[2].1, vec![2.0 * ROW_PX]);

        // And the last letter is where its row is, not where the list ends:
        // Z has two rows, so it starts two from the bottom.
        let (key, stops) = index.last().expect("a last letter");
        assert_eq!(key, "Z");
        assert_eq!(*stops, vec![(names.len() - 2) as f32 * ROW_PX]);
    }

    #[test]
    fn a_heading_takes_space_without_taking_a_letter() {
        let mut names = genres();
        names.insert(0, "GENRES".to_owned());
        let mut blocks = one_block(&names);
        blocks[0].rows[0].heading = true;

        let index = derived_index(&blocks);

        // The heading is not a letter of its own...
        assert_eq!(index[0].0, "#");
        // ...but everything below it is pushed down by its lesser height.
        assert_eq!(index[0].1, vec![HEADING_PX]);
        assert_eq!(index[1].1, vec![HEADING_PX + ROW_PX]);
    }

    #[test]
    fn a_short_list_gets_no_bar() {
        // The folder root is seven entries; letters down the side of it would
        // be furniture.
        let short: Vec<String> = ["albums", "classical", "house", "intl"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert!(derived_index(&one_block(&short)).is_empty());
    }

    #[test]
    fn tiles_and_mixed_screens_get_no_bar() {
        // A tile is not `ROW_PX` tall, and a screen of several blocks has no
        // single column for a letter to scroll to.
        let names = genres();
        let mut shelf = one_block(&names);
        shelf[0].kind = 1;
        assert!(derived_index(&shelf).is_empty());

        let mut two = one_block(&names);
        two.extend(one_block(&names));
        assert!(derived_index(&two).is_empty());
    }

    #[test]
    fn accents_file_under_the_letter_they_are_alphabetized_by() {
        assert_eq!(leading_key("\u{C9}dith Piaf"), "E");
        assert_eq!(leading_key("Sigur R\u{F3}s"), "S");
        assert_eq!(leading_key("Mot\u{F6}rhead"), "M");
        // The ones with no decomposition of their own.
        assert_eq!(leading_key("\u{C6}on"), "A");
        assert_eq!(leading_key("\u{141}\u{F3}d\u{17A}"), "L");
        assert_eq!(leading_key("\u{D8}rsted"), "O");
        // And a script with no Latin base keeps itself, to be bucketed later.
        assert_eq!(leading_key("\u{6797}"), "\u{6797}");
    }

    #[test]
    fn many_scripts_become_one_letter_on_the_bar() {
        let keys = ["#", "A", "Z", "\u{6797}", "\u{68EE}", "\u{4E2D}"]
            .iter()
            .map(|k| (*k).to_owned())
            .collect();
        assert_eq!(bar_keys(keys), vec!["#", "A", "Z", "*"]);

        // A bar that needs no catch-all does not grow one.
        let latin = ["#", "A", "B"].iter().map(|k| (*k).to_owned()).collect();
        assert_eq!(bar_keys(latin), vec!["#", "A", "B"]);
    }

    #[test]
    fn a_script_without_case_keeps_its_own_letter() {
        assert_eq!(leading_key("\u{6797}\u{61B6}\u{84EE}"), "\u{6797}");
        assert_eq!(leading_key("the xx"), "T");
        assert_eq!(leading_key("  spaced"), "S");
        assert_eq!(leading_key("2Pac"), "#");
        assert_eq!(leading_key(""), "#");
    }
    #[test]
    fn a_letter_that_comes_back_keeps_both_places() {
        // The player's own Genres list, in shape: uppercase A-Z, then the
        // entries it sorts after Z — lowercase first, then accented. All of
        // them are one letter on the bar, and that letter holds every place it
        // starts.
        let mut names = genres();
        names.push("classical".to_owned());
        names.push("hip hop / rap".to_owned());
        names.push("\u{C9}lectronique".to_owned());

        let index = derived_index(&one_block(&names));
        let keys: Vec<&str> = index.iter().map(|(k, _)| k.as_str()).collect();

        // One letter each, and nothing after Z.
        assert_eq!(keys.first(), Some(&"#"));
        assert_eq!(keys.last(), Some(&"Z"));
        assert_eq!(keys.iter().filter(|k| **k == "C").count(), 1);
        assert!(!keys.contains(&"c"));

        // C leads with the uppercase run near the top and still knows where
        // "classical" is, three rows from the bottom.
        let c = &index.iter().find(|(k, _)| k == "C").expect("a C").1;
        assert_eq!(c.len(), 2);
        assert!(c[0] < 10.0 * ROW_PX);
        assert_eq!(c[1], (names.len() - 3) as f32 * ROW_PX);

        // And E knows about the accented entry, which is the very last row.
        let e = &index.iter().find(|(k, _)| k == "E").expect("an E").1;
        assert_eq!(e.len(), 2);
        assert_eq!(e[1], (names.len() - 1) as f32 * ROW_PX);
    }

    #[test]
    fn pressing_a_letter_again_moves_to_its_next_place() {
        let index = vec![
            ("A".to_owned(), vec![0.0]),
            ("E".to_owned(), vec![100.0, 900.0]),
        ];
        assert_eq!(stops_derived(&index, "E"), vec![100.0, 900.0]);
        // Typed lowercase, which is how a keyboard sends it.
        assert_eq!(stops_derived(&index, "e"), vec![100.0, 900.0]);

        let mut browsing = Browsing::default();
        assert_eq!(browsing.next_stop("E", 2), Some(0));
        assert_eq!(browsing.next_stop("E", 2), Some(1));
        // Round again rather than stopping at the end.
        assert_eq!(browsing.next_stop("E", 2), Some(0));
        // A different letter starts from its own beginning.
        assert_eq!(browsing.next_stop("A", 1), Some(0));
        assert_eq!(browsing.next_stop("E", 2), Some(0));
        // And a letter with nowhere to go is not a jump at all.
        assert_eq!(browsing.next_stop("Q", 0), None);
    }

    #[test]
    fn leaving_the_screen_starts_the_cycle_over() {
        let mut browsing = Browsing::default();
        assert_eq!(browsing.next_stop("E", 2), Some(0));
        assert_eq!(browsing.next_stop("E", 2), Some(1));
        // Walking into another screen and back must not resume halfway.
        browsing.moved_on();
        assert_eq!(browsing.next_stop("E", 2), Some(0));
    }

    #[test]
    fn a_keystroke_reaches_a_letter_the_bar_only_has_in_capitals() {
        let index = vec![
            ("#".to_owned(), vec![0.0]),
            ("A".to_owned(), vec![52.0]),
            ("C".to_owned(), vec![104.0]),
        ];
        assert_eq!(stops_derived(&index, "c"), vec![104.0]);
        assert_eq!(stops_derived(&index, "C"), vec![104.0]);
        // And a letter on no list is nowhere to go.
        assert!(stops_derived(&index, "q").is_empty());
    }

    #[test]
    fn the_catch_all_gathers_every_letter_behind_it() {
        use bluos::screen::Jump;
        let sent = vec![
            Jump {
                key: "A".to_owned(),
                offset: 0,
            },
            Jump {
                key: "Z".to_owned(),
                offset: 400,
            },
            Jump {
                key: "\u{6797}".to_owned(),
                offset: 440,
            },
            Jump {
                key: "\u{68EE}".to_owned(),
                offset: 446,
            },
        ];
        // What the bar drew as one `*` cycles through both.
        assert_eq!(stops_sent(&sent, "*"), vec![440, 446]);
        // An ordinary letter is itself, however it was typed.
        assert_eq!(stops_sent(&sent, "a"), vec![0]);
        assert!(stops_sent(&sent, "q").is_empty());

        // On a bar with nothing behind it, `*` is nowhere to go.
        let latin = vec![Jump {
            key: "A".to_owned(),
            offset: 0,
        }];
        assert!(stops_sent(&latin, "*").is_empty());
    }
}

#[cfg(test)]
mod tests {

    /// An id read off a URI is still encoded, and the client encodes what it
    /// is given, so it has to come out of here decoded or it is encoded twice.
    #[test]
    fn a_settings_page_id_comes_out_of_its_uri_decoded() {
        use super::settings_page;

        assert_eq!(
            settings_page("/Settings?id=capture"),
            Some(Some("capture".to_owned()))
        );
        assert_eq!(
            settings_page("/Settings?id=a%20b%26c&other=1"),
            Some(Some("a b&c".to_owned()))
        );
        assert_eq!(
            settings_page("/Settings"),
            Some(None),
            "the top of the menu"
        );
        assert_eq!(
            settings_page("/Browse?id=capture"),
            None,
            "not settings at all"
        );
    }

    /// The window, driven the way a hand drives it.
    ///
    /// One test rather than several: the testing backend may only be installed
    /// once in a process, and `cargo test` runs each `#[test]` on its own
    /// thread, so a second `init_no_event_loop` would panic.
    ///
    /// What this covers is the seam nothing else does — the callbacks between
    /// a press and a `Command`. Three of the regressions found by hand on
    /// 2026-08-23/24 lived exactly there and were invisible to every other
    /// test in this repo, because they were reached only by pressing
    /// something.
    #[test]
    fn a_press_becomes_the_command_it_should() {
        i_slint_backend_testing::init_no_event_loop();

        let ui = AppWindow::new().expect("a window");
        let (tx, mut rx) = mpsc::unbounded_channel();
        wire(&ui, tx);

        // A settings-shaped page, numbered the way the alarms editor numbers
        // its rows. The exact value is the point: these are deliberately far
        // above any real row, because the callback used to clamp its argument
        // at zero and every one of them arrived as "the first row".
        let rows = vec![
            SettingItem {
                index: ALARM_SAVE as i32,
                label: "Create".into(),
                control: "button".into(),
                value: "Save".into(),
                available: true,
                ..Default::default()
            },
            SettingItem {
                index: NEW_ALARM as i32,
                label: "New alarm…".into(),
                control: "link".into(),
                available: true,
                ..Default::default()
            },
        ];
        ui.set_settings(ModelRc::new(VecModel::from(rows)));
        ui.set_in_settings(true);

        // Pressing the second row must say the second row, not row zero.
        let row =
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "New alarm…")
                .next()
                .or_else(|| {
                    i_slint_backend_testing::ElementQuery::from_root(&ui)
                        .match_descendants()
                        .match_predicate(|e| {
                            e.accessible_label().is_some_and(|l| l == "New alarm…")
                        })
                        .find_first()
                });

        match row {
            Some(row) => {
                row.invoke_accessible_default_action();
                let sent = rx.try_recv();
                assert!(
                    matches!(sent, Ok(Command::BrowseActivate(at)) if at == NEW_ALARM),
                    "a press on the last row must carry its own index, got {sent:?}"
                );
            }
            // The rows are drawn by a component that does not publish an
            // accessible label. Rather than assert nothing, drive the callback
            // the window itself would: this still crosses the boundary that
            // clamped the index, which is the thing under test.
            None => {
                ui.invoke_browse_activate(NEW_ALARM as i32);
                let sent = rx.try_recv();
                assert!(
                    matches!(sent, Ok(Command::BrowseActivate(at)) if at == NEW_ALARM),
                    "the callback must not clamp a high row index, got {sent:?}"
                );

                ui.invoke_browse_activate(ALARM_SAVE as i32);
                assert!(matches!(
                    rx.try_recv(),
                    Ok(Command::BrowseActivate(at)) if at == ALARM_SAVE
                ));
            }
        }

        // And a negative one — which is what the window sends to dismiss a
        // dialog — must not become row zero either.
        ui.invoke_dialog_press(-1);
        assert!(
            matches!(rx.try_recv(), Ok(Command::DialogPress(at)) if at == usize::MAX),
            "a dismissal must not read as pressing the first button"
        );
    }

    /// Which presses take the speaker away from what it was doing.
    ///
    /// Every URI here came off a Powernode. The distinction is not "does it
    /// play" — a track and a station both play — but "does it leave no way
    /// back to what was on", which is only true of an input.
    #[test]
    fn only_an_input_is_worth_asking_about() {
        let action = |uri: &str, kind| bluos::screen::Action {
            kind,
            uri: Some(uri.to_owned()),
            ..Default::default()
        };
        let link = bluos::screen::ActionKind::PlayerLink;

        // The physical inputs, as Home and the sidebar both write them.
        assert!(switches_input(&action(
            "/Play?url=Capture%3Ahw%3Aimxspdif%2C0%2F1%2F25%2F2%3Fid%3Dinput4",
            link
        )));
        assert!(switches_input(&action(
            "/Play?url=Capture%3Abluez%3Abluetooth",
            link
        )));

        // A station plays, but through the player's own wrapper, and pressing
        // one is not giving up what you were listening to in the same way.
        assert!(!switches_input(&action(
            "/ui/prf?u=%2FPlay%3Furl%3DAirable%253Aradio%253Ahttps%253A%252F%252Fx.io%252Fs%252F1",
            link
        )));
        // A track, same shape.
        assert!(!switches_input(&action(
            "/ui/prf?u=%2FAdd%3Fplaynow%3D1%26file%3D%252Fvar%252Fx.flac",
            link
        )));
        // An album opens a screen.
        assert!(!switches_input(&action(
            "/ui/browseContext?service=LocalMusic&type=Album",
            bluos::screen::ActionKind::Browse
        )));
        // The right shape of URI but the wrong kind of action.
        assert!(!switches_input(&action(
            "/Play?url=Capture%3Abluez%3Abluetooth",
            bluos::screen::ActionKind::Browse
        )));
    }

    /// The one string on the alarms list that is not the player's wording.
    #[test]
    fn a_week_in_words() {
        let days = |on: [bool; 7]| repeat_summary(&on);
        assert_eq!(days([false; 7]), "Once", "no day set is once, not never");
        assert_eq!(days([true; 7]), "Every day");
        assert_eq!(
            days([false, true, true, true, true, true, false]),
            "Weekdays"
        );
        assert_eq!(
            days([true, false, false, false, false, false, true]),
            "Weekends"
        );
        // Anything else is spelled out, Sunday first.
        assert_eq!(
            days([false, true, false, true, false, true, false]),
            "Mon, Wed, Fri"
        );
        assert_eq!(
            days([true, false, false, false, false, false, false]),
            "Sun"
        );
        // Weekdays plus one is not weekdays.
        assert_eq!(
            days([true, true, true, true, true, true, false]),
            "Sun, Mon, Tue, Wed, Thu, Fri"
        );
    }

    /// The two kinds an alarm comes in, and what tells them apart.
    ///
    /// Only `end` does. The player derives a duration from it — a schedule
    /// saved for 07:00 to 17:30 came back carrying `duration="630"` — so
    /// sending both would be asking it two questions at once.
    #[test]
    fn a_schedule_is_an_alarm_with_a_finishing_time() {
        let alarm = bluos::alarms::Alarm {
            hour: 7,
            duration: 15,
            ..Default::default()
        };
        assert!(!alarm.is_schedule());

        let schedule = bluos::alarms::Alarm {
            end: Some("1730".to_owned()),
            ..alarm.clone()
        };
        assert!(schedule.is_schedule());
        assert_eq!(schedule_end(&schedule), (17, 30));

        // An empty string is not a finishing time, and must not make one.
        let neither = bluos::alarms::Alarm {
            end: Some(String::new()),
            ..alarm
        };
        assert_eq!(
            schedule_end(&neither),
            (9, 0),
            "unreadable falls back rather than panicking"
        );
    }

    /// A schedule's finishing time is `"HHmm"` with no separator.
    #[test]
    fn an_end_time_read_back_out_of_the_wire() {
        let at = |end: Option<&str>| {
            schedule_end(&bluos::alarms::Alarm {
                end: end.map(str::to_owned),
                ..Default::default()
            })
        };
        assert_eq!(at(Some("1730")), (17, 30));
        assert_eq!(at(Some("0005")), (0, 5));
        // Nothing set yet: the field still has to open on something.
        assert_eq!(at(None), (9, 0));
        // Not a time. Better a visible nine o'clock than a panic.
        assert_eq!(at(Some("")), (9, 0));
        assert_eq!(at(Some("abcd")), (9, 0));
        assert_eq!(at(Some("7")), (9, 0));
    }

    #[test]
    fn the_players_chrome_reads_american() {
        assert_eq!(american("Favourites"), "Favorites");
        assert_eq!(american("Customise Home"), "Customize Home");
        assert_eq!(american("Added to favourites"), "Added to favorites");
        assert_eq!(american("My Favourites"), "My Favorites");
        assert_eq!(american("Remove favourite"), "Remove favorite");
        // Both words in one label, which is what the Favourites screen sends.
        assert_eq!(
            american("Customise Favourites"),
            "Customize Favorites",
            "the `customiseScreen` row on /ui/Favourites"
        );
    }

    /// The rows that get the transform and the rows that must not.
    ///
    /// This is the whole of the rule: the affordance the player draws at the
    /// end of a screen is the app's own furniture, and everything else on a
    /// browse screen is the name of something and stays as it was written.
    /// `customiseScreen` reaching the window as an ordinary row, with its
    /// title untouched, is what put "Customise Home" on screen after the
    /// chrome elsewhere had been changed.
    #[test]
    fn only_the_screens_own_furniture_is_respelled() {
        assert!(is_chrome_row(ItemKind::Customise));
        assert!(is_chrome_row(ItemKind::Footer));

        for kind in [
            ItemKind::Item,
            ItemKind::Source,
            ItemKind::Input,
            ItemKind::Service,
            ItemKind::Teaser,
            ItemKind::Thumbnail,
        ] {
            assert!(
                !is_chrome_row(kind),
                "{kind:?} names something on the system and must keep its own spelling"
            );
        }
    }

    #[test]
    fn a_word_that_merely_contains_one_is_left_alone() {
        // The reason this matches whole words: a band, an album, a label.
        assert_eq!(american("Colourbox"), "Colourbox");
        assert_eq!(american("Favouritism"), "Favouritism");
        // Not on the list at all, because a record is far likelier to be
        // called one of these than the player is to write it.
        assert_eq!(american("Colour Haze"), "Colour Haze");
        assert_eq!(american("The Centre Cannot Hold"), "The Centre Cannot Hold");
        // And nothing to do at all.
        assert_eq!(american("Radio Paradise"), "Radio Paradise");
        assert_eq!(american(""), "");
    }

    #[test]
    fn punctuation_and_case_survive() {
        assert_eq!(american("Favourites,"), "Favorites,");
        assert_eq!(american("Add to playlist…"), "Add to playlist…");
        assert_eq!(american("FAVOURITES"), "FAVORITES");
        assert_eq!(american("Café favourite"), "Café favorite");
        // Content that happens to look like chrome is the accepted cost.
        assert_eq!(
            american("Favourite Worst Nightmare"),
            "Favorite Worst Nightmare"
        );
    }

    #[test]
    fn a_picker_action_names_its_service() {
        assert_eq!(
            service_named("/ui/action?CfavouritesService=TuneIn").as_deref(),
            Some("TuneIn")
        );
        assert_eq!(
            service_named("/ui/action?x=1&CfavouritesService=LocalMusic&y=2").as_deref(),
            Some("LocalMusic")
        );
        // Not every player-link is a picker.
        assert_eq!(service_named("/ui/action?CBQ=true"), None);
        assert_eq!(service_named("/Play"), None);
        assert_eq!(service_named("/ui/action?CfavouritesService="), None);
    }

    #[test]
    fn the_service_replaces_one_already_on_the_address() {
        assert_eq!(
            with_service("/ui/Favourites", "TuneIn"),
            "/ui/Favourites?service=TuneIn"
        );
        // Switching twice must not leave both behind.
        assert_eq!(
            with_service("/ui/Favourites?service=LocalMusic", "TuneIn"),
            "/ui/Favourites?service=TuneIn"
        );
        assert_eq!(
            with_service("/ui/Favourites?page=2&service=LocalMusic", "TuneIn"),
            "/ui/Favourites?page=2&service=TuneIn"
        );
    }
    use super::*;

    /// The browse memo skips a redraw when the fingerprint matches, so a field
    /// the window draws but the fingerprint omits is a cell that stops
    /// updating. This is the guard on that: change one field at a time and the
    /// fingerprint has to move.
    #[test]
    fn every_drawn_browse_field_is_in_the_fingerprint() {
        use std::hash::Hasher;

        let print = |row: &BrowseData| {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            browse_row_fingerprint(row, &mut hash);
            hash.finish()
        };

        let base = BrowseData {
            index: 1,
            plays: false,
            title: "Title".to_owned(),
            subtitle: "Sub".to_owned(),
            track: "3".to_owned(),
            quality: "FLAC".to_owned(),
            action: "Manage".to_owned(),
            cover: None,
            glyph: None,
            heading: false,
            actionable: true,
            playing: false,
            selected: false,
            has_menu: false,
        };
        let baseline = print(&base);

        let variants: Vec<(&str, BrowseData)> = vec![
            (
                "index",
                BrowseData {
                    index: 2,
                    ..base.clone()
                },
            ),
            (
                "plays",
                BrowseData {
                    plays: true,
                    ..base.clone()
                },
            ),
            (
                "title",
                BrowseData {
                    title: "Other".to_owned(),
                    ..base.clone()
                },
            ),
            (
                "subtitle",
                BrowseData {
                    subtitle: "Other".to_owned(),
                    ..base.clone()
                },
            ),
            (
                "track",
                BrowseData {
                    track: "4".to_owned(),
                    ..base.clone()
                },
            ),
            (
                "quality",
                BrowseData {
                    quality: "MQA".to_owned(),
                    ..base.clone()
                },
            ),
            (
                "action",
                BrowseData {
                    action: "Customise".to_owned(),
                    ..base.clone()
                },
            ),
            (
                "glyph",
                BrowseData {
                    glyph: Some(Glyph::Play),
                    ..base.clone()
                },
            ),
            (
                "heading",
                BrowseData {
                    heading: true,
                    ..base.clone()
                },
            ),
            (
                "actionable",
                BrowseData {
                    actionable: false,
                    ..base.clone()
                },
            ),
            (
                "playing",
                BrowseData {
                    playing: true,
                    ..base.clone()
                },
            ),
            (
                "selected",
                BrowseData {
                    selected: true,
                    ..base.clone()
                },
            ),
            (
                "has_menu",
                BrowseData {
                    has_menu: true,
                    ..base.clone()
                },
            ),
        ];

        for (field, row) in variants {
            assert_ne!(
                print(&row),
                baseline,
                "changing {field} did not change the fingerprint, so the row \
                 would stop redrawing when it changes"
            );
        }
    }

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

    #[test]
    fn a_page_that_brought_rows_keeps_paging() {
        assert_eq!(
            next_after_page("/ui/x?page=2", Some("/ui/x?page=3".to_owned()), 30),
            Some("/ui/x?page=3".to_owned())
        );
    }

    #[test]
    fn a_page_that_brought_nothing_is_the_end() {
        // Even when the player hands over another cursor. Asking again gets
        // the same nothing, and with a height trigger that is a loop.
        assert_eq!(
            next_after_page("/ui/x?page=9", Some("/ui/x?page=10".to_owned()), 0),
            None
        );
    }

    #[test]
    fn a_cursor_that_repeats_itself_is_the_end() {
        // The player pointing at the page just fetched. Following it walks the
        // same rows forever.
        assert_eq!(
            next_after_page("/ui/x?page=4", Some("/ui/x?page=4".to_owned()), 30),
            None
        );
    }

    #[test]
    fn a_page_with_no_cursor_is_the_end_as_it_always_was() {
        assert_eq!(next_after_page("/ui/x?page=4", None, 30), None);
    }

    fn song(title: &str) -> bluos::screen::Item {
        bluos::screen::Item {
            title: Some(title.to_owned()),
            ..Default::default()
        }
    }

    /// A library's Songs as the player draws it, holding `held` of 2062: its
    /// sort options above the counted list and a shelf after it.
    fn songs_crumb(held: usize) -> Crumb {
        use bluos::screen::Section;

        let rows = |name: &str, n: usize| (0..n).map(|i| song(&format!("{name} {i}"))).collect();
        Crumb {
            uri: "/ui/browseGrouped?type=Song".to_owned(),
            screen: Screen {
                offset: Some(0),
                total: Some(2062),
                sections: vec![
                    Section {
                        items: rows("sort", 3),
                        ..Default::default()
                    },
                    Section {
                        paged: true,
                        items: rows("song", held),
                        ..Default::default()
                    },
                    Section {
                        kind: SectionKind::Row,
                        items: rows("shelf", 1),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            query: None,
            page: 30,
        }
    }

    #[test]
    fn end_aims_one_page_back_from_the_end_however_much_is_held() {
        let mut crumb = songs_crumb(90);
        // Ninety held back from the end aimed at 1972, and the window it
        // brought ended thirty short of the last song.
        assert_eq!(end_offset(&crumb, true), Some(2032));
        assert_eq!(end_offset(&crumb, false), Some(0));

        // A list that only points has no end to aim at, though it has a top.
        crumb.screen.total = None;
        assert_eq!(end_offset(&crumb, true), None);
        assert_eq!(end_offset(&crumb, false), Some(0));
    }

    /// The last thirty songs, as End brings them back: a counted window with
    /// no cursor and, for a list the player sent no index for, no index.
    fn last_window() -> Screen {
        use bluos::screen::Section;

        Screen {
            offset: Some(2032),
            total: Some(2062),
            sections: vec![Section {
                paged: true,
                items: (2032..2062).map(|i| song(&format!("song {i}"))).collect(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_counted_list_on_its_last_window_is_still_a_window() {
        let mut browsing = Browsing::default();
        browsing.trail.push(songs_crumb(90));

        // End, as the command claims it.
        browsing.jumps += 1;
        let (era, jump) = (browsing.era, browsing.jumps);
        let offset = end_offset(browsing.trail.last().unwrap(), true).unwrap();
        assert!(land_jump(&mut browsing, last_window(), offset, era, jump));

        let crumb = browsing.trail.last().unwrap();
        // Nothing left to page on and no index, which read as a list held
        // whole: Home scrolled these thirty rows instead of fetching the top.
        assert_eq!(crumb.screen.next, None);
        assert!(crumb.screen.index.is_empty());
        assert!(holds_a_window(&crumb.screen));
        assert_eq!(end_offset(crumb, false), Some(0));

        // A list that arrived whole is still held whole, counted or not.
        let whole = |total: Option<u32>| Screen {
            offset: total.map(|_| 0),
            total,
            sections: vec![bluos::screen::Section {
                items: (0..175).map(|i| song(&format!("genre {i}"))).collect(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(!holds_a_window(&whole(None)));
        assert!(!holds_a_window(&whole(Some(175))));
    }

    #[test]
    fn a_page_asked_for_while_a_jump_is_out_does_not_land_on_the_jumped_window() {
        let mut browsing = Browsing::default();
        let mut crumb = songs_crumb(60);
        crumb.screen.next = Some("/ui/browseGrouped?type=Song&listContinuation=60".to_owned());
        browsing.trail.push(crumb);

        // End claims the count, then a scroll asks for the old window's next
        // page before End's request comes back, and takes the same count.
        browsing.jumps += 1;
        let (era, jump) = (browsing.era, browsing.jumps);
        let asked_more = browsing.jumps;

        assert!(land_jump(&mut browsing, last_window(), 2032, era, jump));
        // The check `BrowseMore` makes when its page arrives.
        assert_ne!(
            browsing.jumps, asked_more,
            "rows 60 to 89 would go on after song 2061"
        );
    }

    #[test]
    fn a_page_goes_into_the_list_and_not_the_shelf_after_it() {
        let sizes = |crumb: &Crumb| -> Vec<usize> {
            crumb
                .screen
                .sections
                .iter()
                .map(|s| s.items.len())
                .collect()
        };
        let mut crumb = songs_crumb(30);

        put_page(&mut crumb.screen, vec![song("later")], false);
        assert_eq!(sizes(&crumb), vec![3, 31, 1]);
        assert_eq!(crumb.screen.sections[1].items[30], song("later"));

        // A jump replaces the window, and leaves the options above it and the
        // shelf after it as they were.
        put_page(&mut crumb.screen, vec![song("jumped")], true);
        assert_eq!(sizes(&crumb), vec![3, 1, 1]);
        assert_eq!(crumb.screen.sections[1].items, vec![song("jumped")]);
        assert_eq!(crumb.screen.sections[2].items, vec![song("shelf 0")]);
    }

    #[test]
    fn an_action_the_player_names_gets_a_word() {
        assert_eq!(action_label("love"), "Love");
        assert_eq!(action_label("ban"), "Ban");
        assert_eq!(action_label("shop"), "Shop");
    }

    #[test]
    fn an_action_nobody_here_has_met_is_still_a_button() {
        // The list is whatever the service offers, and a name this does not
        // know is a working action — showing it title-cased beats hiding it.
        assert_eq!(action_label("bookmark"), "Bookmark");
        assert_eq!(action_label("addToLibrary"), "AddToLibrary");
        assert_eq!(action_label(""), "");
    }

    fn act(name: &str, url: Option<&str>, state: Option<i32>) -> bluos::status::Action {
        bluos::status::Action {
            name: name.to_owned(),
            url: url.map(str::to_owned),
            state,
            ..bluos::status::Action::default()
        }
    }

    /// A skip or back carrying an interval: the fifteen-second nudge.
    fn nudge(name: &str, url: &str, seconds: i32) -> bluos::status::Action {
        bluos::status::Action {
            interval: Some(seconds),
            ..act(name, Some(url), None)
        }
    }

    fn status_offering(actions: Vec<bluos::status::Action>) -> bluos::Status {
        bluos::Status {
            actions: Some(bluos::status::Actions { actions }),
            ..bluos::Status::default()
        }
    }

    #[test]
    fn the_transports_own_actions_are_not_drawn_again() {
        // The shape a Powernode on 4.16.22 actually sends for Radio Paradise,
        // which offers no ratings through BluOS at all: a skip with a URL, a
        // back without one, and nothing else. The right answer is no buttons,
        // not a row of dead ones. The document itself is pinned in
        // `bluos::status`.
        let status = status_offering(vec![
            act("back", None, Some(0)),
            act(
                "skip",
                Some("/Action?service=RadioParadise&next=2825604"),
                Some(0),
            ),
        ]);
        assert_eq!(status.actions().len(), 2, "both arrived");
        assert!(track_actions(&status).is_empty(), "and neither is drawn");
    }

    #[test]
    fn a_service_that_offers_ratings_gets_them() {
        let status = status_offering(vec![
            act("love", Some("/Action?love=1"), Some(1)),
            act("ban", Some("/Action?ban=1"), Some(0)),
            act("shop", Some("/Action?shop=1"), None),
            act("skip", Some("/Skip"), None),
        ]);
        let drawn = track_actions(&status);

        assert_eq!(
            drawn.iter().map(|a| a.label.as_str()).collect::<Vec<_>>(),
            vec!["Love", "Ban", "Shop"],
            "skip is the transport's and is left out"
        );
        assert!(drawn[0].on, "already loved, so the button is filled");
        assert!(!drawn[1].on);
        assert!(!drawn[2].on, "no state at all is not set");
    }

    #[test]
    fn an_action_listed_without_a_url_is_not_offered() {
        // The player declaring it unavailable. This is the case `can` gets
        // wrong, and the reason the filter does not use it.
        let status = status_offering(vec![
            act("love", None, Some(0)),
            act("ban", Some("/Action?ban=1"), Some(0)),
        ]);
        let drawn = track_actions(&status);
        assert_eq!(drawn.len(), 1);
        assert_eq!(drawn[0].label, "Ban");
    }

    #[test]
    fn a_nudge_is_the_transports_business() {
        let status = status_offering(vec![
            nudge("skip", "/Action?fwd=15", 15),
            nudge("back", "/Action?rew=15", 15),
            act("love", Some("/Action?love=1"), None),
        ]);
        let drawn = track_actions(&status);
        assert_eq!(drawn.len(), 1, "only the rating belongs here");
        assert_eq!(drawn[0].label, "Love");
    }

    #[test]
    fn availability_is_the_url_and_not_can() {
        use bluos::status::{Action, Actions, Status};

        // `can` falls back to "anything not a stream can do it", which is
        // right for skip and back and wrong here: this status is queue-based,
        // so `can("love")` says yes about a track whose service never offered
        // a rating at all.
        let listed_without_a_url = Status {
            actions: Some(Actions {
                actions: vec![Action {
                    name: "love".to_owned(),
                    url: None,
                    ..Action::default()
                }],
            }),
            ..Status::default()
        };
        assert!(
            listed_without_a_url.can("love"),
            "this is the trap: can() says yes"
        );
        assert!(
            listed_without_a_url
                .action("love")
                .and_then(|a| a.url.as_deref())
                .is_none(),
            "and the URL, which is the real signal, says no"
        );
    }

    #[test]
    fn a_streams_bitrate_is_shown_as_kilobits() {
        // The same field carries a tier for a file and a bitrate for a
        // stream, so the badge that reads CD on a track read 128000 on a
        // station.
        assert_eq!(quality_label("128000"), "128k");
        assert_eq!(quality_label("320000"), "320k");
        assert_eq!(quality_label(" 96000 "), "96k");
        // Lossless PCM, which is a large number and still a bitrate.
        assert_eq!(quality_label("1411000"), "1411k");
    }

    #[test]
    fn kilobits_are_a_thousand_bits_and_not_1024() {
        // The player's own words for the stream behind 128000 are
        // "MP3 128 kb/s". Dividing by 1024 gives 125, which is wrong and
        // almost impossible to notice.
        assert_eq!(quality_label("128000"), "128k");
        assert_ne!(quality_label("128000"), "125k");
    }

    #[test]
    fn something_that_is_not_a_bitrate_is_left_as_it_arrived() {
        // Anything neither this nor the player has seen before is shown as
        // sent rather than rendered as nonsense.
        assert_eq!(quality_label("999"), "999", "too small to be a bitrate");
        assert_eq!(quality_label("dsd"), "DSD");
        assert_eq!(quality_label("128kbps"), "128KBPS", "not a bare number");
        assert_eq!(quality_label("-1"), "-1");
        assert_eq!(quality_label("1.5"), "1.5");
    }
}

/// Replies that come back for a player after another has been chosen.
///
/// Driven through the command loop rather than by calling `open_screen` with
/// the answer already in hand, because the fault lived in the gap between the
/// two: a `Select` handled while a reply was still out.
#[cfg(test)]
mod selection_tests {
    use super::*;
    use fake_player::{Player, fixtures};

    /// A backend with no window behind it.
    ///
    /// Every publish goes through `invoke_from_event_loop`, which answers
    /// `NoEventLoopProvider` where there is no event loop and does nothing
    /// else, so the browsing state is what a test reads. Nothing here reads or
    /// writes the user's own config or cache: the orders and recent searches
    /// start empty, and the artwork cache is kept in memory.
    fn backend(http: &reqwest::Client) -> (Backend, mpsc::UnboundedReceiver<Command>) {
        let (commands, rx) = mpsc::unbounded_channel();
        let backend = Backend {
            registry: Arc::new(Mutex::new(BTreeMap::new())),
            selected: Arc::new(Mutex::new(None)),
            selections: Arc::new(AtomicU64::new(0)),
            commands,
            ui: slint::Weak::default(),
            artwork: Arc::new(Artwork::in_memory(http.clone(), Default::default())),
            browsing: Arc::new(Mutex::new(Browsing::default())),
            known: Arc::new(Mutex::new(Vec::new())),
            writes: Arc::new(lane::Lane::default()),
            searches: Arc::new(AtomicU64::new(0)),
            forms: Arc::new(AtomicU64::new(0)),
            openings: Arc::new(AtomicU64::new(0)),
            sent_transport: Arc::new(AtomicU64::new(0)),
            sent_queue: Arc::new(AtomicU64::new(0)),
            sent_players: Arc::new(AtomicU64::new(0)),
            sent_settings: Arc::new(AtomicU64::new(0)),
            told_about_update: Arc::new(Mutex::new(std::collections::HashSet::new())),
            update_offer: Arc::new(Mutex::new(None)),
            sent_browse: Arc::new(AtomicU64::new(0)),
            sent_index: Arc::new(AtomicU64::new(0)),
            sent_era: Arc::new(AtomicU64::new(0)),
            refreshing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            orders: Arc::new(Mutex::new(order::Orders::new())),
        };
        (backend, rx)
    }

    /// Put a player in the registry the way `track` would, without the
    /// long-poll and the remembered-players file that come with it.
    fn add(backend: &Backend, player: &Player, http: &reqwest::Client) {
        backend.registry.lock().unwrap().insert(
            player.id(),
            Entry {
                client: Client::with_http(player.id(), http.clone()),
                upgrading: false,
                view: Device::default(),
                status: None,
                status_at: None,
                queue: None,
                sync: None,
                cover_url: None,
                card_cover: None,
            },
        );
    }

    /// Two players and the loop that carries commands to them.
    async fn two_players() -> (Backend, Player, Player) {
        // As `run` does, before the first client is built.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::new();
        let first = Player::start().await;
        let second = Player::start().await;
        let (backend, rx) = backend(&http);
        add(&backend, &first, &http);
        add(&backend, &second, &http);
        tokio::spawn(run_commands(rx, backend.clone(), None, http));
        (backend, first, second)
    }

    /// Wait for something the backend does off the loop, or fail saying what.
    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn device(backend: &Backend) -> Option<DeviceId> {
        backend.browsing.lock().unwrap().device
    }

    fn asked(player: &Player, path: &str) -> usize {
        player
            .asked()
            .iter()
            .filter(|seen| seen.split('?').next() == Some(path))
            .count()
    }

    /// How long a slow player sits on its reply, and how long a test waits
    /// past that for the reply to have been dealt with.
    const SLOW: Duration = Duration::from_millis(400);
    const SETTLED: Duration = Duration::from_millis(600);

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_home_does_not_take_the_pane_from_the_player_chosen_after_it() {
        let (backend, slow, quick) = two_players().await;
        // One screen fewer, so whose sidebar is showing can be told apart.
        quick.serve(
            "/ui/Configuration",
            fixtures::configuration()
                .replace(r#"<item id="favourites" URI="/ui/Favourites"/>"#, ""),
        );
        slow.delay("/ui/Configuration", SLOW);

        let _ = backend.commands.send(Command::Select(slow.id()));
        until("the slow player to be asked", || {
            slow.asked_for("/ui/Configuration")
        })
        .await;
        let _ = backend.commands.send(Command::Select(quick.id()));
        until("the quick player's Home", || {
            device(&backend) == Some(quick.id())
        })
        .await;

        // The slow player's answer arrives now, after the choice has moved on.
        tokio::time::sleep(SETTLED).await;

        let browsing = backend.browsing.lock().unwrap();
        assert_eq!(
            browsing.device,
            Some(quick.id()),
            "the pane stays with the choice"
        );
        assert_eq!(browsing.trail.len(), 1);
        assert_eq!(
            browsing
                .screens
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>(),
            ["Home", "Search"],
            "and so does the sidebar"
        );
    }

    /// A deeper screen asked for from one player and answered after another
    /// was chosen: the same hole as Home, reached by pressing a row.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_screen_opened_before_choosing_another_player_is_not_shown() {
        let (backend, first, second) = two_players().await;

        let _ = backend.commands.send(Command::Select(first.id()));
        until("the first player's Home", || {
            device(&backend) == Some(first.id())
        })
        .await;

        first.delay("/ui/Home", SLOW);
        let opening = tokio::spawn(open_screen(
            backend.clone(),
            first.id(),
            "/ui/Home".to_owned(),
            Arrive::Deeper,
        ));
        until("the deeper screen to be asked for", || {
            asked(&first, "/ui/Home") == 2
        })
        .await;

        let _ = backend.commands.send(Command::Select(second.id()));
        until("the second player's Home", || {
            device(&backend) == Some(second.id())
        })
        .await;
        opening.await.expect("the open finishes");

        let browsing = backend.browsing.lock().unwrap();
        assert_eq!(browsing.device, Some(second.id()));
        assert_eq!(
            browsing.trail.len(),
            1,
            "nothing pushed onto the second player's trail"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_player_that_cannot_say_what_it_offers_leaves_the_last_one_showing() {
        let (backend, first, second) = two_players().await;
        second.forget("/ui/Configuration");

        let _ = backend.commands.send(Command::Select(first.id()));
        until("the first player's Home", || {
            device(&backend) == Some(first.id())
        })
        .await;

        let _ = backend.commands.send(Command::Select(second.id()));
        until("the second player to be asked", || {
            second.asked_for("/ui/Configuration")
        })
        .await;
        tokio::time::sleep(SETTLED).await;

        {
            let browsing = backend.browsing.lock().unwrap();
            assert_eq!(browsing.screens.len(), 3, "the sidebar is not emptied");
            assert_eq!(browsing.queue_uri.as_deref(), Some("/ui/playQueue"));
            assert_eq!(browsing.search_uri.as_deref(), Some("/ui/Search"));
            assert_eq!(browsing.device, Some(first.id()));
        }

        // And it is asked again the next time it is chosen.
        second.serve("/ui/Configuration", fixtures::configuration());
        let _ = backend.commands.send(Command::Select(first.id()));
        let _ = backend.commands.send(Command::Select(second.id()));
        until("the second player's Home", || {
            device(&backend) == Some(second.id())
        })
        .await;
    }

    /// Choose a player and wait for its Home to be the screen showing.
    async fn choose(backend: &Backend, player: &Player) {
        let _ = backend.commands.send(Command::Select(player.id()));
        until("the chosen player's Home", || {
            device(backend) == Some(player.id())
        })
        .await;
    }

    /// Choose a player whose Home never comes, and wait until it has tried.
    ///
    /// A Home arriving takes the pane back to browsing, so this is how a pane
    /// read from one player stays up with another selected: the state every
    /// test below is about. A player that cannot say what screens it has
    /// leaves the window exactly like this for as long as it is chosen, and
    /// any player leaves it like this until its Home lands.
    async fn choose_without_home(backend: &Backend, player: &Player) {
        player.forget("/ui/Configuration");
        let _ = backend.commands.send(Command::Select(player.id()));
        until("the player to be selected and asked", || {
            backend.is_selected(player.id()) && player.asked_for("/ui/Configuration")
        })
        .await;
        tokio::time::sleep(SETTLED).await;
    }

    fn settings_owner(backend: &Backend) -> Option<DeviceId> {
        let browsing = backend.browsing.lock().unwrap();
        browsing.pane.settings_owned().map(|(owner, _)| owner)
    }

    fn settings_depth(backend: &Backend) -> usize {
        match &backend.browsing.lock().unwrap().pane {
            Pane::Settings(_, trail) => trail.len(),
            _ => 0,
        }
    }

    /// A settings page with one switch on it.
    const ONE_SWITCH: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<settings schemaVersion="35">
  <setting id="eq-switch" name="eq-switch" displayName="Tone Controls" url="/alsa_setting" class="boolean" value="OFF"/>
</settings>"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_setting_is_written_to_the_player_whose_page_is_showing() {
        let (backend, first, second) = two_players().await;
        first.serve("/Settings", ONE_SWITCH);
        second.serve("/Settings", ONE_SWITCH);

        choose(&backend, &first).await;
        let _ = backend
            .commands
            .send(Command::OpenSettings(None, Step::Root));
        until("the first player's settings", || {
            settings_owner(&backend) == Some(first.id())
        })
        .await;

        choose_without_home(&backend, &second).await;
        // A press on the row, and a change of its value: the two ways a
        // settings row writes.
        let _ = backend.commands.send(Command::SettingAction(0));
        let _ = backend.commands.send(Command::SettingEdit(0, Edit::Toggle));
        until("both writes", || asked(&first, "/alsa_setting") == 2).await;
        tokio::time::sleep(SETTLED).await;

        assert_eq!(
            asked(&second, "/alsa_setting"),
            0,
            "nothing is written to the player selected now"
        );
        assert_eq!(settings_owner(&backend), Some(first.id()));
    }

    /// A switch, and two settings that need it on while it is off: a button,
    /// and another switch. Each writes somewhere of its own, so a write that
    /// should not have happened is told apart from the one that should.
    const DIMMED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<settings schemaVersion="35">
  <setting id="eq-switch" name="eq-switch" displayName="Tone Controls" url="/alsa_setting" class="boolean" value="OFF"/>
  <setting id="reset" name="reset" displayName="Reset All" url="/reset_setting" class="button">
    <dependsOn name="eq-switch" value="ON"/>
  </setting>
  <setting id="loudness" name="loudness" displayName="Loudness" url="/loudness_setting" class="boolean" value="OFF">
    <dependsOn name="eq-switch" value="ON"/>
  </setting>
</settings>"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_setting_the_player_has_dimmed_is_not_written() {
        let (backend, player, _) = two_players().await;
        player.serve("/Settings", DIMMED);

        choose(&backend, &player).await;
        let _ = backend
            .commands
            .send(Command::OpenSettings(None, Step::Root));
        until("the settings", || {
            settings_owner(&backend) == Some(player.id())
        })
        .await;

        // Every way a row writes, on the two rows the player has dimmed: the
        // press that reaches a button row when its pill refuses it, and the
        // press and the edit on a switch.
        let _ = backend.commands.send(Command::SettingAction(1));
        let _ = backend.commands.send(Command::SettingAction(2));
        let _ = backend.commands.send(Command::SettingEdit(2, Edit::Toggle));
        // Then one that is allowed, so there is something to wait for: the
        // writes run in the order they were sent.
        let _ = backend.commands.send(Command::SettingAction(0));
        until("the write that is allowed", || {
            asked(&player, "/alsa_setting") == 1
        })
        .await;
        tokio::time::sleep(SETTLED).await;

        assert_eq!(
            asked(&player, "/reset_setting"),
            0,
            "the button is not pressed"
        );
        assert_eq!(
            asked(&player, "/loudness_setting"),
            0,
            "the switch is not written"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn settings_asked_for_before_choosing_another_player_are_not_shown() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;

        first.delay("/Settings", SLOW);
        let _ = backend
            .commands
            .send(Command::OpenSettings(None, Step::Root));
        until("the settings to be asked for", || {
            first.asked_for("/Settings")
        })
        .await;
        choose(&backend, &second).await;
        tokio::time::sleep(SETTLED).await;

        assert_eq!(
            settings_owner(&backend),
            None,
            "the player left behind does not put its settings over the one chosen"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_settings_page_joins_only_the_trail_it_was_asked_from() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;
        let _ = backend
            .commands
            .send(Command::OpenSettings(None, Step::Root));
        until("the first player's settings", || {
            settings_owner(&backend) == Some(first.id())
        })
        .await;
        choose_without_home(&backend, &second).await;

        // A re-read of the first player's page still belongs on its trail,
        // whoever is selected: the pane is still that player's.
        let _ = backend
            .commands
            .send(Command::OpenSettings(None, Step::Reload(first.id())));
        until("the page to be read again", || {
            asked(&first, "/Settings") == 2
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        assert_eq!(settings_owner(&backend), Some(first.id()));
        assert_eq!(settings_depth(&backend), 1);

        // Once the pane is the second player's, a page of the first one's has
        // nothing to join.
        let _ = backend
            .commands
            .send(Command::OpenSettings(None, Step::Root));
        until("the second player's settings", || {
            settings_owner(&backend) == Some(second.id())
        })
        .await;
        let _ = backend.commands.send(Command::OpenSettings(
            Some("audio".to_owned()),
            Step::Deeper(first.id()),
        ));
        until("the deeper page to be asked for", || {
            first.asked_for("/Settings?id=audio")
        })
        .await;
        tokio::time::sleep(SETTLED).await;

        assert_eq!(settings_owner(&backend), Some(second.id()));
        assert_eq!(
            settings_depth(&backend),
            1,
            "nothing pushed onto the second player's trail"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_preset_is_renamed_on_the_player_it_belongs_to() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;

        let edit = bluos::Action {
            kind: ActionKind::DeepLink,
            uri: Some(
                "/edit-preset/3?id=3&image=&name=Groove+Salad\
                 &url=http%3A%2F%2Fice1.somafm.com%2Fgroovesalad-128-mp3"
                    .to_owned(),
            ),
            ..Default::default()
        };
        run_action(backend.clone(), first.id(), edit, Arrive::Deeper).await;
        assert!(matches!(
            &backend.browsing.lock().unwrap().pane,
            Pane::EditPreset(page) if page.device == first.id()
        ));

        choose_without_home(&backend, &second).await;
        let _ = backend
            .commands
            .send(Command::SettingEdit(0, Edit::Text("Salad".to_owned())));
        until("the rename", || first.asked_for("/SetPreset")).await;
        tokio::time::sleep(SETTLED).await;

        assert!(
            !second.asked_for("/SetPreset"),
            "no slot is overwritten on the player selected now"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn presets_are_reordered_on_the_player_they_were_read_from() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;
        choose_without_home(&backend, &second).await;

        backend.browsing.lock().unwrap().pane = Pane::Customise(CustomisePage {
            screen: "presets".to_owned(),
            title: "Reorder Presets".to_owned(),
            rows: vec![
                ("2".to_owned(), "Second".to_owned()),
                ("1".to_owned(), "First".to_owned()),
            ],
            presets: Some((first.id(), 34)),
        });
        let _ = backend.commands.send(Command::CustomiseSave);
        until("the reorder", || first.asked_for("/Presets/edit")).await;
        tokio::time::sleep(SETTLED).await;

        assert!(
            !second.asked_for("/Presets/edit"),
            "the player selected now keeps its order"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_question_is_answered_on_the_player_that_asked_it() {
        let (backend, first, second) = two_players().await;
        first.serve("/Ask", fixtures::replace_queue_dialog());
        choose(&backend, &first).await;

        let asking = bluos::Action {
            kind: ActionKind::PlayerLink,
            uri: Some("/Ask".to_owned()),
            ..Default::default()
        };
        run_action(backend.clone(), first.id(), asking, Arrive::Deeper).await;
        assert_eq!(
            backend
                .browsing
                .lock()
                .unwrap()
                .dialog
                .as_ref()
                .map(|(owner, _)| *owner),
            Some(first.id())
        );

        choose_without_home(&backend, &second).await;
        // "Replace", which carries an add of the first player's own file.
        let _ = backend.commands.send(Command::DialogPress(0));
        until("the answer", || first.asked_for("/Add?playnow=1")).await;
        tokio::time::sleep(SETTLED).await;

        assert!(
            !second.asked_for("/Add"),
            "the player selected now plays nothing"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_confirmed_input_switches_the_player_it_was_asked_about() {
        let (backend, first, second) = two_players().await;
        first.serve("/Status", fixtures::status_playing());
        choose(&backend, &first).await;

        // Only asked about while something plays.
        let client = backend
            .with_entry(first.id(), |e| e.client.clone())
            .expect("the first player");
        let status = client.status().await.expect("a status");
        if let Some(entry) = backend.registry.lock().unwrap().get_mut(&first.id()) {
            entry.status = Some(status);
        }
        let input = bluos::Action {
            kind: ActionKind::PlayerLink,
            uri: Some("/Play?url=Capture%3Abluez%3Abt".to_owned()),
            ..Default::default()
        };
        assert!(ask_before_input(&backend, first.id(), &input, "Bluetooth"));

        choose_without_home(&backend, &second).await;
        let _ = backend.commands.send(Command::ConfirmInput(true));
        until("the switch", || first.asked_for("/Play?url=Capture")).await;
        tokio::time::sleep(SETTLED).await;

        assert!(
            !second.asked_for("/Play?url=Capture"),
            "the player selected now keeps playing"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_track_is_filed_on_the_player_that_offered_the_playlists() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;
        choose_without_home(&backend, &second).await;

        backend.browsing.lock().unwrap().pane = playlists_page(first.id(), 1);
        let _ = backend.commands.send(Command::PlaylistAdd(
            1,
            None,
            bluos::client::PlaylistTarget::New("Mix".to_owned()),
        ));
        until("the add", || first.asked_for("/AddToPlaylist")).await;
        tokio::time::sleep(SETTLED).await;

        assert!(
            !second.asked_for("/AddToPlaylist"),
            "nothing is filed on the player selected now"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_question_that_lands_after_choosing_another_player_is_not_put_up() {
        let (backend, first, second) = two_players().await;
        first.serve("/Ask", fixtures::replace_queue_dialog());
        first.delay("/Ask", SLOW);
        choose(&backend, &first).await;

        let asking = bluos::Action {
            kind: ActionKind::PlayerLink,
            uri: Some("/Ask".to_owned()),
            ..Default::default()
        };
        tokio::spawn(run_action(
            backend.clone(),
            first.id(),
            asking,
            Arrive::Deeper,
        ));
        until("the question to be asked for", || first.asked_for("/Ask")).await;
        choose_without_home(&backend, &second).await;
        tokio::time::sleep(SETTLED).await;

        assert!(
            backend.browsing.lock().unwrap().dialog.is_none(),
            "the first player's Replace is not offered over the second player's queue"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn playlists_offered_after_choosing_another_player_are_not_shown() {
        let (backend, first, second) = two_players().await;
        first.serve(
            "/AddToPlaylistOptions",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<addToPlaylistOptions service="LocalMusic">
  <urlPath>/AddToPlaylist</urlPath>
  <requestParameter>songid=%2Fmusic%2Fa.flac</requestParameter>
  <playlists service="LocalMusic" serviceName="BluOS" create="1"></playlists>
</addToPlaylistOptions>"#,
        );
        first.delay("/AddToPlaylistOptions", SLOW);
        choose(&backend, &first).await;

        let filing = bluos::Action {
            kind: ActionKind::Browse,
            uri: Some("/AddToPlaylistOptions?service=LocalMusic&songid=a".to_owned()),
            result_type: Some("AddToPlaylistOptions".to_owned()),
            ..Default::default()
        };
        tokio::spawn(run_action(
            backend.clone(),
            first.id(),
            filing,
            Arrive::Deeper,
        ));
        until("the options to be asked for", || {
            first.asked_for("/AddToPlaylistOptions")
        })
        .await;
        choose_without_home(&backend, &second).await;
        tokio::time::sleep(SETTLED).await;

        assert!(
            !matches!(backend.browsing.lock().unwrap().pane, Pane::Playlists(_)),
            "the first player's playlists are not drawn under the second player"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_preset_is_not_opened_for_renaming_over_another_player() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;
        choose_without_home(&backend, &second).await;

        // A press on the first player's screen, which is still the one
        // showing while the second player's Home has not come.
        let edit = bluos::Action {
            kind: ActionKind::DeepLink,
            uri: Some("/edit-preset/3?id=3&image=&name=Groove+Salad&url=http%3A%2F%2Fa".to_owned()),
            ..Default::default()
        };
        run_action(backend.clone(), first.id(), edit, Arrive::Deeper).await;

        assert!(
            !matches!(backend.browsing.lock().unwrap().pane, Pane::EditPreset(_)),
            "the first player's preset is not offered under the second player's name"
        );
    }

    /// Open a player's alarms, and its one alarm in the editor.
    async fn open_alarm(backend: &Backend, player: &Player) {
        let _ = backend.commands.send(Command::OpenAlarms(player.id()));
        until("the alarms", || alarms_owner(backend) == Some(player.id())).await;
        let _ = backend.commands.send(Command::AlarmOpen(1));
        until("the editor", || {
            matches!(
                &backend.browsing.lock().unwrap().pane,
                Pane::Alarms(page) if page.editing.is_some()
            )
        })
        .await;
    }

    fn picker_depth(backend: &Backend) -> usize {
        match &backend.browsing.lock().unwrap().pane {
            Pane::Alarms(page) => page.picking.len(),
            _ => 0,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_alarm_source_is_chosen_from_the_player_whose_alarm_it_is() {
        let (backend, first, second) = two_players().await;
        first.serve("/Alarms", fixtures::one_alarm());
        choose(&backend, &first).await;
        open_alarm(&backend, &first).await;
        choose_without_home(&backend, &second).await;

        let _ = backend.commands.send(Command::AlarmPick);
        until("the picker", || picker_depth(&backend) == 1).await;
        tokio::time::sleep(SETTLED).await;

        assert!(first.asked_for("/RadioBrowse"));
        assert!(
            !second.asked_for("/RadioBrowse"),
            "the player selected now is not where the alarm's source comes from"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_folder_answered_after_back_does_not_reopen_the_picker() {
        let (backend, first, _second) = two_players().await;
        first.serve("/Alarms", fixtures::one_alarm());
        choose(&backend, &first).await;
        open_alarm(&backend, &first).await;

        let _ = backend.commands.send(Command::AlarmPick);
        until("the picker", || picker_depth(&backend) == 1).await;

        // "Inputs", the row on the first level that leads somewhere.
        first.delay("/RadioBrowse", SLOW);
        let _ = backend.commands.send(Command::AlarmPickRow(1));
        until("the folder to be asked for", || {
            asked(&first, "/RadioBrowse") == 2
        })
        .await;
        let _ = backend.commands.send(Command::BrowseBack);
        until("the picker to close", || picker_depth(&backend) == 0).await;
        tokio::time::sleep(SETTLED).await;

        assert_eq!(
            picker_depth(&backend),
            0,
            "a late folder does not put the picker back up"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_saved_alarm_does_not_land_on_another_players_page() {
        alarms_answered_late_stay_with_their_player(|_| Command::AlarmSave).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_deleted_alarm_does_not_land_on_another_players_page() {
        alarms_answered_late_stay_with_their_player(|_| Command::AlarmDelete).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_armed_alarm_does_not_land_on_another_players_page() {
        // The id the first player's list carries. The command names its
        // player itself, as the list's toggle does.
        alarms_answered_late_stay_with_their_player(|owner| Command::AlarmArm(owner, 1, false))
            .await;
    }

    /// Send a write on the first player's alarm, choose the second player and
    /// open its alarms while the answer is still on its way, and check the
    /// first player's list is not put on the second player's page.
    ///
    /// Every alarm write answers with the whole list, so each of them is a
    /// way for one player's alarms to be drawn, and then toggled, as another's.
    async fn alarms_answered_late_stay_with_their_player(write: impl FnOnce(DeviceId) -> Command) {
        let (backend, first, second) = two_players().await;
        first.serve("/Alarms", fixtures::one_alarm());
        choose(&backend, &first).await;
        open_alarm(&backend, &first).await;

        // Long enough to choose the other player and open its alarms while
        // the write is still on its way.
        let late = SLOW * 4;
        first.delay("/Alarms", late);
        let saving = Instant::now();
        let _ = backend.commands.send(write(first.id()));
        until("the write", || asked(&first, "/Alarms") == 2).await;

        choose_without_home(&backend, &second).await;
        let _ = backend.commands.send(Command::OpenAlarms(second.id()));
        until("the second player's alarms", || {
            alarms_owner(&backend) == Some(second.id())
        })
        .await;
        tokio::time::sleep((saving + late + SETTLED).saturating_duration_since(Instant::now()))
            .await;

        let browsing = backend.browsing.lock().unwrap();
        let Pane::Alarms(page) = &browsing.pane else {
            panic!("the alarms pane went away");
        };
        assert_eq!(page.device, second.id());
        assert!(
            page.list.alarms.is_empty(),
            "the first player's alarm is not listed as the second's"
        );
    }

    /// Which opening of the alarm editor is on screen, when one is.
    fn editor_opened(backend: &Backend) -> Option<u64> {
        match &backend.browsing.lock().unwrap().pane {
            Pane::Alarms(page) if page.editing.is_some() => Some(page.opened),
            _ => None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_saved_alarm_does_not_close_the_editor_opened_since() {
        alarm_writes_close_only_the_editor_they_were_pressed_on(Command::AlarmSave).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_deleted_alarm_does_not_close_the_editor_opened_since() {
        alarm_writes_close_only_the_editor_they_were_pressed_on(Command::AlarmDelete).await;
    }

    /// Press a write on one opening of the editor, go Back and open the alarm
    /// again while the answer is still out, and check the editor the user is
    /// in now is left alone.
    ///
    /// Both writes wait in the writes lane before their round trip, and both
    /// used to close the editor on the player alone. On a slow player that
    /// threw away the time, days and volume typed into the editor opened
    /// since — under a toast saying the alarm was saved — and left the source
    /// picker drawn over no editor at all, so a source chosen on it went
    /// nowhere.
    async fn alarm_writes_close_only_the_editor_they_were_pressed_on(write: Command) {
        let (backend, first, _second) = two_players().await;
        first.serve("/Alarms", fixtures::one_alarm());
        choose(&backend, &first).await;
        open_alarm(&backend, &first).await;
        let pressed = editor_opened(&backend).expect("the editor the write is pressed on");

        // Long enough to leave the editor and open it again while the write
        // is still on its way.
        let late = SLOW * 4;
        first.delay("/Alarms", late);
        let writing = Instant::now();
        let _ = backend.commands.send(write);
        until("the write", || asked(&first, "/Alarms") == 2).await;

        let _ = backend.commands.send(Command::BrowseBack);
        until("the editor to close", || editor_opened(&backend).is_none()).await;
        let _ = backend.commands.send(Command::AlarmOpen(1));
        until("the alarm to be opened again", || {
            editor_opened(&backend).is_some_and(|opened| opened != pressed)
        })
        .await;
        // With the source picker over it, which belongs to this editor and
        // not to the one the write was pressed on.
        let _ = backend.commands.send(Command::AlarmPick);
        until("the picker", || picker_depth(&backend) == 1).await;
        let since = editor_opened(&backend).expect("the editor opened since");
        tokio::time::sleep((writing + late + SETTLED).saturating_duration_since(Instant::now()))
            .await;

        assert_eq!(
            editor_opened(&backend),
            Some(since),
            "the reply closes the editor it was pressed on, not the one opened since"
        );
        assert_eq!(
            picker_depth(&backend),
            1,
            "and leaves that editor's picker where the user left it"
        );
    }

    /// A queue document with one button on it: Clear, as older firmware
    /// writes it, without the ask.
    const QUEUE_WITH_CLEAR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<queue offset="0" total="1" id="7">
  <button text="Clear" icon="/images/ui/btn_clear_queue.png">
    <action type="player-link" URI="/Clear"/>
  </button>
</queue>"#;

    fn queue_buttons_owner(backend: &Backend) -> Option<DeviceId> {
        let browsing = backend.browsing.lock().unwrap();
        browsing.queue_screen.as_ref().map(|(owner, _)| *owner)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_queue_button_runs_only_on_the_queue_it_was_read_from() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;
        // Whatever reads choosing started have finished, so the document
        // below is only served to the read this test makes.
        tokio::time::sleep(SETTLED).await;

        first.serve("/ui/playQueue", QUEUE_WITH_CLEAR);
        first.delay("/ui/playQueue", SLOW);
        let before = asked(&first, "/ui/playQueue");
        let reading = tokio::spawn(fetch_queue_buttons(backend.clone(), first.id()));
        until("the buttons to be asked for", || {
            asked(&first, "/ui/playQueue") > before
        })
        .await;
        choose_without_home(&backend, &second).await;
        reading.await.expect("the read finishes");

        assert_eq!(
            queue_buttons_owner(&backend),
            None,
            "the first player's buttons are not kept under the second's queue"
        );
        let _ = backend.commands.send(Command::QueueButton(0));
        tokio::time::sleep(SETTLED).await;
        assert!(
            !second.asked_for("/Clear"),
            "the player selected now keeps its queue"
        );
        assert!(!first.asked_for("/Clear"));

        // Back on the first player its buttons are read and work as before.
        first.delay("/ui/playQueue", Duration::ZERO);
        let _ = backend.commands.send(Command::Select(first.id()));
        until("the first player's buttons", || {
            queue_buttons_owner(&backend) == Some(first.id())
        })
        .await;
        let _ = backend.commands.send(Command::QueueButton(0));
        until("the clear", || first.asked_for("/Clear")).await;
        assert!(!second.asked_for("/Clear"));

        // The first player's buttons are still held when the second player
        // is chosen, until that one's own document arrives. A press on the
        // row drawn before the move is refused rather than run on either.
        choose_without_home(&backend, &second).await;
        assert_eq!(queue_buttons_owner(&backend), Some(first.id()));
        let _ = backend.commands.send(Command::QueueButton(0));
        tokio::time::sleep(SETTLED).await;
        assert_eq!(
            asked(&first, "/Clear"),
            1,
            "the first player is not cleared again"
        );
        assert!(!second.asked_for("/Clear"));
    }

    fn configured(backend: &Backend) -> Option<DeviceId> {
        backend.browsing.lock().unwrap().configured
    }

    /// The sidebar entry drawn lit.
    fn lit(backend: &Backend) -> Option<(i32, i32)> {
        backend.browsing.lock().unwrap().lit()
    }

    /// Choose a player, and press Favourites in its sidebar while its Home is
    /// still on the way; each reply held back by the time given.
    ///
    /// Whichever lands first, the window ends on Favourites with Favourites
    /// lit — the press is the last thing asked for.
    async fn press_favourites_before_home(home: Duration, favourites: Duration) {
        let (backend, _, player) = two_players().await;
        player.serve("/ui/Favourites", fixtures::home());
        player.delay("/ui/Home", home);
        player.delay("/ui/Favourites", favourites);

        let _ = backend.commands.send(Command::Select(player.id()));
        until("its Home to be asked for", || {
            asked(&player, "/ui/Home") > 0
        })
        .await;
        let _ = backend.commands.send(Command::Sidebar(0, 1));
        until("Favourites to be asked for", || {
            asked(&player, "/ui/Favourites") > 0
        })
        .await;
        assert_eq!(lit(&backend), Some((0, 1)), "the press lights at once");
        tokio::time::sleep(home.max(favourites) + SETTLED).await;

        let browsing = backend.browsing.lock().unwrap();
        assert_eq!(browsing.device, Some(player.id()));
        assert_eq!(
            browsing
                .trail
                .iter()
                .map(|c| c.uri.as_str())
                .collect::<Vec<_>>(),
            ["/ui/Favourites"],
            "the screen pressed for is the one showing"
        );
        assert_eq!(
            browsing.lit(),
            Some((0, 1)),
            "and it is the entry lit, not Home"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_home_that_lands_before_a_press_does_not_unlight_it() {
        press_favourites_before_home(SLOW, SLOW * 2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_home_that_lands_after_a_press_is_not_shown() {
        press_favourites_before_home(SLOW, Duration::ZERO).await;
    }

    /// Chosen, the second player's configuration lands and its Home does not
    /// yet; the first player is chosen back in between. The first player's
    /// screens never left, so only what sits beside them has to be put right —
    /// and without asking again, nothing ever did.
    #[tokio::test(flavor = "multi_thread")]
    async fn choosing_back_before_the_other_players_home_puts_its_own_sidebar_back() {
        let (backend, first, second) = two_players().await;
        // One screen fewer, so whose sidebar is showing can be told apart.
        second.serve(
            "/ui/Configuration",
            fixtures::configuration()
                .replace(r#"<item id="favourites" URI="/ui/Favourites"/>"#, ""),
        );
        first.serve("/ui/Favourites", fixtures::home());
        choose(&backend, &first).await;
        tokio::time::sleep(SETTLED).await;
        assert_eq!(lit(&backend), Some((0, 0)), "a player's Home lights Home");
        // Somewhere other than Home, so what is lit says whose trail it is.
        let _ = backend.commands.send(Command::Sidebar(0, 1));
        until("the first player's Favourites", || {
            asked(&first, "/ui/Favourites") > 0
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        let homes = asked(&first, "/ui/Home");
        let buttons = asked(&first, "/ui/playQueue");

        second.delay("/ui/Home", SLOW);
        let _ = backend.commands.send(Command::Select(second.id()));
        until("the second player's configuration", || {
            configured(&backend) == Some(second.id())
        })
        .await;
        // Its sidebar is up and its Home is not. The first player's Favourites
        // is second in its list and Search is second in this one, so an index
        // carried across would light Search. The same holds for good when
        // that Home never comes.
        {
            let browsing = backend.browsing.lock().unwrap();
            assert_eq!(browsing.screens.len(), 2);
            assert_eq!(browsing.device, Some(first.id()));
            assert_eq!(
                browsing.lit(),
                None,
                "the first player's Favourites does not light the second's Search"
            );
        }
        let _ = backend.commands.send(Command::Select(first.id()));
        until("the first player's configuration again", || {
            configured(&backend) == Some(first.id())
        })
        .await;
        // The second player's Home arrives now, and is dropped.
        tokio::time::sleep(SETTLED).await;

        {
            let browsing = backend.browsing.lock().unwrap();
            assert_eq!(browsing.device, Some(first.id()));
            assert_eq!(browsing.configured, Some(first.id()));
            assert_eq!(
                browsing.screens.len(),
                3,
                "the sidebar is the first player's"
            );
            assert_eq!(browsing.trail.len(), 1);
            assert_eq!(browsing.trail[0].uri, "/ui/Favourites");
            assert_eq!(
                browsing.lit(),
                Some((0, 1)),
                "and it lights the screen showing, not Home"
            );
        }
        assert_eq!(
            asked(&first, "/ui/Home"),
            homes,
            "the trail is left where it was rather than started again"
        );
        assert!(
            asked(&first, "/ui/playQueue") > buttons,
            "and the queue's buttons are read from the first player's address"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_row_of_the_last_players_screen_does_not_play_on_it() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;
        choose_without_home(&backend, &second).await;

        // "A Song", the first row of the first player's Home, still showing.
        let _ = backend.commands.send(Command::BrowseActivate(0));
        tokio::time::sleep(SETTLED).await;

        assert!(
            !first.asked_for("/ui/prf"),
            "the player not selected does not start playing"
        );
        assert!(!second.asked_for("/ui/prf"));
    }

    /// A Sources screen with one input on it, for the sidebar's lower half.
    const ONE_INPUT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<screen screenTitle="Sources" id="screen-sources">
  <row id="inputs" title="Inputs">
    <source text="Line In" image="/images/in.png">
      <action type="player-link" URI="/Play?url=Capture%3Ahw%3Ain"/>
    </source>
  </row>
</screen>"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn an_input_in_the_last_players_sidebar_is_not_switched_to_on_this_one() {
        let (backend, first, second) = two_players().await;
        first.serve("/ui/Sources", ONE_INPUT);
        second.serve("/ui/Sources", ONE_INPUT);
        choose(&backend, &first).await;
        choose_without_home(&backend, &second).await;

        let _ = backend.commands.send(Command::Sidebar(1, 0));
        until("the configuration to be asked for again", || {
            asked(&second, "/ui/Configuration") == 2
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        assert!(
            !second.asked_for("/Play?"),
            "the first player's input is not sent to the second"
        );
        assert!(!first.asked_for("/Play?"));

        // Once the player answers, its own sidebar works.
        second.serve("/ui/Configuration", fixtures::configuration());
        let _ = backend.commands.send(Command::Sidebar(1, 0));
        until("the second player's configuration", || {
            configured(&backend) == Some(second.id())
        })
        .await;
        let _ = backend.commands.send(Command::Sidebar(1, 0));
        until("the input", || second.asked_for("/Play?")).await;
        assert!(!first.asked_for("/Play?"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn alarms_answered_after_another_player_is_chosen_are_not_shown() {
        let (backend, first, second) = two_players().await;
        first.serve("/Alarms", fixtures::one_alarm());
        choose(&backend, &first).await;

        first.delay("/Alarms", SLOW);
        let _ = backend.commands.send(Command::OpenAlarms(first.id()));
        until("the alarms to be asked for", || first.asked_for("/Alarms")).await;
        choose(&backend, &second).await;
        tokio::time::sleep(SETTLED).await;

        assert_eq!(
            alarms_owner(&backend),
            None,
            "the first player's alarms do not replace the second player's Home"
        );
    }

    /// The web pages are on the player's port 80, which the fake does not
    /// serve, so the rule a form lands by is exercised on its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_form_goes_up_only_over_its_own_players_page() {
        let (backend, first, second) = two_players().await;
        let put = |player: &Player, replacing| {
            show_form(
                &backend,
                player.id(),
                replacing,
                "Form".to_owned(),
                bluos::forms::Form::default(),
                String::new(),
            )
        };
        let showing = || match &backend.browsing.lock().unwrap().pane {
            Pane::Form(page) => Some((page.device, page.from.is_some())),
            _ => None,
        };

        let shares = || {
            backend.browsing.lock().unwrap().pane = Pane::Web(
                first.id(),
                WebPage::Shares {
                    action: None,
                    shares: Vec::new(),
                },
            );
        };

        shares();
        assert_eq!(
            put(&second, Replacing::Page),
            None,
            "not over another player's page"
        );
        let wifi = put(&first, Replacing::Page).expect("over its own page");
        assert_eq!(showing(), Some((first.id(), true)));

        assert_eq!(
            put(&second, Replacing::Form(wifi)),
            None,
            "not over another player's form"
        );
        assert_eq!(put(&first, Replacing::Form(wifi)), Some(wifi));
        assert_eq!(
            showing(),
            Some((first.id(), true)),
            "the answer keeps the page Back returns to"
        );

        // Back, and another of the same player's forms opened in its place.
        shares();
        assert_eq!(
            put(&first, Replacing::Form(wifi)),
            None,
            "an answer does not reopen a form that was left"
        );
        let service = put(&first, Replacing::Page).expect("over its own page");
        assert_ne!(service, wifi);
        assert_eq!(
            put(&first, Replacing::Form(wifi)),
            None,
            "an answer for one form does not land on another of the same player's"
        );
        assert_eq!(put(&first, Replacing::Form(service)), Some(service));

        backend.browsing.lock().unwrap().pane = Pane::Browse;
        assert_eq!(put(&first, Replacing::Form(service)), None);
        assert_eq!(put(&first, Replacing::Page), None);
    }

    /// Back out of the placeholder of a form opened on a settings page, while
    /// the player is still answering: the page it was opened on, not Home,
    /// and the answer arriving afterwards does not put the form back.
    #[tokio::test(flavor = "multi_thread")]
    async fn back_out_of_a_form_returns_to_the_settings_page_it_was_opened_on() {
        let (backend, first, _) = two_players().await;
        backend.browsing.lock().unwrap().pane =
            Pane::Settings(first.id(), vec![SettingsPage::default()]);
        let wifi = show_form(
            &backend,
            first.id(),
            Replacing::Page,
            "WiFi".to_owned(),
            bluos::forms::Form::default(),
            "Reading the page…".to_owned(),
        )
        .expect("over its own settings page");

        let _ = backend.commands.send(Command::BrowseBack);
        until("the settings page to be back", || {
            settings_owner(&backend) == Some(first.id())
        })
        .await;

        assert_eq!(
            show_form(
                &backend,
                first.id(),
                Replacing::Form(wifi),
                "WiFi".to_owned(),
                bluos::forms::Form::default(),
                String::new(),
            ),
            None,
            "the scan landing late does not reopen the form"
        );
        assert_eq!(settings_owner(&backend), Some(first.id()));
    }

    /// The search key, with a player chosen that has not said what it offers,
    /// while the last player's Home is still showing.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_search_key_lights_nothing_for_a_player_that_is_not_configured() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;
        until("the first player's Home to be lit", || {
            lit(&backend) == Some((0, 0))
        })
        .await;
        choose_without_home(&backend, &second).await;

        let _ = backend.commands.send(Command::OpenSearch);
        until("the configuration to be asked for again", || {
            asked(&second, "/ui/Configuration") == 2
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        assert_eq!(
            lit(&backend),
            Some((0, 0)),
            "the last player's Search is not lit over its Home"
        );
        assert!(!first.asked_for("/ui/Search"));
        assert!(!second.asked_for("/ui/Search"));

        // Nor once that player is chosen back.
        let _ = backend.commands.send(Command::Select(first.id()));
        until("the first player's configuration", || {
            configured(&backend) == Some(first.id())
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        assert_eq!(lit(&backend), Some((0, 0)));
    }

    /// The search key, with a player chosen whose configuration has landed and
    /// whose Home has not, so the last player's screens are still the ones
    /// showing: it searches the player it was asked of, and lights nothing of
    /// the last one's.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_search_key_opens_on_the_player_chosen_before_its_home_lands() {
        let (backend, first, second) = two_players().await;
        second.serve("/ui/Search", fixtures::home());
        choose(&backend, &first).await;
        until("the first player's Home to be lit", || {
            lit(&backend) == Some((0, 0))
        })
        .await;

        second.delay("/ui/Home", Duration::from_secs(1));
        let _ = backend.commands.send(Command::Select(second.id()));
        until("the second player's configuration", || {
            configured(&backend) == Some(second.id())
        })
        .await;
        assert_eq!(device(&backend), Some(first.id()), "its Home is held back");

        let _ = backend.commands.send(Command::OpenSearch);
        until("Search on the second player", || {
            device(&backend) == Some(second.id())
        })
        .await;
        assert_eq!(lit(&backend), Some((0, 2)), "its own Search entry");
        assert!(!first.asked_for("/ui/Search"));

        // The first player chosen back, and the second one's Home left to
        // land after it: the first player's Home, with Home lit.
        let _ = backend.commands.send(Command::Select(first.id()));
        until("the first player's Home", || {
            configured(&backend) == Some(first.id()) && device(&backend) == Some(first.id())
        })
        .await;
        until("the second player's Home to be asked", || {
            second.asked_for("/ui/Home")
        })
        .await;
        tokio::time::sleep(Duration::from_secs(1) + SETTLED).await;
        assert_eq!(device(&backend), Some(first.id()));
        assert_eq!(lit(&backend), Some((0, 0)));
    }

    /// As above, with a Home that never comes: the last player's screens stay
    /// up for as long as this one is chosen, and choosing it back keeps them.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_search_key_lights_no_search_over_the_last_players_home() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;
        until("the first player's Home to be lit", || {
            lit(&backend) == Some((0, 0))
        })
        .await;

        second.forget("/ui/Home");
        let _ = backend.commands.send(Command::Select(second.id()));
        until("the second player's configuration", || {
            configured(&backend) == Some(second.id())
        })
        .await;
        until("its Home to be asked for", || second.asked_for("/ui/Home")).await;

        let _ = backend.commands.send(Command::OpenSearch);
        until("Search to be asked of the second player", || {
            asked(&second, "/ui/Search") == 1
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        assert!(!first.asked_for("/ui/Search"));

        let _ = backend.commands.send(Command::Select(first.id()));
        until("the first player's configuration", || {
            configured(&backend) == Some(first.id())
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        assert_eq!(device(&backend), Some(first.id()), "its trail is kept");
        assert_eq!(
            lit(&backend),
            Some((0, 0)),
            "its own Home is lit, not Search and not nothing"
        );
    }

    /// A form closed by its own reply rather than by Back — a submit with no
    /// next step — goes back to the settings page it was opened on, and only
    /// the form the reply was for is closed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_form_closed_by_its_reply_returns_to_the_settings_page_it_was_opened_on() {
        let (backend, first, _) = two_players().await;
        backend.browsing.lock().unwrap().pane =
            Pane::Settings(first.id(), vec![SettingsPage::default()]);
        let wifi = show_form(
            &backend,
            first.id(),
            Replacing::Page,
            "WiFi".to_owned(),
            bluos::forms::Form::default(),
            "Reading the page…".to_owned(),
        )
        .expect("over its own settings page");

        assert!(!close_form(&backend, first.id(), wifi + 1));
        assert!(matches!(
            backend.browsing.lock().unwrap().pane,
            Pane::Form(_)
        ));
        assert!(close_form(&backend, first.id(), wifi));
        assert_eq!(settings_owner(&backend), Some(first.id()));
        assert!(
            !close_form(&backend, first.id(), wifi),
            "the page put back is not closed again"
        );
        assert_eq!(settings_owner(&backend), Some(first.id()));
    }

    /// A form the player answered with no next step puts back the page it was
    /// opened from as the player has it now, not as it was before the submit:
    /// the settings page is read again, and so is the services list.
    ///
    /// Driven through `close_answered_form` rather than a submit: a form goes
    /// to the player's web port, which a fake on a port of its own cannot be.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_form_closed_by_its_reply_reads_its_page_again() {
        let (backend, first, _) = two_players().await;
        first.serve("/Settings", ONE_SWITCH);
        choose(&backend, &first).await;
        let _ = backend
            .commands
            .send(Command::OpenSettings(None, Step::Root));
        until("the first player's settings", || {
            settings_owner(&backend) == Some(first.id())
        })
        .await;

        let form = |backend: &Backend| {
            show_form(
                backend,
                first.id(),
                Replacing::Page,
                "WiFi".to_owned(),
                bluos::forms::Form::default(),
                String::new(),
            )
            .expect("over its own page")
        };
        let wifi = form(&backend);
        close_answered_form(&backend, first.id(), wifi + 1);
        tokio::time::sleep(SETTLED).await;
        assert_eq!(asked(&first, "/Settings"), 1, "not the form it was for");

        close_answered_form(&backend, first.id(), wifi);
        until("the settings page to be read again", || {
            asked(&first, "/Settings") == 2
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        assert_eq!(settings_owner(&backend), Some(first.id()));
        assert_eq!(settings_depth(&backend), 1, "read again, not pushed");

        first.serve(
            "/redirectToCp",
            r#"<ul><li id="Tidal"><a href="/tidal">TIDAL<span class="bs-list-logo"></span></a></li></ul>"#,
        );
        backend.browsing.lock().unwrap().pane =
            Pane::Web(first.id(), WebPage::Services(Vec::new()));
        let sign_in = form(&backend);
        close_answered_form(&backend, first.id(), sign_in);
        until("the services list to be read again", || {
            matches!(
                &backend.browsing.lock().unwrap().pane,
                Pane::Web(owner, WebPage::Services(list))
                    if *owner == first.id() && list.len() == 1
            )
        })
        .await;
    }

    /// A form whose page cannot be read, opened from settings: the placeholder
    /// comes down onto the settings page, not Home.
    ///
    /// The address is off the player, so it is refused before any request and
    /// no browser is opened for it.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreadable_form_returns_to_the_settings_page_it_was_opened_on() {
        let (backend, first, _) = two_players().await;
        backend.browsing.lock().unwrap().pane =
            Pane::Settings(first.id(), vec![SettingsPage::default()]);

        let _ = backend.commands.send(Command::OpenForm {
            device: first.id(),
            title: "WiFi".to_owned(),
            path: "http://192.0.2.1/wifi".to_owned(),
        });
        until("the form to go up and come down again", || {
            backend.forms.load(Ordering::Relaxed) == 1
                && settings_owner(&backend) == Some(first.id())
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        assert_eq!(settings_owner(&backend), Some(first.id()));
    }

    /// A context menu with one line on it.
    const ONE_LINE_MENU: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<contextMenu title="A Song" version="1">
  <item text="Remove">
    <action type="player-link" URI="/Delete?id=0"></action>
  </item>
</contextMenu>"#;

    fn menu_owner(backend: &Backend) -> Option<DeviceId> {
        let browsing = backend.browsing.lock().unwrap();
        browsing.queue_menu.as_ref().and(browsing.queue_menu_owner)
    }

    fn close_menu(backend: &Backend) {
        let mut browsing = backend.browsing.lock().unwrap();
        browsing.queue_menu = None;
        browsing.queue_menu_owner = None;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_queue_rows_menu_that_lands_after_choosing_another_player_is_not_opened() {
        let (backend, first, second) = two_players().await;
        first.serve("/ui/queueCM", ONE_LINE_MENU);
        choose(&backend, &first).await;
        tokio::time::sleep(SETTLED).await;

        // Answered in time, it opens: what is dropped below is dropped for
        // being late, not for being unreadable.
        let _ = backend.commands.send(Command::QueueMenu(0));
        until("the menu", || menu_owner(&backend) == Some(first.id())).await;
        close_menu(&backend);

        first.delay("/ui/queueCM", SLOW);
        let _ = backend.commands.send(Command::QueueMenu(0));
        until("the menu to be asked for again", || {
            asked(&first, "/ui/queueCM") == 2
        })
        .await;
        choose_without_home(&backend, &second).await;
        tokio::time::sleep(SETTLED).await;

        assert_eq!(
            menu_owner(&backend),
            None,
            "the first player's track menu is not opened over the second player's queue"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_browse_rows_menu_that_lands_after_choosing_another_player_is_not_opened() {
        let (backend, first, second) = two_players().await;
        // "A Song", the first row, with dots.
        first.serve(
            "/ui/Home",
            fixtures::home().replacen(
                "</item>",
                r#"<contextMenu type="browse" URI="/ui/rowCM" resultType="contextMenu"></contextMenu></item>"#,
                1,
            ),
        );
        first.serve("/ui/rowCM", ONE_LINE_MENU);
        choose(&backend, &first).await;

        let _ = backend.commands.send(Command::BrowseMenu(0));
        until("the menu", || menu_owner(&backend) == Some(first.id())).await;
        close_menu(&backend);

        first.delay("/ui/rowCM", SLOW);
        let _ = backend.commands.send(Command::BrowseMenu(0));
        until("the menu to be asked for again", || {
            asked(&first, "/ui/rowCM") == 2
        })
        .await;
        choose_without_home(&backend, &second).await;
        tokio::time::sleep(SETTLED).await;

        assert_eq!(
            menu_owner(&backend),
            None,
            "the first player's row menu is not opened over the second player"
        );
    }

    /// The addresses held are the last configured player's until this one's
    /// configuration lands, which here it never does.
    #[tokio::test(flavor = "multi_thread")]
    async fn menus_on_a_player_not_yet_configured_are_not_asked_for_at_anothers_address() {
        let (backend, first, second) = two_players().await;
        first.serve(
            "/ui/Configuration",
            fixtures::configuration()
                .replace("/ui/queueCM", "/ui/firstQueueCM")
                .replace("/ui/nowPlayingCM", "/ui/firstNowCM"),
        );
        choose(&backend, &first).await;
        choose_without_home(&backend, &second).await;

        let _ = backend.commands.send(Command::QueueMenu(0));
        let _ = backend.commands.send(Command::NowPlayingInfo);
        until("both menus to be asked for", || {
            second.asked_for("/ui/queueItemCM") && second.asked_for("/ui/nowPlayingCM")
        })
        .await;
        assert!(
            !second.asked_for("/ui/firstQueueCM") && !second.asked_for("/ui/firstNowCM"),
            "the first player's addresses are not used on the second"
        );
    }

    /// A sidebar press refused while the configuration is out asks for it
    /// again, so two replies land for one choice. Favourites pressed between
    /// them is a press of this player's, and the second must not take it for
    /// a leftover.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_configuration_does_not_unlight_a_press_made_after_the_first() {
        let (backend, _, player) = two_players().await;
        player.serve("/ui/Favourites", fixtures::home());
        player.delay("/ui/Configuration", SLOW);
        player.delay("/ui/Home", SLOW);
        player.delay("/ui/Favourites", SLOW);

        let _ = backend.commands.send(Command::Select(player.id()));
        until("its configuration to be asked for", || {
            asked(&player, "/ui/Configuration") == 1
        })
        .await;
        tokio::time::sleep(SLOW / 2).await;
        let _ = backend.commands.send(Command::Sidebar(0, 0));
        until("the configuration to be asked for again", || {
            asked(&player, "/ui/Configuration") == 2
        })
        .await;
        until("the first configuration", || {
            configured(&backend) == Some(player.id())
        })
        .await;
        let _ = backend.commands.send(Command::Sidebar(0, 1));
        until("Favourites to be asked for", || {
            asked(&player, "/ui/Favourites") > 0
        })
        .await;
        tokio::time::sleep(SLOW + SETTLED).await;

        let browsing = backend.browsing.lock().unwrap();
        assert_eq!(
            browsing
                .trail
                .iter()
                .map(|c| c.uri.as_str())
                .collect::<Vec<_>>(),
            ["/ui/Favourites"]
        );
        assert_eq!(
            browsing.lit(),
            Some((0, 1)),
            "the entry lit is the screen showing, not Home"
        );
    }

    /// Driven through `close_answered_form`, for the reason given on the test
    /// above that reads the page again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_services_list_read_again_does_not_come_back_over_where_the_user_went() {
        let (backend, first, _) = two_players().await;
        choose(&backend, &first).await;
        first.serve(
            "/redirectToCp",
            r#"<ul><li id="Tidal"><a href="/tidal">TIDAL<span class="bs-list-logo"></span></a></li></ul>"#,
        );
        first.delay("/redirectToCp", SLOW);
        backend.browsing.lock().unwrap().pane =
            Pane::Web(first.id(), WebPage::Services(Vec::new()));
        let sign_in = show_form(
            &backend,
            first.id(),
            Replacing::Page,
            "TIDAL".to_owned(),
            bluos::forms::Form::default(),
            String::new(),
        )
        .expect("over its own page");

        close_answered_form(&backend, first.id(), sign_in);
        until("the services list to be asked for", || {
            first.asked_for("/redirectToCp")
        })
        .await;
        // Back, while the list is on its way.
        let _ = backend.commands.send(Command::BrowseBack);
        until("the list to be left", || {
            matches!(backend.browsing.lock().unwrap().pane, Pane::Browse)
        })
        .await;
        tokio::time::sleep(SETTLED).await;

        assert!(
            matches!(backend.browsing.lock().unwrap().pane, Pane::Browse),
            "the list read again is not put back over the screen Back went to"
        );
    }

    fn help_detail(backend: &Backend) -> bool {
        matches!(backend.browsing.lock().unwrap().pane, Pane::HelpDetail(..))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn diagnostics_that_land_after_choosing_another_player_are_not_shown() {
        let (backend, first, second) = two_players().await;
        // Readable, so what is dropped below is dropped for being late and
        // no browser is ever offered in its place.
        first.serve(
            "/redirectToCp",
            r#"<div class="ui-grid-a"><div class="ui-block-a">BluOS Version:</div>
<div class="ui-block-b">4.16.6</div></div>"#,
        );
        choose(&backend, &first).await;
        let row = HELP_ENTRIES
            .iter()
            .position(|(_, kind, _, _)| *kind == HelpKind::Diagnostics)
            .expect("a Diagnostics row");

        let _ = backend.commands.send(Command::HelpAction(row));
        until("the first player's diagnostics", || help_detail(&backend)).await;
        backend.browsing.lock().unwrap().pane = Pane::Help;

        first.delay("/redirectToCp", SLOW);
        let _ = backend.commands.send(Command::HelpAction(row));
        until("the diagnostics to be asked for again", || {
            asked(&first, "/redirectToCp") == 2
        })
        .await;
        choose_without_home(&backend, &second).await;
        tokio::time::sleep(SETTLED).await;

        assert!(
            !help_detail(&backend),
            "the first player's facts are not shown under the second player"
        );
    }

    /// Whose alarms the open pane belongs to.
    fn alarms_owner(backend: &Backend) -> Option<DeviceId> {
        match &backend.browsing.lock().unwrap().pane {
            Pane::Alarms(page) => Some(page.device),
            _ => None,
        }
    }

    /// A page offering to file one of `device`'s tracks, as opening `opened`.
    fn playlists_page(device: DeviceId, opened: u64) -> Pane {
        Pane::Playlists(Box::new(PlaylistPage {
            device,
            opened,
            title: "Add to playlist".to_owned(),
            options: bluos::playlists::AddToPlaylist {
                url_path: "/AddToPlaylist".to_owned(),
                parameters: vec![("file".to_owned(), "/music/a.flac".to_owned())],
                ..Default::default()
            },
            naming: false,
        }))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_track_filed_late_does_not_close_the_page_the_user_went_to() {
        let (backend, first, _) = two_players().await;
        first.serve("/AddToPlaylist", "<addToPlaylist/>");
        first.delay("/AddToPlaylist", SLOW);
        choose(&backend, &first).await;

        backend.browsing.lock().unwrap().pane = playlists_page(first.id(), 1);
        let _ = backend.commands.send(Command::PlaylistAdd(
            1,
            None,
            bluos::client::PlaylistTarget::New("Mix".to_owned()),
        ));
        until("the add", || first.asked_for("/AddToPlaylist")).await;
        // Settings, opened while the add is on its way.
        backend.browsing.lock().unwrap().pane =
            Pane::Settings(first.id(), vec![SettingsPage::default()]);
        tokio::time::sleep(SLOW + SETTLED).await;

        assert_eq!(
            settings_owner(&backend),
            Some(first.id()),
            "the add closes its own page, not the one opened since"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_playlist_chosen_on_one_page_is_not_added_from_the_next() {
        let (backend, first, second) = two_players().await;
        choose(&backend, &first).await;

        // Chosen on opening 1, the first player's. The second player's
        // playlists went up before the add was handled.
        backend.browsing.lock().unwrap().pane = playlists_page(second.id(), 2);
        let _ = backend.commands.send(Command::PlaylistAdd(
            1,
            None,
            bluos::client::PlaylistTarget::New("Mix".to_owned()),
        ));
        tokio::time::sleep(SETTLED).await;

        assert!(
            !second.asked_for("/AddToPlaylist"),
            "a choice made on one player's page is not sent with another's track"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_folder_answered_after_the_picker_is_opened_again_does_not_join_it() {
        let (backend, first, _second) = two_players().await;
        first.serve("/Alarms", fixtures::one_alarm());
        choose(&backend, &first).await;
        open_alarm(&backend, &first).await;

        let _ = backend.commands.send(Command::AlarmPick);
        until("the picker", || picker_depth(&backend) == 1).await;

        // "Inputs", the row on the first level that leads somewhere.
        first.delay("/RadioBrowse", SLOW * 2);
        let _ = backend.commands.send(Command::AlarmPickRow(1));
        until("the folder to be asked for", || {
            asked(&first, "/RadioBrowse") == 2
        })
        .await;
        let _ = backend.commands.send(Command::BrowseBack);
        until("the picker to close", || picker_depth(&backend) == 0).await;
        // Plays again, answered at once, while the folder is still out. Its
        // top level is at the depth the folder was pressed at.
        first.delay("/RadioBrowse", Duration::ZERO);
        let _ = backend.commands.send(Command::AlarmPick);
        until("the picker to open again", || picker_depth(&backend) == 1).await;
        tokio::time::sleep(SLOW * 2 + SETTLED).await;

        assert_eq!(
            picker_depth(&backend),
            1,
            "a folder pressed on the picker closed by Back is not put over the new one"
        );
    }

    /// What a player with firmware waiting answers, to the page and the check.
    const UPGRADE_WAITING: &str =
        r#"<upgrade inProgress="false" available="true" version="4.18.2"/>"#;

    fn help_row(kind: HelpKind) -> usize {
        HELP_ENTRIES
            .iter()
            .position(|(_, entry, _, _)| *entry == kind)
            .expect("a Help row")
    }

    /// The player an Install on the page showing is for, if it offers one.
    fn install_offered(backend: &Backend) -> Option<DeviceId> {
        match &backend.browsing.lock().unwrap().pane {
            Pane::HelpDetail(_, _, _, offer) => *offer,
            _ => None,
        }
    }

    fn upgrade_asked(backend: &Backend) -> Option<DeviceId> {
        backend.browsing.lock().unwrap().upgrade_asked
    }

    /// Whether a player was told to install, as opposed to asked about it.
    fn upgrade_started(player: &Player) -> bool {
        player
            .asked()
            .iter()
            .any(|seen| seen.starts_with("/upgrade?") && seen.contains("upgrade=this"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_on_an_upgrade_check_runs_only_on_the_player_it_checked() {
        let (backend, first, second) = two_players().await;
        first.serve("/upgrade", UPGRADE_WAITING);
        second.serve("/upgrade", UPGRADE_WAITING);
        choose(&backend, &first).await;
        let _ = backend
            .commands
            .send(Command::HelpAction(help_row(HelpKind::Upgrade)));
        until("the first player's check to offer Install", || {
            install_offered(&backend) == Some(first.id())
        })
        .await;

        // Asked about the first player, and another chosen under the question.
        let _ = backend.commands.send(Command::HelpAction(UPGRADE_ROW));
        until("the question", || {
            upgrade_asked(&backend) == Some(first.id())
        })
        .await;
        choose_without_home(&backend, &second).await;
        let _ = backend.commands.send(Command::UpgradeAnswer(true));
        tokio::time::sleep(SETTLED).await;
        assert!(!upgrade_started(&first) && !upgrade_started(&second));

        // The page is still up under the second player. Install asks nothing.
        let _ = backend.commands.send(Command::HelpAction(UPGRADE_ROW));
        tokio::time::sleep(SETTLED).await;
        assert_eq!(upgrade_asked(&backend), None);
        let _ = backend.commands.send(Command::UpgradeAnswer(true));
        tokio::time::sleep(SETTLED).await;
        assert!(
            !upgrade_started(&second),
            "the player chosen since is not updated off the first player's check"
        );
        assert!(!upgrade_started(&first));

        // With the player it checked selected again, it does run, there.
        let _ = backend.commands.send(Command::Select(first.id()));
        until("the first player to be selected", || {
            backend.is_selected(first.id())
        })
        .await;
        let _ = backend.commands.send(Command::HelpAction(UPGRADE_ROW));
        until("the question", || {
            upgrade_asked(&backend) == Some(first.id())
        })
        .await;
        let _ = backend.commands.send(Command::UpgradeAnswer(true));
        until("the first player's update", || upgrade_started(&first)).await;
        assert!(!upgrade_started(&second));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_is_not_offered_on_the_next_help_page() {
        let (backend, first, second) = two_players().await;
        first.serve("/upgrade", UPGRADE_WAITING);
        second.serve("/upgrade", UPGRADE_WAITING);
        second.serve(
            "/redirectToCp",
            r#"<div class="ui-grid-a"><div class="ui-block-a">BluOS Version:</div>
<div class="ui-block-b">4.16.6</div></div>"#,
        );
        choose(&backend, &first).await;
        let _ = backend
            .commands
            .send(Command::HelpAction(help_row(HelpKind::Upgrade)));
        until("the first player's check to offer Install", || {
            install_offered(&backend) == Some(first.id())
        })
        .await;
        let _ = backend.commands.send(Command::BrowseBack);
        until("Help", || {
            matches!(backend.browsing.lock().unwrap().pane, Pane::Help)
        })
        .await;

        choose(&backend, &second).await;
        let _ = backend
            .commands
            .send(Command::HelpAction(help_row(HelpKind::Diagnostics)));
        until("the second player's diagnostics", || help_detail(&backend)).await;
        assert_eq!(install_offered(&backend), None);

        let _ = backend.commands.send(Command::HelpAction(UPGRADE_ROW));
        tokio::time::sleep(SETTLED).await;
        let _ = backend.commands.send(Command::UpgradeAnswer(true));
        tokio::time::sleep(SETTLED).await;
        assert!(
            !upgrade_started(&second),
            "an Install left from one player's check is not pressed on another's page"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_upgrade_check_that_lands_after_choosing_another_player_is_not_shown() {
        let (backend, first, second) = two_players().await;
        first.serve("/upgrade", UPGRADE_WAITING);
        first.delay("/upgrade", SLOW);
        choose(&backend, &first).await;

        let _ = backend
            .commands
            .send(Command::HelpAction(help_row(HelpKind::Upgrade)));
        until("the check to be asked for", || first.asked_for("/upgrade")).await;
        choose_without_home(&backend, &second).await;
        // Two round trips, each held back.
        tokio::time::sleep(SLOW * 2 + SETTLED).await;

        assert!(
            !help_detail(&backend),
            "the first player's update is not shown under the second player"
        );
        assert_eq!(install_offered(&backend), None);
    }

    /// The pages that would have been opened in a browser on `player`'s
    /// control port.
    fn opened_on(player: &Player) -> Vec<String> {
        let address = player.address().to_string();
        OPENED_IN_BROWSER
            .lock()
            .unwrap()
            .iter()
            .filter(|url| url.contains(&address))
            .cloned()
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unreadable_diagnostics_that_land_after_choosing_another_player_open_no_browser() {
        let (backend, first, second) = two_players().await;
        // Not served, so the page cannot be read and the fallback is taken.
        first.delay("/redirectToCp", SLOW);
        choose(&backend, &first).await;
        let row = help_row(HelpKind::Diagnostics);

        let _ = backend.commands.send(Command::HelpAction(row));
        until("the diagnostics to be asked for", || {
            first.asked_for("/redirectToCp")
        })
        .await;
        choose_without_home(&backend, &second).await;
        tokio::time::sleep(SETTLED).await;
        assert_eq!(
            opened_on(&first),
            Vec::<String>::new(),
            "the first player's page is not opened while the second is selected"
        );

        // Pressed with the first player selected, the fallback is taken.
        let _ = backend.commands.send(Command::Select(first.id()));
        until("the first player to be selected", || {
            backend.is_selected(first.id())
        })
        .await;
        let _ = backend.commands.send(Command::HelpAction(row));
        until("the browser", || !opened_on(&first).is_empty()).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_list_read_again_that_cannot_be_read_opens_no_browser() {
        let (backend, first, _) = two_players().await;
        choose(&backend, &first).await;
        // The services list is not served on this player. The shares page
        // is read from port 80 on the player's host, where nothing answers.
        let fallbacks = || {
            OPENED_IN_BROWSER
                .lock()
                .unwrap()
                .iter()
                .filter(|url| url.contains("/services?noheader") || url.contains("/sharecfg"))
                .count()
        };

        backend.browsing.lock().unwrap().pane =
            Pane::Web(first.id(), WebPage::Services(Vec::new()));
        let _ = backend.commands.send(Command::OpenServices {
            device: first.id(),
            reload: true,
        });
        until("the services list to be asked for", || {
            first.asked_for("/redirectToCp")
        })
        .await;
        backend.browsing.lock().unwrap().pane = Pane::Web(
            first.id(),
            WebPage::Shares {
                action: None,
                shares: Vec::new(),
            },
        );
        let _ = backend.commands.send(Command::OpenShares {
            device: first.id(),
            reload: true,
        });
        tokio::time::sleep(SETTLED).await;
        assert_eq!(
            fallbacks(),
            0,
            "a reload nobody asked a window for opens none"
        );

        // Opened rather than reloaded, both fall back to the browser.
        let _ = backend.commands.send(Command::OpenServices {
            device: first.id(),
            reload: false,
        });
        let _ = backend.commands.send(Command::OpenShares {
            device: first.id(),
            reload: false,
        });
        until("both fallbacks", || fallbacks() == 2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_press_left_from_before_choosing_away_and_back_is_not_lit_on_home() {
        let (backend, first, second) = two_players().await;
        first.serve("/ui/Favourites", fixtures::home());
        first.delay("/ui/Home", SLOW);
        first.delay("/ui/Favourites", SLOW);

        let _ = backend.commands.send(Command::Select(first.id()));
        until("its configuration", || {
            configured(&backend) == Some(first.id())
        })
        .await;
        let _ = backend.commands.send(Command::Sidebar(0, 1));
        until("Favourites to be asked for", || {
            asked(&first, "/ui/Favourites") > 0
        })
        .await;
        // Another chosen before either lands, so both are dropped, and then
        // this one again. It is still the player configured.
        choose_without_home(&backend, &second).await;
        assert_eq!(configured(&backend), Some(first.id()));
        first.delay("/ui/Home", Duration::ZERO);
        choose(&backend, &first).await;
        tokio::time::sleep(SETTLED).await;

        assert_eq!(
            lit(&backend),
            Some((0, 0)),
            "Home is showing, so Home is lit, not the press dropped before"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_press_whose_screen_failed_is_not_lit_on_the_home_asked_for_again() {
        let (backend, first, _) = two_players().await;
        // Neither Home nor Favourites is served.
        first.forget("/ui/Home");

        let _ = backend.commands.send(Command::Select(first.id()));
        until("its Home to be asked for", || {
            asked(&first, "/ui/Home") == 1
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        let _ = backend.commands.send(Command::Sidebar(0, 1));
        until("Favourites to be asked for", || {
            asked(&first, "/ui/Favourites") == 1
        })
        .await;
        tokio::time::sleep(SETTLED).await;
        assert_eq!(
            lit(&backend),
            Some((0, 1)),
            "a failed screen leaves its entry lit"
        );

        // Its card pressed again, and Home answers this time.
        first.serve("/ui/Home", fixtures::home());
        let _ = backend.commands.send(Command::Select(first.id()));
        until("its Home", || device(&backend) == Some(first.id())).await;
        tokio::time::sleep(SETTLED).await;

        assert_eq!(lit(&backend), Some((0, 0)));
    }
}
