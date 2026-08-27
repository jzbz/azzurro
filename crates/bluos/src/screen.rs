//! The server-driven screens.
//!
//! The controller does not know what a music service's menus look like, and it
//! does not need to: the player describes each screen as XML and the client
//! renders it. `/ui/Configuration` lists the screens that exist, every
//! `browse` action names the next document to fetch, and browsing anything —
//! the library, TuneIn, a service released next year — is the same loop.
//!
//! ```text
//! GET /ui/Configuration          → home, sources, favourites, search, queue…
//! GET /ui/Sources                → a <screen> of rows and items
//!     item → action browse       → /ui/BrowseObjects?service=Airable&…
//! GET /ui/BrowseObjects?…        → another <screen>
//! ```
//!
//! The vocabulary is closed and small — around twenty-five elements and nine
//! action types — so this is a `match`, not a general-purpose document
//! renderer. That is what makes the whole thing tractable in a toolkit with no
//! runtime widget tree.
//!
//! Parsed by hand with a pull parser rather than by `serde`, because the
//! children are heterogeneous: a row holds any of eight different kinds of
//! item, and each of those holds actions, buttons and match rules.

use std::collections::BTreeMap;

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::error::{Error, Result};
use crate::xml::{attributes, entity as resolve_entity, flag, local_name};

/// One screen, as the player describes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Screen {
    /// Heading for the screen. Some documents use `navigationTitle` instead,
    /// which [`Screen::heading`] folds together.
    pub title: Option<String>,
    pub navigation_title: Option<String>,
    pub navigation_icon: Option<String>,
    pub id: Option<String>,
    /// Second line, on the documents that have one — a context menu names the
    /// track it was opened on.
    pub subtitle: Option<String>,
    /// Artwork for the thing the screen is about.
    pub image: Option<String>,
    /// Whether this arrived as a `<contextMenu>` rather than a `<screen>`.
    /// Same vocabulary either way; only the framing differs.
    pub is_context_menu: bool,
    /// Which service this screen belongs to, where it belongs to one.
    pub service: Option<String>,
    /// Redraw when the selected player changes.
    pub refresh_on_player_change: bool,
    /// Redraw when a `/Status` field takes a given value: `(key, value)`.
    /// The player is telling the client what to watch, so a client does not
    /// have to guess when a screen has gone stale.
    pub refresh_on: Vec<(String, String)>,
    /// Screen-level actions, shown by the official app in a menu.
    pub menu_actions: Vec<MenuAction>,
    /// The row of buttons a document puts under everything else. The play queue
    /// is the one that has them: Save, Edit, Clear, and — to a client that says
    /// it understands a new enough schema — Queue Builder Mode.
    pub buttons: Vec<Button>,
    /// Present only on the play queue, and the way to tell it from a browse
    /// screen. Its own struct rather than four more fields here, because paging
    /// is the queue's business and no other screen has any.
    pub queue: Option<QueuePage>,
    /// The player's own "you are in a mode" bar, served on every screen for as
    /// long as the mode lasts.
    pub mode_indicator: Option<ModeIndicator>,
    /// What a screen about one thing says about it. An album's page opens with
    /// its cover, who made it, when, and what can be done with the whole of it.
    pub header: Option<Header>,
    /// Where the rest of a long list is.
    ///
    /// A library's Artists page answers thirty names of four hundred and
    /// forty-eight and hands over the request for the next thirty. Without
    /// following it there is no way to reach the four hundred and eighteen.
    pub next: Option<String>,
    pub sections: Vec<Section>,
}

impl Screen {
    /// The best available heading.
    pub fn heading(&self) -> Option<&str> {
        self.title.as_deref().or(self.navigation_title.as_deref())
    }

    /// Every item on the screen, in order, ignoring which section it came
    /// from. Convenient for a flat list view.
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.sections.iter().flat_map(|s| s.items.iter())
    }

    pub fn is_empty(&self) -> bool {
        self.sections.iter().all(|s| s.items.is_empty())
    }

    /// Whether this screen has gone stale.
    ///
    /// `refreshOnStatusChange` records what a field held when the screen was
    /// built. The player is not asking to be polled — it is saying "this
    /// drawing is only valid while `sid` is still 8", so the screen wants
    /// re-fetching once that stops being true.
    pub fn is_stale(&self, status: &crate::Status) -> bool {
        // A key the status cannot answer for — one [`crate::Status::field`]
        // does not model, or one the player left out — is "cannot tell", not
        // "changed". Reading it as changed made the screen permanently stale:
        // no reply could ever satisfy the comparison, so every status tick
        // spawned another refetch of the same screen, once a second, for as
        // long as it stayed open. `is_playing` on the same field already takes
        // the cautious reading; this now matches it.
        self.refresh_on
            .iter()
            .any(|(key, value)| status.field(key).is_some_and(|actual| actual != *value))
    }
}

/// How a group of items wants to be laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SectionKind {
    /// A plain vertical list.
    #[default]
    List,
    /// A shelf. `scrollable` means it runs off the side of the screen.
    Row,
    /// A picker — "Select Service" — where choosing an entry replaces the
    /// screen rather than pushing a new one.
    SelectorMenu,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Section {
    pub kind: SectionKind,
    pub id: Option<String>,
    pub title: Option<String>,
    /// Only meaningful on a row.
    pub scrollable: bool,
    /// The player pinning this section where it is. The home screen marks its
    /// teaser row so, and a client offering to reorder the screen has to leave
    /// that one alone.
    pub no_reorder: bool,
    /// On a selector menu, whether picking replaces the current screen.
    pub replace_screen: bool,
    pub menu_actions: Vec<MenuAction>,
    pub buttons: Vec<Button>,
    /// What the section itself leads to, where it leads anywhere.
    ///
    /// A library's front page is nine of these: `<row title="Artists">` with an
    /// action and nothing inside it. The player is describing a heading that is
    /// also a way in, and the official controller draws those as plain rows
    /// with a chevron rather than as empty shelves.
    pub action: Option<Action>,
    pub items: Vec<Item>,
}

/// What kind of thing an item is. Mostly a presentation hint: they all carry
/// the same fields and behave the same way when activated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ItemKind {
    #[default]
    Item,
    /// An entry on the Sources screen.
    Source,
    /// A physical input.
    Input,
    /// A music service.
    Service,
    /// A promotional card.
    Teaser,
    /// A cover tile.
    Thumbnail,
    /// Explanatory text where a list would otherwise be empty.
    InfoPanel,
    /// A search box, not a row to click.
    Search,
    Footer,
    /// "Customise" affordance at the end of a screen.
    Customise,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Item {
    pub kind: ItemKind,
    pub title: Option<String>,
    /// Some documents label with `text` rather than `title`; see
    /// [`Item::label`].
    pub text: Option<String>,
    pub subtitle: Option<String>,
    pub subsubtitle: Option<String>,
    pub body: Option<String>,
    pub icon: Option<String>,
    pub image: Option<String>,
    pub duration: Option<String>,
    pub quality: Option<String>,
    pub service: Option<String>,
    /// `Album`, `Song`, `Artist`, and so on.
    pub object_type: Option<String>,
    pub selected: bool,
    /// What happens when it is activated.
    pub action: Option<Action>,
    /// Present where activating and playing are different gestures — a tile
    /// that opens an album but has a play button on it.
    pub play_action: Option<Action>,
    /// The long-press or right-click menu, fetched on demand.
    pub context_menu: Option<Action>,
    pub buttons: Vec<Button>,
    /// `(status key, value)`: this item is the one playing when `/Status` has
    /// that field set to that value. The player is telling the client how to
    /// decide, instead of the client inferring it.
    pub now_playing_match: Option<(String, String)>,
    /// Anything not modelled above, kept rather than discarded so a client can
    /// still pass it back.
    pub extra: BTreeMap<String, String>,
}

impl Item {
    /// The label to draw.
    pub fn label(&self) -> Option<&str> {
        self.title.as_deref().or(self.text.as_deref())
    }

    /// Whether activating this item does anything at all. Screens do contain
    /// inert rows — the library browse screen lists the physical inputs
    /// without actions on them.
    pub fn is_actionable(&self) -> bool {
        self.action.is_some() || self.play_action.is_some()
    }

    /// Placeholder text for a search box — "Search..." — as distinct from its
    /// label, which is what [`Item::label`] returns.
    pub fn prompt(&self) -> Option<&str> {
        self.extra.get("prompt").map(String::as_str)
    }

    /// Where a search box wants the user's text: the player names the query
    /// parameter rather than fixing one.
    pub fn search_parameter(&self) -> Option<&str> {
        if self.kind != ItemKind::Search {
            return None;
        }
        self.extra.get("parameterName").map(String::as_str)
    }

    /// The path to fetch to search for `query`.
    ///
    /// Returns `None` for anything that is not a search box, or for one the
    /// player described without enough to act on.
    pub fn search_url(&self, query: &str) -> Option<String> {
        let parameter = self.search_parameter()?;
        let uri = self.action.as_ref()?.uri.as_deref()?;
        let separator = if uri.contains('?') { '&' } else { '?' };
        let encoded = utf8_percent_encode(query, QUERY_VALUE);
        Some(format!("{uri}{separator}{parameter}={encoded}"))
    }

