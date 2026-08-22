//! The player's settings.
//!
//! A fourth server-driven grammar, and the one that is easiest to mistake for
//! something else: `/Settings` on the control port answers **301** to port
//! 11001, and what comes back there is not a web page but a description of the
//! settings themselves —
//!
//! ```xml
//! <settings schemaVersion="28">
//!   <setting id="alarms" displayName="Alarms" class="alarms" count="0"/>
//!   <menuGroup id="library" displayName="Music library">
//!     <setting id="reindex" displayName="Reindex music collection"
//!              url="/Reindex" class="button"/>
//! ```
//!
//! So settings can be drawn natively, the same way the browse screens are.
//! The exception is the handful that carry a `<webview>` child: WiFi setup and
//! network-share configuration really are web pages, served from port 80, and
//! those are the player saying "this one is not mine to describe".
//!
//! The vocabulary is six elements and eight classes. `<value>` does double
//! duty — bounds when it carries `min`/`max`, an option when it carries
//! `name`/`displayName` — which is the one thing here that has to be read
//! carefully.

use std::collections::BTreeMap;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::error::{Error, Result};
use crate::xml::{attributes, flag, local_name};

/// One settings page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub page_id: Option<String>,
    pub schema_version: Option<u32>,
    /// Where this document came from, so a write can be addressed relative to
    /// it. The settings service is not on the control port.
    pub base: String,
    pub entries: Vec<Entry>,
}

