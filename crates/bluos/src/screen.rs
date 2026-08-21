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

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::error::{Error, Result};

/// One screen, as the player describes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Screen {
    /// Heading for the screen. Some documents use `navigationTitle` instead,
    /// which [`Screen::heading`] folds together.
    pub title: Option<String>,
    pub navigation_title: Option<String>,
    pub navigation_icon: Option<String>,
    pub id: Option<String>,
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
        self.refresh_on
            .iter()
            .any(|(key, value)| status.field(key).as_deref() != Some(value.as_str()))
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
    /// On a selector menu, whether picking replaces the current screen.
    pub replace_screen: bool,
    pub menu_actions: Vec<MenuAction>,
    pub buttons: Vec<Button>,
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

    /// Whether `status` says this is the item currently playing.
    pub fn is_playing(&self, status: &crate::Status) -> bool {
        let Some((key, value)) = &self.now_playing_match else {
            return false;
        };
        status.field(key).is_some_and(|actual| &actual == value)
    }
}

/// What an action does. The nine the player emits, plus a catch-all so an
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
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Button {
    pub text: Option<String>,
    pub icon: Option<String>,
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MenuAction {
    pub text: Option<String>,
    /// `settings`, `add`, and so on — a hint at which icon to use.
    pub kind: Option<String>,
    pub action: Option<Action>,
}

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
    /// Anything unrecognised, so its end tag pops the right thing.
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
                let name = local_name(e.name().as_ref());
                start(
                    &name,
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
                let name = local_name(e.name().as_ref());
                start(
                    &name,
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
                    &name,
                    &mut stack,
                    &mut screen,
                    &mut section,
                    &mut item,
                    &mut menu_action,
                    &mut button,
                );
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                end(
                    &name,
                    &mut stack,
                    &mut screen,
                    &mut section,
                    &mut item,
                    &mut menu_action,
                    &mut button,
                );
            }
            Ok(_) => {}
            Err(e) => return Err(Error::Screen(format!("malformed XML: {e}"))),
        }
    }

    if !seen_screen {
        return Err(Error::Screen("no <screen> element".into()));
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
        "screen" => {
            *seen_screen = true;
            screen.title = a.remove("screenTitle");
            screen.navigation_title = a.remove("navigationTitle");
            screen.navigation_icon = a.remove("navigationIcon");
            screen.id = a.remove("id");
            screen.service = a.remove("service");
            screen.refresh_on_player_change = flag(a.remove("refreshOnPlayerChange"));
            stack.push(Ctx::Screen);
        }

        "refreshOnStatusChange" => {
            if let (Some(key), Some(value)) = (a.remove("key"), a.remove("value")) {
                screen.refresh_on.push((key, value));
            }
            stack.push(Ctx::Other);
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
            if let Some((_, kind)) = ITEM_ELEMENTS.iter().find(|(n, _)| *n == name) {
                *item = Some(Item {
                    kind: *kind,
                    title: a.remove("title"),
                    text: a.remove("text"),
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
                match section {
                    Some(section) => section.menu_actions.push(done),
                    None => screen.menu_actions.push(done),
                }
            }
        }
        Some(Ctx::Button) => {
            if let Some(done) = button.take() {
                match (item.as_mut(), section.as_mut()) {
                    (Some(item), _) => item.buttons.push(done),
                    (None, Some(section)) => section.buttons.push(done),
                    (None, None) => {}
                }
            }
        }
        _ => {}
    }
    let _ = name;
}

fn action(element: &str, mut a: BTreeMap<String, String>) -> Action {
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
        extra: a,
    }
}

/// Element names arrive namespaced in these documents (`xsi:…`); only the local
/// part matters here. Takes the raw bytes so it serves start and end tags
/// alike.
fn local_name(raw: &[u8]) -> String {
    let full = String::from_utf8_lossy(raw).into_owned();
    match full.split_once(':') {
        Some((_, local)) => local.to_owned(),
        None => full,
    }
}

fn attributes(e: &BytesStart<'_>) -> BTreeMap<String, String> {
    e.attributes()
        .flatten()
        .filter_map(|attr| {
            let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
            // Drop the schema noise the player puts on every <screen>.
            if key.starts_with("xmlns") || key.starts_with("xsi:") {
                return None;
            }
            // These documents all declare XML 1.0, and quick-xml wants to be
            // told which rules to normalise entities under.
            let value = attr
                .normalized_value(XmlVersion::Explicit1_0)
                .ok()?
                .into_owned();
            Some((key, value))
        })
        .collect()
}

fn flag(value: Option<String>) -> bool {
    matches!(value.as_deref(), Some("true" | "1"))
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
}