    /// Whether `status` says this is the item currently playing.
    pub fn is_playing(&self, status: &crate::Status) -> bool {
        let Some((key, value)) = &self.now_playing_match else {
            return false;
        };
        status.field(key).is_some_and(|actual| &actual == value)
    }
}

/// What an action does. The ten the player emits, plus a catch-all so an
/// unknown type is inert rather than fatal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActionKind {
    /// Fetch `uri`; the result is another screen.
    Browse,
    /// Fetch `uri`; the result is a context menu.
    ContextBrowse,
    /// Send `uri` to the player as a command. Nothing to render.
    PlayerLink,
    /// Navigate within the client. Not an HTTP path — see
    /// [`Action::deep_link_target`].
    DeepLink,
    /// Open in a browser. Usually Lenbrook's cloud control panel.
    Webpage,
    /// A player settings page, also a web page.
    Setting,
    /// Add to the queue or to a playlist.
    Add,
    /// Reorder the queue.
    Reorder,
    /// A player-link the player wants confirmed before it is sent. `title`
    /// carries the question to ask.
    Confirmation,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Action {
    pub kind: ActionKind,
    /// The path to fetch or send. Already unescaped.
    pub uri: Option<String>,
    pub href: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    /// What the player says will come back: `screen`, `contextMenu`, `queue`…
    pub result_type: Option<String>,
    pub service: Option<String>,
    /// Re-read the current screen once this has been sent.
    pub refresh_screen: bool,
    /// Close the current screen once this has been sent.
    pub close_screen: bool,
    /// What to tell the user once it has been sent — "Added to favourites".
    /// The player supplies the wording, so a client need not invent any.
    pub notification: Option<String>,
    pub extra: BTreeMap<String, String>,
}

impl Action {
    /// A deep link's target, with the leading marker stripped.
    ///
    /// These are the official app's own routes — `/settings`, `/edit-queue`,
    /// `/music-service/LocalMusic` — and not paths on the player. A client has
    /// to decide for itself what each means.
    pub fn deep_link_target(&self) -> Option<&str> {
        if self.kind != ActionKind::DeepLink {
            return None;
        }
        self.uri.as_deref()
    }

    /// Whether following this means fetching another document to render.
    pub fn is_navigational(&self) -> bool {
        matches!(self.kind, ActionKind::Browse | ActionKind::ContextBrowse)
    }

    /// Whether following this leads to a menu of categories rather than to
    /// things to play.
    ///
    /// The player says so in the request it hands over: a browse into a
    /// service's own menu carries `type=BrowseMenu`, where a browse into its
    /// content carries the kind of object instead — `type=Artist` on a search
    /// screen's Artists row. It is worth telling apart because the picture on a
    /// menu row is the service's branding for a category, and the picture on a
    /// content row is the thing itself.
    pub fn is_browse_menu(&self) -> bool {
        self.uri
            .as_deref()
            .is_some_and(|uri| uri.contains("type=BrowseMenu"))
    }
}

/// What the `<queue>` root says about itself.
///
/// `/ui/Queue` is a window onto the queue rather than the whole of it: `offset`
/// is where this document starts and `total` is how many tracks there are
/// altogether.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueuePage {
    pub offset: u32,
    pub total: u32,
    /// The queue has been changed away from the playlist that filled it, which
    /// is the only thing that makes saving it worth offering.
    pub modified: bool,
    /// The playlist it was filled from, when it came from one.
    pub name: Option<String>,
}

/// The block at the top of a screen that is about one thing.
///
/// An album, an artist, a playlist: the player sends the artwork, the three
/// lines it wants shown, and the buttons for acting on the whole of it — "Play
/// all" and "Shuffle" on an album.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Header {
    pub image: Option<String>,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    /// The third line: "1982 • 1 Tracks".
    pub subsubtitle: Option<String>,
    pub buttons: Vec<Button>,
}

/// A bar the player puts on every screen while a mode is on.
///
/// Queue Builder Mode is the one that uses it. What matters is that the way out
/// arrives with it: the buttons here carry the action that leaves the mode, so a
/// client never has to guess the request that turns it off.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModeIndicator {
    pub text: Option<String>,
    pub icon: Option<String>,
    pub buttons: Vec<Button>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Button {
    pub text: Option<String>,
    pub icon: Option<String>,
    /// The player's own recommendation of which button matters right now.
    pub highlight: bool,
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MenuAction {
    pub text: Option<String>,
    /// `settings`, `add`, and so on — a hint at which icon to use.
    pub kind: Option<String>,
    pub action: Option<Action>,
}

/// Everything that would otherwise be read as query-string structure, plus the
/// characters a URL cannot carry raw. Deliberately narrow: over-encoding a
/// search term works but makes the request unreadable when it needs debugging.
const QUERY_VALUE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Where the parser currently is, so that a nested `<action>` can be attached
/// to whatever opened it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Screen,
    Section,
    /// A `<list>` inside a `<row>`, which groups but does not start a section.
    NestedList,
    Item,
    MenuAction,
    Button,
    Action,
    /// The alphabet down the side of a long list. Its children look exactly
    /// like content and are not.
    Index,
    /// The block at the top of a screen about one thing, which has buttons.
    Header,
    /// The request for the next page of a long list, whose text is the uri.
    NextLink,
    /// Anything unrecognized, so its end tag pops the right thing.
    Other,
}

/// Read a `<screen>` document.
pub fn parse(xml: &str) -> Result<Screen> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<Ctx> = Vec::new();
    let mut screen = Screen::default();
    let mut section: Option<Section> = None;
    let mut item: Option<Item> = None;
    let mut menu_action: Option<MenuAction> = None;
    let mut button: Option<Button> = None;
    let mut seen_screen = false;

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                start(
                    name,
                    &e,
                    &mut stack,
                    &mut screen,
                    &mut section,
                    &mut item,
                    &mut menu_action,
                    &mut button,
                    &mut seen_screen,
                )?;
            }
            Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                start(
                    name,
                    &e,
                    &mut stack,
                    &mut screen,
                    &mut section,
                    &mut item,
                    &mut menu_action,
                    &mut button,
                    &mut seen_screen,
                )?;
                end(
                    name,
                    &mut stack,
                    &mut screen,
                    &mut section,
                    &mut item,
                    &mut menu_action,
                    &mut button,
                );
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                end(
                    name,
                    &mut stack,
                    &mut screen,
                    &mut section,
                    &mut item,
                    &mut menu_action,
                    &mut button,
                );
            }
            // The only element whose text matters: the request for the next
            // page of a long list is written between its tags rather than as
            // an attribute.
            Ok(Event::Text(text)) if stack.last() == Some(&Ctx::NextLink) => {
                if let Some(next) = screen.next.as_mut()
                    && let Ok(raw) = text.decode()
                {
                    next.push_str(&raw);
                }
            }

            // An entity arrives as an event of its own, so a continuation's
            // `&amp;` would otherwise fall out from between its parameters and
            // leave a request the player cannot read.
            Ok(Event::GeneralRef(entity)) if stack.last() == Some(&Ctx::NextLink) => {
                if let Some(next) = screen.next.as_mut()
                    && let Ok(name) = entity.decode()
                {
                    next.push_str(&resolve_entity(name.as_ref()));
                }
            }
            Ok(_) => {}
            Err(e) => return Err(Error::Screen(format!("malformed XML: {e}"))),
        }
    }

    // Trimmed once at the end rather than per fragment, for the same reason.
    if let Some(next) = screen.next.as_mut() {
        let trimmed = next.trim().to_owned();
        screen.next = (!trimmed.is_empty()).then_some(trimmed);
    }

    if !seen_screen {
        return Err(Error::Screen(
            "no <screen>, <contextMenu> or <queue> element".into(),
        ));
    }

    // A document whose items sit straight under <screen> still needs somewhere
    // to keep them.
    if let Some(section) = section.take()
        && !section.items.is_empty()
    {
        screen.sections.push(section);
    }
    Ok(screen)
}

const ITEM_ELEMENTS: &[(&str, ItemKind)] = &[
    ("item", ItemKind::Item),
    ("source", ItemKind::Source),
    ("input", ItemKind::Input),
    ("service", ItemKind::Service),
    ("teaser", ItemKind::Teaser),
    ("largeThumbnail", ItemKind::Thumbnail),
    ("smallThumbnail", ItemKind::Thumbnail),
    ("infoPanel", ItemKind::InfoPanel),
    ("search", ItemKind::Search),
    ("footer", ItemKind::Footer),
    ("customiseScreen", ItemKind::Customise),
];