impl Settings {
    /// Every setting on the page, groups flattened away.
    pub fn settings(&self) -> Vec<&Setting> {
        fn walk<'a>(entries: &'a [Entry], out: &mut Vec<&'a Setting>) {
            for entry in entries {
                match entry {
                    Entry::Setting(setting) => out.push(setting.as_ref()),
                    Entry::Group(group) => walk(&group.entries, out),
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.entries, &mut out);
        out
    }

    /// The value a setting currently holds, by name. Used to resolve
    /// [`Setting::depends_on`].
    pub fn value_of(&self, name: &str) -> Option<&str> {
        self.settings()
            .into_iter()
            .find(|s| s.name.as_deref() == Some(name))
            .and_then(|s| s.value.as_deref())
    }

    /// Whether a setting's precondition is met, and it should be shown.
    pub fn is_available(&self, setting: &Setting) -> bool {
        match &setting.depends_on {
            Some((name, wanted)) => self.value_of(name) == Some(wanted.as_str()),
            None => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Group(Group),
    /// Boxed because a `Setting` carries every attribute the grammar allows
    /// and dwarfs a `Group`, which would make every entry in a list that size.
    Setting(Box<Setting>),
}

/// A page of settings, or a heading with settings under it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Group {
    pub id: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    /// Where writes for the settings inside go, unless one names its own.
    pub url: Option<String>,
    /// Whether the group offers a "reset to defaults".
    pub defaults: bool,
    pub entries: Vec<Entry>,
}

impl Group {
    /// A group with nothing inside it is a link to its own page rather than a
    /// heading: the top-level document lists `player`, `audio` and so on with
    /// no children, and each is fetched by id.
    pub fn is_page_link(&self) -> bool {
        self.entries.is_empty()
    }
}

/// How a setting wants to be presented, from its `class`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Kind {
    /// On or off. May still carry two `<value>` options naming what those
    /// mean to the player — the subwoofer setting is `default`/`withsub`.
    Boolean,
    /// One of several named options.
    List,
    /// A number between bounds.
    Range,
    /// Two numbers between bounds, written as `low,high`.
    DualRange,
    /// Does something when pressed; has no value.
    Button,
    /// Free text, usually with a pattern to satisfy.
    Text,
    /// The alarm editor, which is a screen of its own.
    Alarms,
    /// The sleep timer.
    Sleep,
    /// Something a later firmware added.
    #[default]
    Other,
}

impl Kind {
    fn parse(class: Option<&str>) -> Self {
        match class {
            Some("boolean") => Self::Boolean,
            Some("list") => Self::List,
            Some("range") => Self::Range,
            Some("dual-range") => Self::DualRange,
            Some("button") => Self::Button,
            Some("text") => Self::Text,
            Some("alarms") => Self::Alarms,
            Some("sleep") => Self::Sleep,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Setting {
    pub id: String,
    /// The parameter name a write uses. Absent on settings that are only
    /// links, such as the alarm editor.
    pub name: Option<String>,
    pub display_name: Option<String>,
    /// A one-line summary of the current state, already in words.
    pub description: Option<String>,
    /// The longer explanation the official app shows underneath.
    pub explanation: Option<String>,
    pub icon: Option<String>,
    /// Where a write goes. Relative to [`Settings::base`].
    pub url: Option<String>,
    pub help_url: Option<String>,
    pub kind: Kind,
    pub value: Option<String>,
    /// How many of a thing there are — alarms, for instance.
    pub count: Option<u32>,
    pub enabled: Option<bool>,
    /// The player refusing the write, with `explanation` saying why. Not the
    /// same as disabled-because-a-precondition-is-unmet.
    pub disabled: bool,
    pub hide_if_disabled: bool,
    /// Re-read the page after writing this one.
    pub refresh: bool,
    pub style: Option<String>,
    pub pattern: Option<String>,
    pub pattern_error: Option<String>,
    pub options: Vec<Choice>,
    pub range: Option<Bounds>,
    /// A page served by the player over HTTP that it will not describe. Open
    /// it in a browser; there is nothing to render.
    pub webview: Option<String>,
    /// `(name, value)`: only meaningful while that setting holds that value.
    pub depends_on: Option<(String, String)>,
    pub extra: BTreeMap<String, String>,
}

impl Setting {
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }

    /// Whether a boolean is currently on.
    ///
    /// The player writes `ON`/`OFF` for most, but a boolean with options of
    /// its own is on when it holds the second one — the subwoofer switch is
    /// `default` off and `withsub` on.
    pub fn is_on(&self) -> bool {
        match self.value.as_deref() {
            Some("ON" | "on" | "1" | "true") => true,
            Some(value) => self.options.len() == 2 && self.options[1].name == value,
            None => false,
        }
    }

    /// What to write to turn a boolean the other way.
    pub fn toggled(&self) -> Option<String> {
        if self.kind != Kind::Boolean {
            return None;
        }
        Some(if self.options.len() == 2 {
            let (on, off) = (&self.options[1].name, &self.options[0].name);
            if self.is_on() {
                off.clone()
            } else {
                on.clone()
            }
        } else if self.is_on() {
            "OFF".to_owned()
        } else {
            "ON".to_owned()
        })
    }

    /// The current value as a number, for the sliders.
    pub fn number(&self) -> Option<f32> {
        self.value.as_deref()?.parse().ok()
    }
}

/// One option of a list, or of a boolean that names its two states.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Choice {
    pub name: String,
    pub display_name: Option<String>,
}

impl Choice {
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

/// The bounds of a range, from a `<value>` that carries `min` rather than
/// `name`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bounds {
    pub min: f32,
    pub max: f32,
    pub step: Option<f32>,
    /// The narrowest a dual range may be.
    pub min_range: Option<f32>,
    pub units: Option<String>,
}

/// Read a `<settings>` document.
pub fn parse(xml: &str, base: &str) -> Result<Settings> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut settings = Settings {
        base: base.to_owned(),
        ..Default::default()
    };
    // Groups nest, so the stack holds the ones still open; entries are pushed
    // onto whichever is innermost, or onto the document when there is none.
    let mut groups: Vec<Group> = Vec::new();
    let mut setting: Option<Setting> = None;
    let mut seen_root = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| Error::Screen(format!("malformed settings XML: {e}")))?;

        match event {
            Event::Eof => break,
            // A self-closing element gets no End of its own, so it is opened
            // and closed here. The player writes `<setting></setting>` today,
            // and nothing should depend on it continuing to.
            Event::Empty(e) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                open(
                    name,
                    &e,
                    &mut settings,
                    &mut groups,
                    &mut setting,
                    &mut seen_root,
                );
                close(name, &mut settings, &mut groups, &mut setting);
            }
            Event::Start(e) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                open(
                    name,
                    &e,
                    &mut settings,
                    &mut groups,
                    &mut setting,
                    &mut seen_root,
                );
            }
            Event::End(e) => {
                let qname = e.name();
                close(
                    local_name(qname.as_ref()),
                    &mut settings,
                    &mut groups,
                    &mut setting,
                );
            }
            _ => {}
        }
    }

    if !seen_root {
        return Err(Error::Screen("no <settings> element".into()));
    }
    Ok(settings)
}