#[allow(clippy::too_many_arguments)]
fn start(
    name: &str,
    e: &BytesStart<'_>,
    stack: &mut Vec<Ctx>,
    screen: &mut Screen,
    section: &mut Option<Section>,
    item: &mut Option<Item>,
    menu_action: &mut Option<MenuAction>,
    button: &mut Option<Button>,
    seen_screen: &mut bool,
) -> Result<()> {
    let mut a = attributes(e);

    match name {
        // `contextMenu` is two different things depending on where it sits: a
        // root element when a whole document is a menu, and an action on an
        // item when a row has one to open. Only position tells them apart.
        // `queue` is the third root the player serves. Everything inside it is
        // ordinary screen furniture — items with actions, context menus and
        // now-playing matches, and a row of buttons — so it parses as a screen
        // and only its own attributes need lifting.
        "screen" | "contextMenu" | "queue" if stack.is_empty() => {
            *seen_screen = true;
            screen.is_context_menu = name == "contextMenu";
            if name == "queue" {
                screen.queue = Some(QueuePage {
                    offset: a.remove("offset").and_then(|v| v.parse().ok()).unwrap_or(0),
                    total: a.remove("total").and_then(|v| v.parse().ok()).unwrap_or(0),
                    // The queue differs from the playlist that filled it. The
                    // player acts on this itself, putting `highlight` on the
                    // Save button rather than leaving a client to work out when
                    // saving is worth offering.
                    modified: flag(a.remove("modified")),
                    name: a.remove("name"),
                });
            }
            screen.subtitle = a.remove("subTitle");
            screen.image = a.remove("image");
            // A screen uses `screenTitle`; a context menu uses `title`.
            screen.title = a.remove("screenTitle").or_else(|| a.remove("title"));
            screen.navigation_title = a.remove("navigationTitle");
            screen.navigation_icon = a.remove("navigationIcon");
            screen.id = a.remove("id");
            screen.service = a.remove("service");
            screen.refresh_on_player_change = flag(a.remove("refreshOnPlayerChange"));
            stack.push(Ctx::Screen);
        }

        // The bar that says a mode is on. Parsed mainly so its buttons do not
        // end up in the document's own button row — they close with no item and
        // no section open, which is where the queue's Save and Clear live.
        "modeIndicator" => {
            screen.mode_indicator = Some(ModeIndicator {
                text: a.remove("text"),
                icon: a.remove("icon"),
                buttons: Vec::new(),
            });
            stack.push(Ctx::Other);
        }

        "refreshOnStatusChange" => {
            if let (Some(key), Some(value)) = (a.remove("key"), a.remove("value")) {
                screen.refresh_on.push((key, value));
            }
            stack.push(Ctx::Other);
        }

        // A jump index: `<index><item key="A" offset="6" length="22"/>…`, which
        // a library's Artists page puts at the top of four hundred names. Its
        // items carry no title and no action — they are somewhere to scroll to,
        // not something to open — so drawing them gave a screenful of blank
        // rows before the first artist.
        "index" => {
            stack.push(Ctx::Index);
        }

        "nextLink" => {
            screen.next = Some(String::new());
            stack.push(Ctx::NextLink);
        }

        "header" => {
            screen.header = Some(Header {
                image: a.remove("image"),
                title: a.remove("title"),
                subtitle: a.remove("subTitle"),
                subsubtitle: a.remove("subSubTitle"),
                buttons: Vec::new(),
            });
            stack.push(Ctx::Header);
        }

        "row" | "selectorMenu" => {
            *section = Some(Section {
                kind: if name == "row" {
                    SectionKind::Row
                } else {
                    SectionKind::SelectorMenu
                },
                id: a.remove("id"),
                title: a.remove("title").or_else(|| a.remove("menuTitle")),
                scrollable: flag(a.remove("scrollable")),
                no_reorder: flag(a.remove("noReorder")),
                replace_screen: flag(a.remove("replaceScreen")),
                ..Default::default()
            });
            stack.push(Ctx::Section);
        }

        // A <list> is a section on its own, but only when it is not already
        // inside a row that is grouping it.
        "list" => {
            if section.is_some() {
                stack.push(Ctx::NestedList);
            } else {
                *section = Some(Section {
                    kind: SectionKind::List,
                    id: a.remove("id"),
                    ..Default::default()
                });
                stack.push(Ctx::Section);
            }
        }

        "menuAction" => {
            *menu_action = Some(MenuAction {
                text: a.remove("text"),
                kind: a.remove("type"),
                action: None,
            });
            stack.push(Ctx::MenuAction);
        }

        "button" => {
            *button = Some(Button {
                text: a.remove("text"),
                icon: a.remove("icon"),
                // `backgroundColor` and `textColor` are dropped on purpose:
                // this app draws the player's furniture in its own theme, the
                // same decision `glyphs` records for the player's icons. But
                // `highlight` is not decoration — it is the player saying which
                // button is the one to press, which is how Save lights up once
                // the queue has been changed.
                highlight: flag(a.remove("highlight")),
                action: None,
            });
            stack.push(Ctx::Button);
        }

        "action" | "playAction" | "contextMenu" => {
            let built = action(name, a);
            // Whoever opened most recently owns it.
            match stack.last() {
                Some(Ctx::Button) => {
                    if let Some(button) = button {
                        button.action = Some(built);
                    }
                }
                Some(Ctx::MenuAction) => {
                    if let Some(menu_action) = menu_action {
                        menu_action.action = Some(built);
                    }
                }
                _ => {
                    if let Some(item) = item {
                        match name {
                            "playAction" => item.play_action = Some(built),
                            "contextMenu" => item.context_menu = Some(built),
                            _ => item.action = Some(built),
                        }
                    // No item open, so it belongs to the section: an empty row
                    // whose action is the whole of its content.
                    } else if let Some(section) = section {
                        section.action = Some(built);
                    }
                }
            }
            stack.push(Ctx::Action);
        }

        "nowPlayingMatch" => {
            if let (Some(key), Some(value)) = (a.remove("key"), a.remove("value"))
                && let Some(item) = item
            {
                item.now_playing_match = Some((key, value));
            }
            stack.push(Ctx::Other);
        }

        _ => {
            if stack.last() == Some(&Ctx::Index) {
                stack.push(Ctx::Other);
            } else if let Some((_, kind)) = ITEM_ELEMENTS.iter().find(|(n, _)| *n == name) {
                // A search box is the one item whose action lives on the
                // element itself, so it has to be lifted out before the rest of
                // the attributes are swept into `extra`.
                let own_action = (*kind == ItemKind::Search).then(|| {
                    let mut owned = BTreeMap::new();
                    for key in ["type", "URI", "href", "url", "resultType", "service"] {
                        if let Some(value) = a.remove(key) {
                            owned.insert(key.to_owned(), value);
                        }
                    }
                    action("action", owned)
                });

                *item = Some(Item {
                    kind: *kind,
                    action: own_action,
                    title: a.remove("title"),
                    subtitle: a.remove("subTitle"),
                    subsubtitle: a.remove("subSubTitle"),
                    body: a.remove("body"),
                    icon: a.remove("icon"),
                    image: a.remove("image").or_else(|| a.remove("backgroundImage")),
                    duration: a.remove("duration"),
                    quality: a.remove("quality"),
                    service: a.remove("service"),
                    object_type: a.remove("objectType"),
                    selected: flag(a.remove("selected")),
                    text: a.remove("text"),
                    extra: a,
                    ..Default::default()
                });
                stack.push(Ctx::Item);
            } else {
                stack.push(Ctx::Other);
            }
        }
    }
    Ok(())
}

fn end(
    name: &str,
    stack: &mut Vec<Ctx>,
    screen: &mut Screen,
    section: &mut Option<Section>,
    item: &mut Option<Item>,
    menu_action: &mut Option<MenuAction>,
    button: &mut Option<Button>,
) {
    let ctx = stack.pop();

    match ctx {
        Some(Ctx::Item) => {
            if let Some(done) = item.take() {
                section
                    .get_or_insert_with(Section::default)
                    .items
                    .push(done);
            }
        }
        Some(Ctx::Section) => {
            if let Some(done) = section.take() {
                screen.sections.push(done);
            }
        }
        Some(Ctx::MenuAction) => {
            if let Some(done) = menu_action.take() {
                // Not `section.is_some()`: a screen whose items sit loose under
                // the root — every context menu — has a section open that no
                // tag opened and no tag will close, built to hold them by the
                // `Ctx::Item` arm above. Everything at the root after the first
                // of those items would have been swallowed by it. The stack
                // says what is really open.
                match section.as_mut().filter(|_| in_section(stack)) {
                    Some(section) => section.menu_actions.push(done),
                    None => screen.menu_actions.push(done),
                }
            }
        }
        Some(Ctx::Button) => {
            if let Some(done) = button.take() {
                // Innermost open context wins, the same rule `<action>` follows.
                match (
                    item.as_mut(),
                    // Only a section a tag actually opened; see `Ctx::MenuAction`.
                    section.as_mut().filter(|_| in_section(stack)),
                    screen.mode_indicator.as_mut(),
                ) {
                    (Some(item), _, _) => item.buttons.push(done),
                    (None, Some(section), _) => section.buttons.push(done),
                    (None, None, Some(mode)) => mode.buttons.push(done),
                    // "Play all" and "Shuffle" belong to the thing the screen
                    // is about rather than to the screen.
                    (None, None, None) if screen.header.is_some() => {
                        if let Some(header) = screen.header.as_mut() {
                            header.buttons.push(done);
                        }
                    }
                    // Nothing else open, so it belongs to the document: the
                    // play queue's button row sits directly under its root.
                    (None, None, None) => screen.buttons.push(done),
                }
            }
        }
        _ => {}
    }
    let _ = name;
}

/// Whether a `<row>`, `<list>` or `<selectorMenu>` is open around here.
///
/// Called after the closing element has been popped, so the top of the stack
/// is what encloses it.
fn in_section(stack: &[Ctx]) -> bool {
    matches!(stack.last(), Some(Ctx::Section | Ctx::NestedList))
}

pub(crate) fn action(element: &str, mut a: BTreeMap<String, String>) -> Action {
    let declared = a.remove("type").unwrap_or_default();
    let kind = match declared.as_str() {
        "browse" => ActionKind::Browse,
        "context-browse" => ActionKind::ContextBrowse,
        "player-link" => ActionKind::PlayerLink,
        "deep-link" => ActionKind::DeepLink,
        "webpage" => ActionKind::Webpage,
        // The screen-level menu uses `settings` where an item uses `setting`.
        "setting" | "settings" => ActionKind::Setting,
        "add" => ActionKind::Add,
        "reorder" => ActionKind::Reorder,
        // Everything a player-link is, plus the player's own instruction to ask
        // first. Clearing the queue arrives this way once a client declares a
        // new enough schema, with the question to put to the user in `title`.
        "confirmation" => ActionKind::Confirmation,
        // A <contextMenu> with no type is still a context browse.
        "" if element == "contextMenu" => ActionKind::ContextBrowse,
        _ => ActionKind::Unknown,
    };

    Action {
        kind,
        uri: a.remove("URI"),
        href: a.remove("href"),
        url: a.remove("url"),
        title: a.remove("title"),
        result_type: a.remove("resultType"),
        service: a.remove("service"),
        refresh_screen: flag(a.remove("refreshScreen")),
        close_screen: flag(a.remove("closeScreen")),
        notification: a.remove("notification"),
        extra: a,
    }
}

/// The same action, but adding to the queue instead of playing.
///
/// A track's action is a play command wrapped in the player's own
/// `/ui/prf?…&u=<command>`, and the command inside is `/Add?playnow=1&file=…`.
/// Dropping `playnow` is the whole difference between "play this now" and "put
/// this at the end" — verified on a Powernode, where `/Add?where=last&file=…`
/// grew the queue from three to four without interrupting what was playing.
///
/// Returns `None` where the action is not one that plays, so a caller can tell
/// "nothing to change" from "changed".
pub fn appending(uri: &str) -> Option<String> {
    let (path, query) = uri.split_once('?')?;

    // The wrapper carries the real command percent-encoded in `u`, and repeats
    // its parameters in `a` for the player's own bookkeeping. Both mention
    // playing; only `u` decides it.
    let mut parts: Vec<String> = Vec::new();
    let mut changed = false;

    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("u", encoded)) => {
                let (inner, dropped) = without_playnow(encoded);
                changed |= dropped;
                parts.push(format!("u={inner}"));
            }
            _ => parts.push(pair.to_owned()),
        }
    }

    // A bare command, not wrapped: `/Add?playnow=1&file=…` on its own.
    if !changed {
        let (rebuilt, dropped) = without_playnow(query);
        if !dropped {
            return None;
        }
        return Some(format!("{path}?{rebuilt}"));
    }

    Some(format!("{path}?{}", parts.join("&")))
}

/// Strip `playnow=1` from a query, however many times it is encoded.
///
/// The wrapper's `u` is encoded once, so its separators read as `%3D` and
/// `%26`; the same query unwrapped uses `=` and `&`. Both spellings are
/// handled rather than decoding and re-encoding, which would rewrite every
/// other parameter on the way through and risk changing a file name.
/// Whether pressing this action makes the player start making sound.
///
/// Not the same question as [`appending`], which asks whether an action can be
/// turned into a queue append — only a track can. A radio station plays and
/// cannot be appended, so the two answers differ and using the append test to
/// decide what to badge left every station looking unplayable.
///
/// The player writes two commands that start audio, either bare or wrapped in
/// its own `/ui/prf?…&u=<command>`:
///
/// * `/Add?playnow=1&file=…` — a track. Without `playnow` the same command
///   only grows the queue, which is what Queue Builder Mode turns it into.
/// * `/Play?url=…` — a stream. Radio stations, Radio Paradise channels, and
///   the physical inputs, which are `Capture:` URLs and do start audio.
///
/// Everything else is false, including the wrapped commands that merely change
/// something: `/DeleteFavourite`, service switches, clearing the queue.
pub fn starts_playing(uri: &str) -> bool {
    let Some((_, query)) = uri.split_once('?') else {
        return false;
    };

    // The wrapper carries the real command percent-encoded in `u`; the other
    // parameters repeat it for the player's own bookkeeping and are not it.
    match query.split('&').find_map(|pair| pair.strip_prefix("u=")) {
        Some(encoded) => {
            let inner = percent_encoding::percent_decode_str(encoded).decode_utf8_lossy();
            is_play_command(&inner)
        }
        None => is_play_command(uri),
    }
}

/// One unwrapped command, judged.
fn is_play_command(command: &str) -> bool {
    let Some((path, query)) = command.split_once('?') else {
        return false;
    };
    match path {
        "/Play" => true,
        // `playnow` is the whole difference between playing it and queueing it.
        "/Add" => query.split('&').any(|pair| pair == "playnow=1"),
        _ => false,
    }
}

fn without_playnow(query: &str) -> (String, bool) {
    let mut out = query.to_owned();
    let mut dropped = false;

    for pattern in [
        "playnow%3D1%26",
        "%26playnow%3D1",
        "playnow=1&",
        "&playnow=1",
    ] {
        // `replace` builds the result in one pass; the loop that was here
        // removed one occurrence at a time with `replace_range`, and each
        // removal shifted the whole remaining tail down — quadratic in the
        // length of a URI the player chose.
        //
        // Repeated until it settles rather than replaced once, because
        // removing one occurrence can join its neighbours into another:
        // "playn" + "playnow=1&" + "ow=1&" becomes a match that a single pass
        // would leave behind. The string shrinks every round, so this ends.
        loop {
            let next = out.replace(pattern, "");
            if next == out {
                break;
            }
            out = next;
            dropped = true;
        }
    }
    (out, dropped)
}

/// `/ui/Configuration` — which screens this player offers.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Configuration {
    #[serde(default, rename = "item")]
    pub items: Vec<ConfigurationItem>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ConfigurationItem {
    /// `home`, `sources`, `favourites`, `search`, `queue`, `presets`…
    #[serde(rename = "@id")]
    pub id: String,
    #[serde(rename = "@URI")]
    pub uri: String,
    #[serde(rename = "@resultType")]
    pub result_type: Option<String>,
}

impl Configuration {
    pub fn uri(&self, id: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.uri.as_str())
    }
}

#[cfg(test)]
mod starts_playing_tests {
    use super::starts_playing;

    /// Every one of these came off a Powernode on BluOS 4.16.6, verbatim.
    #[test]
    fn what_makes_a_sound_and_what_does_not() {
        // A track on the Recently Played shelf.
        assert!(starts_playing(
            "/ui/prf?u=%2FAdd%3Fplaynow%3D1%26file%3D%252Fvar%252Fmnt%252Fx.flac"
        ));
        // A track inside an album, which carries the shelf's bookkeeping too.
        assert!(starts_playing(
            "/ui/prf?a=album%3DX%26playnow%3D1&t=1&u=%2FAdd%3Fplaynow%3D1%26file%3D%252Fvar%252Fx.flac"
        ));
        // A TuneIn/Airable station: /Play, and no `playnow` anywhere. This is
        // the case the append test got wrong.
        assert!(starts_playing(
            "/ui/prf?u=%2FPlay%3Furl%3DAirable%253Aradio%253Ahttps%253A%252F%252Fx.io%252Fs%252F1%26title1%3DCBC"
        ));
        // A Radio Paradise channel.
        assert!(starts_playing(
            "/ui/prf?u=%2FPlay%3Furl%3DRadioParadise%253A%252F42%253A20%252FSerenity"
        ));
        // A physical input, unwrapped.
        assert!(starts_playing("/Play?url=Capture%3Abluez%3Abluetooth"));

        // An album and the library: these open a screen.
        assert!(!starts_playing(
            "/ui/browseContext?service=LocalMusic&type=Album"
        ));
        assert!(!starts_playing("/ui/browseMenuGroup?service=LocalMusic"));
        // A wrapped command that changes something rather than playing it.
        assert!(!starts_playing(
            "/ui/prf?cgsc=1&u=%2FDeleteFavourite%3Ffn%3Dx"
        ));
        // The same track command with `playnow` dropped, which is what Queue
        // Builder Mode sends: it grows the queue and plays nothing.
        assert!(!starts_playing("/Add?where=last&file=%2Fvar%2Fx.flac"));
        // Nothing to judge.
        assert!(!starts_playing("/Play"));
        assert!(!starts_playing(""));
    }