fn open(
    name: &str,
    e: &BytesStart<'_>,
    settings: &mut Settings,
    groups: &mut Vec<Group>,
    setting: &mut Option<Setting>,
    seen_root: &mut bool,
) {
    let mut a = attributes(e);

    match name {
        "settings" => {
            *seen_root = true;
            settings.page_id = a.remove("pageId");
            settings.schema_version = a.remove("schemaVersion").and_then(|v| v.parse().ok());
        }
        "menuGroup" => groups.push(Group {
            id: a.remove("id").unwrap_or_default(),
            display_name: a.remove("displayName"),
            description: a.remove("description"),
            icon: a.remove("icon"),
            url: a.remove("url"),
            defaults: flag(a.remove("defaults")),
            entries: Vec::new(),
        }),
        "setting" => {
            *setting = Some(Setting {
                kind: Kind::parse(a.remove("class").as_deref()),
                id: a.remove("id").unwrap_or_default(),
                name: a.remove("name"),
                display_name: a.remove("displayName"),
                description: a.remove("description"),
                explanation: a.remove("explanation"),
                icon: a.remove("icon"),
                url: a.remove("url"),
                help_url: a.remove("helpUrl"),
                value: a.remove("value"),
                count: a.remove("count").and_then(|v| v.parse().ok()),
                enabled: a.remove("enabled").map(|v| v != "0" && v != "false"),
                disabled: flag(a.remove("disable")),
                hide_if_disabled: flag(a.remove("hideIfDisabled")),
                refresh: flag(a.remove("refresh")),
                style: a.remove("style"),
                pattern: a.remove("pattern"),
                pattern_error: a.remove("patternError"),
                extra: a,
                ..Default::default()
            })
        }
        // Bounds when it carries `min`, an option when it carries `name`. The
        // same element either way, which is the one thing in this grammar that
        // has to be read carefully.
        "value" => {
            if let Some(setting) = setting.as_mut() {
                if let Some(min) = a.remove("min") {
                    setting.range = Some(Bounds {
                        min: min.parse().unwrap_or(0.0),
                        max: a.remove("max").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                        step: a.remove("step").and_then(|v| v.parse().ok()),
                        min_range: a.remove("minRange").and_then(|v| v.parse().ok()),
                        units: a.remove("units"),
                    });
                } else if let Some(name) = a.remove("name") {
                    setting.options.push(Choice {
                        name,
                        display_name: a.remove("displayName"),
                    });
                }
            }
        }
        "webview" => {
            if let Some(setting) = setting.as_mut() {
                setting.webview = a.remove("url");
            }
        }
        "dependsOn" => {
            if let (Some(setting), Some(name), Some(value)) =
                (setting.as_mut(), a.remove("name"), a.remove("value"))
            {
                setting.depends_on = Some((name, value));
            }
        }
        _ => {}
    }
}

fn close(
    name: &str,
    settings: &mut Settings,
    groups: &mut Vec<Group>,
    setting: &mut Option<Setting>,
) {
    match name {
        "setting" => {
            if let Some(done) = setting.take() {
                push(settings, groups, Entry::Setting(Box::new(done)));
            }
        }
        "menuGroup" => {
            if let Some(done) = groups.pop() {
                push(settings, groups, Entry::Group(done));
            }
        }
        _ => {}
    }
}