    /// The two questions are genuinely different, and a station is the proof.
    #[test]
    fn it_is_not_the_same_question_as_appending() {
        let station = "/ui/prf?u=%2FPlay%3Furl%3DRadioParadise%253A%252F42%253A20%252FSerenity";
        assert!(starts_playing(station));
        assert!(
            super::appending(station).is_none(),
            "a stream cannot be put at the end of a queue"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Status;

    /// `/ui/Sources` from a real NAD Powernode. Exercises most of the
    /// vocabulary in one document: screen-level menu actions, two rows, a
    /// nested list, inputs, services, four of the nine action types, and the
    /// now-playing rules.
    const SOURCES: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<screen xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="screen.xsd" version="1" screenTitle="Music Sources" id="screen-sources" refreshOnPlayerChange="true">
  <refreshOnStatusChange key="sid" value="8"></refreshOnStatusChange>
  <menuAction type="add">
    <action type="webpage" URI="/redirectToCp?href=%2Fservices%3Fnoheader%3D1" title="Music Services" refreshScreen="true"></action>
  </menuAction>
  <row id="inputs" title="Inputs" scrollable="true" solidBackground="false">
    <menuAction text="Customise">
      <action type="setting" URI="/Settings?id=capture" title="Inputs" refreshScreen="true"></action>
    </menuAction>
    <input title="HDMI ARC" icon="/images/capture/ic_tv.png">
      <action type="player-link" URI="/Play?url=Capture%3Ahw%3Aimxspdif&amp;title=HDMI+ARC" haptic="true"></action>
      <nowPlayingMatch key="inputId" value="input4"></nowPlayingMatch>
    </input>
  </row>
  <row id="services" title="Music Services" solidBackground="false">
    <list>
      <service icon="/images/ui/Source/LibrarySourceIcon.png" title="Library" isLink="true">
        <action type="deep-link" URI="/music-service/LocalMusic" title="Library" service="LocalMusic"></action>
        <nowPlayingMatch key="service" value="LocalMusic"></nowPlayingMatch>
      </service>
      <service icon="/images/ui/Source/BluOSRadioSourceIcon.png" title="Radio" isLink="true">
        <action type="browse" URI="/ui/BrowseObjects?service=Airable&amp;type=BrowseMenu&amp;url=%2FRadioBrowse" resultType="screen" title="Radio" service="Airable"></action>
        <nowPlayingMatch key="service" value="Airable"></nowPlayingMatch>
      </service>
    </list>
  </row>
</screen>"##;

    /// `/ui/Favourites`, which uses the other two container shapes: a selector
    /// menu, and an info panel standing in for an empty list.
    const FAVOURITES: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<screen version="1" screenTitle="Favourites" id="screen-LocalMusic-Favourites" service="LocalMusic">
  <selectorMenu menuTitle="Select Service" replaceScreen="true">
    <item icon="/images/LibraryIcon.png?style=Default" text="Library" selected="true">
      <action type="browse" URI="/ui/Favourites?service=LocalMusic" resultType="screen" title="Favourites"></action>
    </item>
    <item icon="/Sources/images/TuneInIcon.png?style=Default" text="TuneIn">
      <action type="browse" URI="/ui/Favourites?service=TuneIn" resultType="screen" title="Favourites"></action>
    </item>
  </selectorMenu>
  <infoPanel icon="/images/ui/ic_info_favourites.png" text="You don&#39;t have any Favourites on Library" subText="You can add any content within Library as a favourite."></infoPanel>
</screen>"##;

    /// A key the status does not model used to read as "changed", which no
    /// reply could ever satisfy — so the screen refetched once a second for
    /// as long as it was open.
    /// The loop this replaced removed one occurrence at a time and shifted
    /// the tail down for each, which is quadratic in a URI the player chose.
    #[test]
    fn playnow_comes_out_whole_and_only_playnow() {
        let strip = |q: &str| without_playnow(q);

        assert_eq!(
            strip("cmd=x&playnow=1&id=3"),
            ("cmd=x&id=3".to_owned(), true)
        );
        assert_eq!(strip("cmd=x&playnow=1"), ("cmd=x".to_owned(), true));
        assert_eq!(
            strip("cmd%3Dx%26playnow%3D1%26id%3D3"),
            ("cmd%3Dx%26id%3D3".to_owned(), true)
        );

        assert_eq!(
            strip("cmd=x&id=3"),
            ("cmd=x&id=3".to_owned(), false),
            "nothing to drop leaves the query and the flag alone"
        );
        assert_eq!(
            strip("cmd=playnowish&id=3"),
            ("cmd=playnowish&id=3".to_owned(), false),
            "and it does not match inside a longer word"
        );

        // Removing one occurrence can join its neighbours into another, which
        // a single pass would leave behind.
        assert_eq!(
            strip("aplaynplaynow=1&ow=1&b"),
            ("ab".to_owned(), true),
            "a match created by an earlier removal is removed too"
        );

        // Both come out: the `playnow=1&` pass takes the first, and the
        // `&playnow=1` pass that follows takes what it left.
        assert_eq!(
            strip("a&playnow=1&playnow=1b"),
            ("ab".to_owned(), true),
            "repeated occurrences all come out"
        );
    }

    #[test]
    fn a_refresh_key_the_status_cannot_answer_is_not_a_reason_to_refetch() {
        let status = crate::Status {
            prid: Some(8),
            ..Default::default()
        };

        let with = |key: &str, value: &str| Screen {
            refresh_on: vec![(key.to_owned(), value.to_owned())],
            ..Screen::default()
        };

        assert!(!with("prid", "8").is_stale(&status), "the value matches");
        assert!(
            with("prid", "9").is_stale(&status),
            "a modelled key that moved is stale"
        );
        assert!(
            !with("nosuchkey", "1").is_stale(&status),
            "an unmodelled key cannot be satisfied, so it must not mean stale"
        );
        assert!(
            !with("sid", "3").is_stale(&status),
            "a modelled key the player left out is 'cannot tell', not 'changed'"
        );
    }

    #[test]
    fn reads_a_real_sources_screen() {
        let screen = parse(SOURCES).unwrap();

        assert_eq!(screen.heading(), Some("Music Sources"));
        assert_eq!(screen.id.as_deref(), Some("screen-sources"));
        assert!(screen.refresh_on_player_change);
        // The player says which status change should redraw this screen.
        assert_eq!(screen.refresh_on, vec![("sid".to_owned(), "8".to_owned())]);

        // The schema noise on <screen> is not mistaken for content.
        assert_eq!(screen.menu_actions.len(), 1);
        let menu = &screen.menu_actions[0];
        assert_eq!(menu.kind.as_deref(), Some("add"));
        assert_eq!(
            menu.action.as_ref().map(|a| a.kind),
            Some(ActionKind::Webpage)
        );

        assert_eq!(screen.sections.len(), 2);
        let inputs = &screen.sections[0];
        assert_eq!(inputs.kind, SectionKind::Row);
        assert_eq!(inputs.title.as_deref(), Some("Inputs"));
        assert!(inputs.scrollable);
        // A row's own menu action attaches to the row, not to the screen.
        assert_eq!(inputs.menu_actions.len(), 1);
        assert_eq!(inputs.menu_actions[0].text.as_deref(), Some("Customise"));
        assert_eq!(inputs.items.len(), 1);

        let arc = &inputs.items[0];
        assert_eq!(arc.kind, ItemKind::Input);
        assert_eq!(arc.label(), Some("HDMI ARC"));
        let action = arc.action.as_ref().unwrap();
        assert_eq!(action.kind, ActionKind::PlayerLink);
        // XML entities are resolved; percent-encoding is left alone, because
        // it is part of the URL rather than of the document.
        assert_eq!(
            action.uri.as_deref(),
            Some("/Play?url=Capture%3Ahw%3Aimxspdif&title=HDMI+ARC")
        );
    }

    #[test]
    fn a_nested_list_groups_without_starting_a_section() {
        let screen = parse(SOURCES).unwrap();
        // <row><list><service/></list></row> is two elements but one section.
        let services = &screen.sections[1];
        assert_eq!(services.kind, SectionKind::Row);
        assert_eq!(services.id.as_deref(), Some("services"));
        assert_eq!(services.items.len(), 2);
        assert!(services.items.iter().all(|i| i.kind == ItemKind::Service));
    }

    #[test]
    fn distinguishes_the_action_types() {
        let screen = parse(SOURCES).unwrap();
        let services = &screen.sections[1];

        // The library is a client-side route, not a path on the player.
        let library = services.items[0].action.as_ref().unwrap();
        assert_eq!(library.kind, ActionKind::DeepLink);
        assert_eq!(
            library.deep_link_target(),
            Some("/music-service/LocalMusic")
        );
        assert!(!library.is_navigational());

        // Radio is fetched, and the player says a screen comes back.
        let radio = services.items[1].action.as_ref().unwrap();
        assert_eq!(radio.kind, ActionKind::Browse);
        assert!(radio.is_navigational());
        assert_eq!(radio.result_type.as_deref(), Some("screen"));
        assert_eq!(radio.service.as_deref(), Some("Airable"));
        assert_eq!(radio.deep_link_target(), None);
    }

    #[test]
    fn the_player_decides_which_item_is_playing() {
        let screen = parse(SOURCES).unwrap();
        let arc = &screen.sections[0].items[0];
        assert_eq!(
            arc.now_playing_match,
            Some(("inputId".to_owned(), "input4".to_owned()))
        );

        let on_arc = Status {
            input_id: Some("input4".into()),
            ..Default::default()
        };
        assert!(arc.is_playing(&on_arc));

        let elsewhere = Status {
            input_id: Some("input5".into()),
            ..Default::default()
        };
        assert!(!arc.is_playing(&elsewhere));
        assert!(!arc.is_playing(&Status::default()));

        // Matching on a field this crate does not model reads as "no", not as
        // a crash.
        let unknown = Item {
            now_playing_match: Some(("somethingNew".to_owned(), "1".to_owned())),
            ..Default::default()
        };
        assert!(!unknown.is_playing(&on_arc));
    }

    #[test]
    fn reads_selector_menus_and_info_panels() {
        let screen = parse(FAVOURITES).unwrap();
        assert_eq!(screen.service.as_deref(), Some("LocalMusic"));
        assert_eq!(screen.sections.len(), 2);

        let selector = &screen.sections[0];
        assert_eq!(selector.kind, SectionKind::SelectorMenu);
        assert_eq!(selector.title.as_deref(), Some("Select Service"));
        assert!(selector.replace_screen);
        assert_eq!(selector.items.len(), 2);
        // These label with `text` rather than `title`.
        assert_eq!(selector.items[0].label(), Some("Library"));
        assert!(selector.items[0].selected);
        assert!(!selector.items[1].selected);

        // An info panel is an item with no action: it explains an empty list.
        let panel = &screen.sections[1].items[0];
        assert_eq!(panel.kind, ItemKind::InfoPanel);
        assert!(!panel.is_actionable());
        assert_eq!(
            panel.label(),
            Some("You don't have any Favourites on Library")
        );
    }

    /// Captured from a Powernode on BluOS 4.16.6 with the schema headers this
    /// crate sends. Trimmed to two of thirteen tracks; nothing else is edited.
    ///
    /// Two things here appear nowhere else: buttons directly under the root,
    /// and `type="confirmation"`. A client that declares no schema version is
    /// served three buttons and a plain `player-link` on Clear.
    const QUEUE: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<queue offset="0" total="13" id="723" modified="true">
  <refreshOnStatusChange key="pid" value="723"></refreshOnStatusChange>
  <button text="Save" backgroundColor="#363d3f" icon="/images/ui/btn_save_queue.png" highlight="true">
    <action type="browse" URI="/AddToPlaylistOptions?saveQueue=1" resultType="SaveQueueOptions" title="Save playlist"></action>
  </button>
  <button text="Edit" icon="/images/ui/btn_edit_queue.png">
    <action type="deep-link" URI="/edit-queue"></action>
  </button>
  <button text="Clear" icon="/images/ui/btn_clear_queue.png">
    <action type="confirmation" URI="/Clear" title="Clear queue?" refreshScreen="true" notification="Play Queue cleared"></action>
  </button>
  <button text="Queue builder mode" icon="/images/ui/btn_qbm.png">
    <action type="player-link" URI="/ui/action?CBQ=true" refreshScreen="true"></action>
  </button>
  <item image="/Artwork?service=LocalMusic&amp;artist=50+Cent" title="Many Men (Wish Death)" subTitle="50 Cent" subSubTitle="Get Rich or Die Tryin" quality="cd" duration="4:16">
    <action type="player-link" URI="/Play?id=0"></action>
    <contextMenu type="browse" URI="/ui/queueItemCM?id=0" resultType="contextMenu"></contextMenu>
    <nowPlayingMatch key="song" value="0"></nowPlayingMatch>
  </item>
  <item image="/Artwork?service=LocalMusic&amp;artist=50+Cent" title="In da Club" subTitle="50 Cent" subSubTitle="Get Rich or Die Tryin" quality="cd" duration="3:13">
    <action type="player-link" URI="/Play?id=1"></action>
    <contextMenu type="browse" URI="/ui/queueItemCM?id=1" resultType="contextMenu"></contextMenu>
    <nowPlayingMatch key="song" value="1"></nowPlayingMatch>
  </item>
</queue>"##;

    #[test]
    fn reads_the_play_queue() {
        let queue = parse(QUEUE).unwrap();

        let page = queue.queue.clone().expect("a queue document says so");
        assert_eq!(page.offset, 0);
        assert_eq!(page.total, 13);
        assert!(page.modified);
        assert!(!queue.is_context_menu);
        assert_eq!(queue.id.as_deref(), Some("723"));
        // The queue asks to be re-read when it is replaced, and says so with
        // the same mechanism every other screen uses.
        assert_eq!(queue.refresh_on, vec![("pid".to_owned(), "723".to_owned())]);

        // Buttons under the root belong to the document, not to a section.
        assert_eq!(queue.buttons.len(), 4);
        assert!(queue.sections.iter().all(|s| s.buttons.is_empty()));

        let save = &queue.buttons[0];
        assert_eq!(save.text.as_deref(), Some("Save"));
        assert!(save.highlight, "a changed queue lights its own Save button");

        let clear = &queue.buttons[2];
        assert_eq!(
            clear.action.as_ref().map(|a| a.kind),
            Some(ActionKind::Confirmation)
        );
        assert_eq!(
            clear.action.as_ref().and_then(|a| a.title.as_deref()),
            Some("Clear queue?")
        );

        // Only offered to a client that declares a new enough schema.
        assert_eq!(queue.buttons[3].text.as_deref(), Some("Queue builder mode"));

        // The tracks are ordinary items, so everything that draws a browse row
        // draws a queue row.
        let tracks: Vec<_> = queue.items().collect();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].label(), Some("Many Men (Wish Death)"));
        assert_eq!(tracks[0].duration.as_deref(), Some("4:16"));
        assert_eq!(tracks[0].quality.as_deref(), Some("cd"));
        assert!(tracks[0].context_menu.is_some());
        assert_eq!(
            tracks[1].now_playing_match,
            Some(("song".to_owned(), "1".to_owned()))
        );
    }

    /// A library's Artists page: an alphabet to jump by, then the names. Both
    /// are `<item>`, and only the second kind is content.
    const INDEXED: &str = r##"<screen version="1" service="LocalMusic">
  <list offset="0" total="448">
    <index revision="191">
      <item key="#" offset="0" length="6"></item>
      <item key="A" offset="6" length="22"></item>
    </index>
    <item title="10cm">
      <action type="browse" URI="/ui/browseContext?title=10cm" resultType="screen"></action>
    </item>
    <item title="21 Savage">
      <action type="browse" URI="/ui/browseContext?title=21+Savage" resultType="screen"></action>
    </item>
  </list>
</screen>"##;

    #[test]
    fn a_long_list_says_where_the_rest_is() {
        let screen = parse(
            r#"<screen version="1"><list offset="0" total="448">
                 <item title="10cm"></item>
               </list>
               <nextLink>/ui/browseGrouped?listContinuation=30&amp;type=Artist</nextLink>
             </screen>"#,
        )
        .unwrap();
        assert_eq!(
            screen.next.as_deref(),
            Some("/ui/browseGrouped?listContinuation=30&type=Artist")
        );
        // And a screen that is all there says nothing.
        let whole =
            parse(r#"<screen version="1"><list><item title="x"></item></list></screen>"#).unwrap();
        assert!(whole.next.is_none());
    }

    #[test]
    fn a_jump_index_is_not_content() {
        let screen = parse(INDEXED).unwrap();
        let titles: Vec<_> = screen.items().filter_map(|item| item.label()).collect();
        assert_eq!(
            titles,
            vec!["10cm", "21 Savage"],
            "the alphabet is somewhere to scroll to, not something to draw"
        );
    }

    #[test]
    fn refuses_documents_that_are_not_screens() {
        assert!(parse("").is_err());
        assert!(parse("<playlist length=\"3\"></playlist>").is_err());
        // Well-formed and a screen, just empty.
        let empty = parse("<screen screenTitle=\"Nothing\"></screen>").unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.heading(), Some("Nothing"));
    }

    #[test]
    fn unknown_elements_and_actions_are_inert_rather_than_fatal() {
        // A future firmware adding an element must not break browsing.
        let screen = parse(
            r#"<screen screenTitle="Later"><list>
                 <item title="Known"><action type="browse" URI="/ui/X"></action></item>
                 <hologram title="Newer"><action type="teleport" URI="/ui/Y"></action></hologram>
                 <item title="After"></item>
               </list></screen>"#,
        )
        .unwrap();

        let items: Vec<_> = screen.items().collect();
        assert_eq!(items.len(), 2, "the unknown element is skipped");
        assert_eq!(items[0].label(), Some("Known"));
        assert_eq!(items[1].label(), Some("After"));
        assert!(!items[1].is_actionable());

        let unknown_action = parse(
            r#"<screen><list><item title="X"><action type="teleport" URI="/ui/Y"></action></item></list></screen>"#,
        )
        .unwrap();
        let action = unknown_action
            .items()
            .next()
            .unwrap()
            .action
            .as_ref()
            .unwrap();
        assert_eq!(action.kind, ActionKind::Unknown);
        assert!(!action.is_navigational());
    }

    /// `/ui/nowPlayingCM` from a real player, trimmed to three entries. A
    /// context menu is a different root element carrying the same vocabulary.
    const CONTEXT_MENU: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<contextMenu image="/Artwork?service=LocalMusic&amp;fn=%2Fmusic%2Ftrack.flac" subTitle="The Rolling Stones • Aftermath" title="Paint It, Black" version="1">
  <item icon="/images/ui/cm_favourite_add.png" text="Favourite">
    <action type="player-link" URI="/ui/prf?cgsc=1&amp;u=%2FAddFavourite%3Ffn%3D%252Fmusic%252Ftrack.flac" refreshScreen="true" notification="Added to favourites"></action>
  </item>
  <item icon="/images/ui/cm_gotoartist.png" text="Go to artist">
    <action type="context-browse" URI="/library/v1/Artists?artist=The+Rolling+Stones&amp;service=LocalMusic" resultType="Artist" title="The Rolling Stones" service="LocalMusic"></action>
  </item>
  <item icon="/images/ui/cm_info.png" text="Info">
    <action type="browse" URI="/Info?service=LocalMusic" resultType="Info" title="Info"></action>
  </item>
</contextMenu>"##;