/// Into the innermost open group, or the document if none is open.
fn push(settings: &mut Settings, groups: &mut [Group], entry: Entry) {
    match groups.last_mut() {
        Some(group) => group.entries.push(entry),
        None => settings.entries.push(entry),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Audio page from a real NAD Powernode. Between them these settings
    /// use every class and both meanings of `<value>`.
    const AUDIO: &str = r##"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<settings pageId="audio" schemaVersion="28">
	<menuGroup id="audio" defaults="true" displayName="Audio" icon="/images/settings/ic_audio.png" url="/audiomodes">
		<setting id="eq-switch" name="eq-switch" displayName="Tone Controls" url="/alsa_setting" class="boolean" value="OFF" hideIfDisabled="true"></setting>
		<setting id="eq-bass" name="eq-bass" displayName="Bass" url="/alsa_setting" class="range" value="0" hideIfDisabled="true">
			<value min="-6" max="6" step="0.5" units="dB"></value>
			<dependsOn name="eq-switch" value="ON"></dependsOn>
		</setting>
		<setting id="subwoofer" name="subwoofer" displayName="Subwoofer" url="/audiomodes" class="boolean" value="default">
			<value displayName="Off" name="default"></value>
			<value displayName="On" name="withsub"></value>
		</setting>
		<setting id="replayGainMode" name="replayGainMode" displayName="Replay-gain" url="/audiomodes" class="list" value="none" description="Disabled">
			<value displayName="Disabled" name="none"></value>
			<value displayName="Track gain" name="track"></value>
		</setting>
		<setting id="volumeLimits" name="volumeLimits" displayName="Volume limits (dB)" class="dual-range" value="-65,0">
			<value min="-90" max="0" minRange="30" units="dB"></value>
		</setting>
		<setting id="reset" name="reset" displayName="Reset All" url="/alsa_setting" class="button" style="center"></setting>
	</menuGroup>
</settings>"##;

    /// The top level, whose groups are links to their own pages, plus a
    /// setting the player will not describe.
    const ROOT: &str = r##"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<settings schemaVersion="28">
	<setting id="alarms" displayName="Alarms" class="alarms" count="0" enabled="0"></setting>
	<menuGroup id="player" displayName="Player" icon="/images/players/N225_nt.png"></menuGroup>
	<menuGroup id="library" displayName="Music library">
		<setting id="sharecfg" displayName="Network shares" description="//10.0.0.100/media/music" helpUrl="https://support.bluos.net/hc/en-us/articles/360000469948">
			<webview url="http://10.0.0.155:80/sharecfg?noheader=1"></webview>
		</setting>
		<setting id="reindex" name="reindex" displayName="Reindex music collection" url="/Reindex" class="button"></setting>
	</menuGroup>
</settings>"##;

    fn audio() -> Settings {
        parse(AUDIO, "http://192.0.2.155:11001").unwrap()
    }

    #[test]
    fn reads_a_real_settings_page() {
        let page = audio();
        assert_eq!(page.page_id.as_deref(), Some("audio"));
        assert_eq!(page.schema_version, Some(28));
        assert_eq!(page.base, "http://192.0.2.155:11001");

        // One group, with everything inside it.
        assert_eq!(page.entries.len(), 1);
        let Entry::Group(group) = &page.entries[0] else {
            panic!("expected a group")
        };
        assert_eq!(group.display_name.as_deref(), Some("Audio"));
        assert!(group.defaults);
        assert!(!group.is_page_link());
        assert_eq!(page.settings().len(), 6);
    }

    #[test]
    fn value_means_bounds_or_an_option_depending_on_what_it_carries() {
        let page = audio();
        let by = |id: &str| {
            page.settings()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("no setting {id}"))
                .clone()
        };

        // With min/max it is the bounds of a range, and no option.
        let bass = by("eq-bass");
        assert_eq!(bass.kind, Kind::Range);
        assert!(bass.options.is_empty());
        let bounds = bass.range.as_ref().expect("a range has bounds");
        assert_eq!(bounds.min, -6.0);
        assert_eq!(bounds.max, 6.0);
        assert_eq!(bounds.step, Some(0.5));
        assert_eq!(bounds.units.as_deref(), Some("dB"));
        assert_eq!(bass.number(), Some(0.0));

        // With name/displayName it is an option, and there are no bounds.
        let gain = by("replayGainMode");
        assert_eq!(gain.kind, Kind::List);
        assert!(gain.range.is_none());
        assert_eq!(gain.options.len(), 2);
        assert_eq!(gain.options[0].label(), "Disabled");
        assert_eq!(gain.options[1].name, "track");

        // A dual range keeps both numbers in one string.
        let limits = by("volumeLimits");
        assert_eq!(limits.kind, Kind::DualRange);
        assert_eq!(limits.value.as_deref(), Some("-65,0"));
        assert_eq!(limits.range.unwrap().min_range, Some(30.0));
    }

    #[test]
    fn a_boolean_may_name_its_own_two_states() {
        let page = audio();
        let by = |id: &str| {
            page.settings()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap()
                .clone()
        };

        // The ordinary kind, written ON and OFF.
        let tone = by("eq-switch");
        assert!(!tone.is_on());
        assert_eq!(tone.toggled().as_deref(), Some("ON"));

        // The subwoofer switch is off at "default" and on at "withsub", so
        // toggling it has to write one of those and not "ON".
        let sub = by("subwoofer");
        assert_eq!(sub.kind, Kind::Boolean);
        assert!(!sub.is_on());
        assert_eq!(sub.toggled().as_deref(), Some("withsub"));

        // ...and back again from the on state.
        let on = Setting {
            value: Some("withsub".into()),
            ..sub.clone()
        };
        assert!(on.is_on());
        assert_eq!(on.toggled().as_deref(), Some("default"));

        // Only booleans toggle.
        assert_eq!(by("eq-bass").toggled(), None);
    }

    #[test]
    fn a_setting_can_depend_on_another() {
        let page = audio();
        let bass = page
            .settings()
            .into_iter()
            .find(|s| s.id == "eq-bass")
            .unwrap()
            .clone();

        assert_eq!(
            bass.depends_on,
            Some(("eq-switch".to_owned(), "ON".to_owned()))
        );
        // Tone controls are off, so the bass slider is not live.
        assert!(!page.is_available(&bass));
        assert_eq!(page.value_of("eq-switch"), Some("OFF"));

        // Something with no precondition always is.
        let reset = page
            .settings()
            .into_iter()
            .find(|s| s.id == "reset")
            .unwrap();
        assert!(page.is_available(reset));
    }

    #[test]
    fn groups_without_children_are_links_to_their_own_page() {
        let page = parse(ROOT, "http://192.0.2.155:11001").unwrap();

        let Entry::Group(player) = &page.entries[1] else {
            panic!("expected a group")
        };
        assert!(player.is_page_link(), "fetched by id rather than expanded");

        let Entry::Group(library) = &page.entries[2] else {
            panic!("expected a group")
        };
        assert!(!library.is_page_link());
        assert_eq!(library.entries.len(), 2);
    }

    #[test]
    fn a_webview_setting_is_the_player_declining_to_describe_it() {
        let page = parse(ROOT, "http://192.0.2.155:11001").unwrap();
        let shares = page
            .settings()
            .into_iter()
            .find(|s| s.id == "sharecfg")
            .unwrap();

        // No class, so nothing to draw — but a page to open, on port 80.
        assert_eq!(shares.kind, Kind::Other);
        assert_eq!(
            shares.webview.as_deref(),
            Some("http://10.0.0.155:80/sharecfg?noheader=1")
        );
        assert!(shares.help_url.is_some());

        // The alarm editor is a screen of its own, and says how many there are.
        let alarms = page
            .settings()
            .into_iter()
            .find(|s| s.id == "alarms")
            .unwrap();
        assert_eq!(alarms.kind, Kind::Alarms);
        assert_eq!(alarms.count, Some(0));
        assert_eq!(alarms.enabled, Some(false));
    }

    #[test]
    fn refuses_documents_that_are_not_settings() {
        assert!(parse("", "http://x").is_err());
        assert!(parse("<screen screenTitle=\"Home\"></screen>", "http://x").is_err());
    }

    #[test]
    fn a_self_closing_setting_is_not_lost() {
        // The player writes <setting></setting> today. Nothing should depend
        // on that continuing to be true.
        let page = parse(
            r#"<settings><setting id="a" class="button" displayName="Go"/></settings>"#,
            "http://x",
        )
        .unwrap();
        assert_eq!(page.settings().len(), 1);
        assert_eq!(page.settings()[0].label(), "Go");
    }
}