    /// `/ui/Search`, whose `<search>` element carries its action on itself
    /// rather than in a child.
    const SEARCH: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<screen version="1" screenTitle="Search" id="screen-LocalMusic-Search" service="LocalMusic">
  <search prompt="Search..." parameterName="q" type="browse" URI="/ui/Search?forService=LocalMusic" resultType="screen" title="Search" service="LocalMusic"></search>
</screen>"##;

    #[test]
    fn a_context_menu_is_a_screen_in_different_clothes() {
        let menu = parse(CONTEXT_MENU).unwrap();

        assert!(menu.is_context_menu);
        assert_eq!(menu.heading(), Some("Paint It, Black"));
        assert_eq!(
            menu.subtitle.as_deref(),
            Some("The Rolling Stones • Aftermath")
        );
        assert!(menu.image.is_some());

        // Items sit straight under the root with no section around them, and
        // still have to land somewhere.
        let items: Vec<_> = menu.items().collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].label(), Some("Favourite"));

        // The player supplies the wording for the confirmation.
        let favourite = items[0].action.as_ref().unwrap();
        assert_eq!(favourite.kind, ActionKind::PlayerLink);
        assert_eq!(
            favourite.notification.as_deref(),
            Some("Added to favourites")
        );
        assert!(favourite.refresh_screen);

        // "Go to artist" navigates, so it can go on the same trail as any
        // other browse.
        assert_eq!(
            items[1].action.as_ref().unwrap().kind,
            ActionKind::ContextBrowse
        );
        assert!(items[1].action.as_ref().unwrap().is_navigational());
    }

    #[test]
    fn a_nested_context_menu_is_an_action_not_a_root() {
        // Queue and browse rows carry <contextMenu> as the way to open their
        // menu. Reading one as a document root would throw away the screen.
        let screen = parse(
            r#"<screen screenTitle="Recently Played"><list>
                 <item title="A Song" subTitle="An Artist">
                   <action type="player-link" URI="/Play?id=1"></action>
                   <contextMenu type="browse" URI="/ui/queueItemCM?id=0" resultType="contextMenu"></contextMenu>
                 </item>
               </list></screen>"#,
        )
        .unwrap();

        assert!(!screen.is_context_menu);
        assert_eq!(screen.heading(), Some("Recently Played"));

        let item = screen.items().next().unwrap();
        assert_eq!(item.label(), Some("A Song"));
        assert_eq!(item.subtitle.as_deref(), Some("An Artist"));
        // The row's own action is untouched by the menu beside it.
        assert_eq!(item.action.as_ref().unwrap().kind, ActionKind::PlayerLink);

        let menu = item.context_menu.as_ref().expect("the row has a menu");
        assert_eq!(menu.kind, ActionKind::Browse);
        assert_eq!(menu.uri.as_deref(), Some("/ui/queueItemCM?id=0"));
    }

    #[test]
    fn a_search_box_names_its_own_query_parameter() {
        let screen = parse(SEARCH).unwrap();
        let search = screen.items().next().unwrap();

        assert_eq!(search.kind, ItemKind::Search);
        // The label and the placeholder are different strings.
        assert_eq!(search.label(), Some("Search"));
        assert_eq!(search.prompt(), Some("Search..."));
        assert_eq!(search.search_parameter(), Some("q"));
        assert_eq!(
            search.action.as_ref().map(|a| a.kind),
            Some(ActionKind::Browse)
        );

        // The URI already has a query, so the parameter is appended.
        assert_eq!(
            search.search_url("rolling stones").as_deref(),
            Some("/ui/Search?forService=LocalMusic&q=rolling%20stones")
        );
        // Anything that would be read as query structure is encoded.
        assert_eq!(
            search.search_url("rock & roll?").as_deref(),
            Some("/ui/Search?forService=LocalMusic&q=rock%20%26%20roll%3F")
        );

        // Nothing else is a search box.
        let sources = parse(SOURCES).unwrap();
        let input = &sources.sections[0].items[0];
        assert_eq!(input.search_parameter(), None);
        assert_eq!(input.search_url("anything"), None);
    }

    #[test]
    fn reads_the_configuration_index() {
        let config: Configuration = quick_xml::de::from_str(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<configuration>
  <item id="home" URI="/ui/Home"></item>
  <item id="queue" URI="/ui/Queue" resultType="queue"></item>
</configuration>"#,
        )
        .unwrap();

        assert_eq!(config.items.len(), 2);
        assert_eq!(config.uri("home"), Some("/ui/Home"));
        assert_eq!(config.uri("queue"), Some("/ui/Queue"));
        assert_eq!(config.uri("nonesuch"), None);
    }
    /// `/ui/queueItemCM?id=0` from a real NAD Powernode. Every row labels
    /// itself with `text` and none of them carry a `title`, which is the whole
    /// point of this fixture.
    const QUEUE_ITEM_MENU: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<contextMenu image="/Artwork?service=LocalMusic&amp;artist=21+Savage&amp;album=Gang+Shit" subTitle="21 Savage • Gang Shit" title="Gang Shit" version="1" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="screen.xsd">
  <item icon="/images/ui/cm_favourite_add.png" text="Favourite">
    <action type="player-link" URI="/ui/prf?cgsc=1&amp;u=%2FAddFavourite%3Ffn%3D%252Fvar%252Fmnt%252F10.0.0.100-mediamusic%252Fplaylist%252F21%2BSavage%2B-%2BGang%2BShit.flac%26service%3DLocalMusic" refreshScreen="true" haptic="true" notification="Added to favourites" notificationIcon="/images/ui/cm_favourite_add.png"></action>
  </item>
  <item icon="/images/ui/cm_addtoplaylist.png" text="Add to playlist…">
    <action type="browse" URI="/AddToPlaylistOptions?service=LocalMusic&amp;songid=%2Fvar%2Fmnt%2F10.0.0.100-mediamusic%2Fplaylist%2F21+Savage+-+Gang+Shit.flac" resultType="AddToPlaylistOptions" title="Add to playlist…" service="LocalMusic"></action>
  </item>
  <item icon="/images/ui/cm_info.png" text="Info">
    <action type="browse" URI="/Info?album=Gang+Shit&amp;artist=21+Savage&amp;service=LocalMusic&amp;title=Gang+Shit" resultType="Info" title="Info" service="LocalMusic"></action>
  </item>
  <item icon="/images/ui/cm_info.png" text="Technical info">
    <action type="browse" URI="/Info?category=technical&amp;filename=%2Fvar%2Fmnt%2F10.0.0.100-mediamusic%2Fplaylist%2F21+Savage+-+Gang+Shit.flac&amp;service=LocalMusic" resultType="BriefInfo" title="Technical info" service="LocalMusic"></action>
  </item>
  <item icon="/images/ui/cm_delete.png" text="Delete from play queue">
    <action type="player-link" URI="/Delete?id=0" refreshScreen="true" haptic="true" notification="Deleted &#34;Gang Shit&#34;"></action>
  </item>
</contextMenu>"##;

    #[test]
    fn a_queue_item_menu_labels_its_rows_with_text() {
        let menu = parse(QUEUE_ITEM_MENU).expect("parses");
        assert!(menu.is_context_menu);
        assert_eq!(menu.title.as_deref(), Some("Gang Shit"));

        let labels: Vec<&str> = menu.items().filter_map(|item| item.label()).collect();
        assert_eq!(
            labels,
            [
                "Favourite",
                "Add to playlist…",
                "Info",
                "Technical info",
                "Delete from play queue"
            ]
        );

        // The trap this fixture exists for: `title` is empty on every one of
        // them, so anything reading it instead of `label()` sees an empty menu
        // and reports that the player offered nothing.
        assert!(menu.items().all(|item| item.title.is_none()));
        assert!(menu.items().all(|item| item.action.is_some()));
    }
    /// `/ui/Sources` from a real NAD Powernode. Its inputs and its music
    /// services sit in one flat list of items and are told apart only by the
    /// action each carries.
    const SOURCES_SCREEN: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<screen xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="screen.xsd" version="1" screenTitle="Music Sources" id="screen-sources" refreshOnPlayerChange="true">
  <refreshOnStatusChange key="sid" value="17"></refreshOnStatusChange>
  <menuAction type="add">
    <action type="webpage" URI="/redirectToCp?href=%2Fservices%3Fnoheader%3D1%26schemaVersion%3D35" title="Music Services" refreshScreen="true"></action>
  </menuAction>
  <row id="inputs" title="Inputs" scrollable="true" solidBackground="false">
    <menuAction text="Customise">
      <action type="setting" URI="/Settings?id=capture" title="Inputs" refreshScreen="true"></action>
    </menuAction>
    <input title="Bluetooth" icon="/images/BluetoothIcon.png">
      <action type="player-link" URI="/Play?url=Capture%3Abluez%3Abluetooth&amp;title=Bluetooth&amp;image=%2Fimages%2FBluetoothIcon.png" haptic="true"></action>
      <nowPlayingMatch key="inputId" value="input5"></nowPlayingMatch>
    </input>
    <input title="HDMI ARC" icon="/images/capture/ic_tv.png">
      <action type="player-link" URI="/Play?url=Capture%3Ahw%3Aimxspdif%2C0%2F1%2F25%2F2%3Fid%3Dinput4&amp;title=HDMI+ARC&amp;image=%2Fimages%2Fcapture%2Fic_tv.png" haptic="true"></action>
      <nowPlayingMatch key="inputId" value="input4"></nowPlayingMatch>
    </input>
  </row>
  <row id="services" title="Music Services" solidBackground="false">
    <menuAction text="Manage">
      <action type="webpage" URI="/redirectToCp?href=%2Fservices%3Fnoheader%3D1%26schemaVersion%3D35" title="Music Services" refreshScreen="true"></action>
    </menuAction>
    <list>
      <service icon="/images/ui/Source/LibrarySourceIcon.png" title="Library" isLink="true">
        <action type="browse" URI="/ui/browseMenuGroup?service=LocalMusic" resultType="screen" title="Library" service="LocalMusic"></action>
        <nowPlayingMatch key="service" value="LocalMusic"></nowPlayingMatch>
      </service>
      <service icon="/images/ui/Source/BluOSRadioSourceIcon.png" title="Radio" isLink="true">
        <action type="browse" URI="/ui/browseMenuGroup?service=Airable" resultType="screen" title="Radio" service="Airable"></action>
        <nowPlayingMatch key="service" value="Airable"></nowPlayingMatch>
      </service>
      <service icon="/images/ui/Source/RadioParadiseSourceIcon.png" title="Radio Paradise" isLink="true">
        <action type="browse" URI="/ui/browseMenuGroup?service=RadioParadise" resultType="screen" title="Radio Paradise" service="RadioParadise"></action>
        <nowPlayingMatch key="service" value="RadioParadise"></nowPlayingMatch>
      </service>
      <service icon="/images/ui/Source/TuneInSourceIcon.png" title="TuneIn" isLink="true">
        <action type="browse" URI="/ui/browseMenuGroup?service=TuneIn" resultType="screen" title="TuneIn" service="TuneIn"></action>
        <nowPlayingMatch key="service" value="TuneIn"></nowPlayingMatch>
      </service>
    </list>
  </row>
</screen>"##;

    #[test]
    fn an_input_is_a_player_link_that_plays_and_a_service_is_a_browse() {
        let screen = parse(SOURCES_SCREEN).expect("parses");

        let plays: Vec<&str> = screen
            .items()
            .filter(|item| {
                item.action.as_ref().is_some_and(|a| {
                    a.kind == ActionKind::PlayerLink
                        && a.uri.as_deref().is_some_and(|uri| uri.starts_with("/Play"))
                })
            })
            .filter_map(|item| item.label())
            .collect();
        assert_eq!(plays, ["Bluetooth", "HDMI ARC"]);

        // The trap: every one of those carries `action`, not `play_action`, so
        // "has no browse action" identifies none of them.
        assert!(screen.items().all(|item| item.play_action.is_none()));

        let browses: Vec<&str> = screen
            .items()
            .filter(|item| {
                item.action
                    .as_ref()
                    .is_some_and(|a| a.kind == ActionKind::Browse)
            })
            .filter_map(|item| item.label())
            .collect();
        assert_eq!(browses, ["Library", "Radio", "Radio Paradise", "TuneIn"]);
    }
    /// A favourite track's action, exactly as a Powernode writes it.
    const PLAY_ACTION: &str = "/ui/prf?a=category%3DFAVOURITES%26cursor%3Dlast%26listindex%3D0%26nextlist%3D1%26playnow%3D1%26service%3DLocalMusic%26where%3Dlast&t=1&u=%2FAdd%3Fplaynow%3D1%26file%3D%252Fvar%252Fmnt%252Fx.flac";

    #[test]
    fn playing_becomes_appending() {
        let appended = appending(PLAY_ACTION).expect("this one plays");
        // The command inside no longer says to play it now...
        assert!(!appended.contains("u=%2FAdd%3Fplaynow%3D1"));
        assert!(appended.contains("u=%2FAdd%3Ffile%3D%252Fvar%252Fmnt%252Fx.flac"));
        // ...and nothing else about it moved.
        assert!(appended.starts_with("/ui/prf?a=category%3DFAVOURITES"));
        assert!(appended.contains("&t=1&"));
        assert!(appended.contains("where%3Dlast"));
    }

    #[test]
    fn a_bare_add_is_rewritten_too() {
        let appended = appending("/Add?playnow=1&file=%2Fx.flac").expect("plays");
        assert_eq!(appended, "/Add?file=%2Fx.flac");
    }

    #[test]
    fn an_action_that_does_not_play_is_left_alone() {
        // Browsing, favouriting, switching service: none of these play, and a
        // caller needs to know so it can run them unchanged.
        assert_eq!(appending("/ui/BrowseObjects?service=LocalMusic"), None);
        assert_eq!(appending("/ui/action?CfavouritesService=TuneIn"), None);
        assert_eq!(appending("/Clear"), None);
        assert_eq!(appending("/Play"), None);
    }

    #[test]
    fn a_file_name_that_mentions_playing_is_not_mangled() {
        // The pattern is anchored to the parameter, not to the text: a track
        // called "playnow=1.flac" must survive.
        let uri = "/Add?playnow=1&file=%2Fmusic%2Fplaynow%3D1.flac";
        assert_eq!(
            appending(uri).as_deref(),
            Some("/Add?file=%2Fmusic%2Fplaynow%3D1.flac")
        );
    }
    #[test]
    fn a_source_may_carry_its_name_only_on_its_action() {
        // Home's Most Used shelf, exactly as a Powernode writes it: the inputs
        // and the library are titled, the two radio services are not, and
        // their names live on the action instead. Anything choosing an icon
        // from the label alone sees nothing for those two.
        let xml = r##"<screen version="1" id="screen-home">
  <row id="mostUsed" title="Most Used">
    <source icon="/images/LibraryIcon.png" title="Library">
      <action type="browse" URI="/ui/browseMenuGroup?service=LocalMusic" title="Library"></action>
    </source>
    <source icon="/images/ui/Source/RadioParadiseLogo.png">
      <action type="browse" URI="/ui/browseMenuGroup?service=RadioParadise" title="Radio Paradise"></action>
    </source>
    <source icon="/images/ui/Source/TuneInLogo.png">
      <action type="browse" URI="/ui/browseMenuGroup?service=TuneIn" title="TuneIn"></action>
    </source>
  </row>
</screen>"##;
        let screen = parse(xml).expect("parses");
        let named: Vec<&str> = screen
            .items()
            .map(|item| {
                item.label().unwrap_or_else(|| {
                    item.action
                        .as_ref()
                        .and_then(|a| a.title.as_deref())
                        .unwrap_or_default()
                })
            })
            .collect();
        assert_eq!(named, ["Library", "Radio Paradise", "TuneIn"]);

        // The half that makes the fallback necessary.
        let labels: Vec<Option<&str>> = screen.items().map(|item| item.label()).collect();
        assert_eq!(labels, [Some("Library"), None, None]);
    }

    /// A context menu is items directly under its root, with no `<row>` to
    /// group them, so the parser opens a section of its own to hold them. That
    /// section has no closing tag to end it, and anything at the root that
    /// follows the items used to be read as part of it.
    #[test]
    fn what_follows_a_rows_worth_of_loose_items_still_belongs_to_the_document() {
        let doc = r#"<contextMenu title="Now playing">
            <item title="Technical info"><action type="browse" URI="/info"/></item>
            <item title="Add to playlist"><action type="browse" URI="/add"/></item>
            <menuAction title="Edit" id="edit"/>
            <button title="Clear" id="clear"/>
        </contextMenu>"#;
        let screen = parse(doc).expect("parses");

        assert_eq!(screen.sections.len(), 1, "{:?}", screen.sections);
        assert_eq!(screen.sections[0].items.len(), 2);
        assert!(
            screen.sections[0].menu_actions.is_empty(),
            "the menu action was filed under the items"
        );
        assert!(
            screen.sections[0].buttons.is_empty(),
            "the button was filed under the items"
        );
        assert_eq!(screen.menu_actions.len(), 1);
        assert_eq!(screen.buttons.len(), 1);
    }

    /// The same two elements inside a real `<row>` still belong to it.
    #[test]
    fn a_row_that_was_written_down_keeps_what_is_inside_it() {
        let doc = r#"<screen screenTitle="Library">
            <row title="Albums">
                <item title="One"/>
                <menuAction title="Sort" id="sort"/>
                <button title="Play all" id="play"/>
            </row>
        </screen>"#;
        let screen = parse(doc).expect("parses");

        assert_eq!(screen.sections.len(), 1);
        assert_eq!(screen.sections[0].menu_actions.len(), 1);
        assert_eq!(screen.sections[0].buttons.len(), 1);
        assert!(screen.menu_actions.is_empty());
        assert!(screen.buttons.is_empty());
    }
}
