//! The player's presets: the numbered slots its hardware buttons play.
//!
//! `/Presets` lists them and is the only way to know what is already there.
//! Every other preset route is handed its subject by the player — a station's
//! menu carries what to save, a preset's menu carries what it holds — so this
//! exists for the one question none of them answer: **is this already a
//! preset?** Without it the same station can be saved over and over, each
//! taking another slot, which is what happens when nothing checks.
//!
//! ```xml
//! <presets prid="52">
//!   <preset id="1" name="RockIt!" url="RadioParadise:/2:20/RockIt%21"
//!           image="https://img.radioparadise.com/…"></preset>
//! </presets>
//! ```
//!
//! Read off a Powernode running 4.16.22. `prid` identifies the list as it
//! stands and changes on **every** edit — a save, a delete, a rename, a
//! reorder — which is what the Presets screen's `refreshOnStatusChange` is
//! watching.
//!
//! Slots are numbered from one and are not necessarily contiguous: deleting a
//! preset leaves its slot empty rather than closing the gap.

use serde::Deserialize;

/// The whole list, as `/Presets` gives it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Presets {
    /// Which version of the list this is. Changes on every edit.
    #[serde(rename = "@prid")]
    pub prid: Option<u32>,
    #[serde(default, rename = "preset")]
    pub presets: Vec<Preset>,
}

/// One numbered slot.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Preset {
    /// The number printed on the remote.
    #[serde(rename = "@id")]
    pub id: u32,
    #[serde(rename = "@name")]
    pub name: Option<String>,
    /// What it plays: the player's own scheme for something it offered, or an
    /// ordinary URL for something typed in.
    #[serde(rename = "@url")]
    pub url: Option<String>,
    #[serde(rename = "@image")]
    pub image: Option<String>,
}

impl Presets {
    /// The preset already holding `url`, if one does.
    ///
    /// Compared exactly, because the URL is the player's own and is carried
    /// rather than built — the same station always names itself the same way.
    /// Anything cleverer would be guessing at a scheme this crate does not own.
    pub fn holding(&self, url: &str) -> Option<&Preset> {
        self.presets
            .iter()
            .find(|preset| preset.url.as_deref() == Some(url))
    }

    /// What to call a preset, which is its name or failing that its address.
    pub fn label(preset: &Preset) -> &str {
        match preset.name.as_deref() {
            Some(name) if !name.is_empty() => name,
            _ => preset.url.as_deref().unwrap_or("a preset"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<presets prid="52">
  <preset id="1" name="RockIt!" url="RadioParadise:/2:20/RockIt%21" image="https://img.radioparadise.com/channels/0/2/cover_512x512/0.jpg"></preset>
  <preset id="2" name="The Globe" url="RadioParadise:/3:20/The%20Globe"></preset>
  <preset id="4" name="VOCM" url="TuneIn:s24752"></preset>
</presets>"#;

    #[test]
    fn reads_a_real_list_off_a_player() {
        let presets: Presets = quick_xml::de::from_str(REAL).expect("parses");
        assert_eq!(presets.prid, Some(52));
        assert_eq!(presets.presets.len(), 3);
        assert_eq!(presets.presets[0].name.as_deref(), Some("RockIt!"));
        assert_eq!(presets.presets[0].id, 1);
    }

    #[test]
    fn slots_are_not_contiguous() {
        // Deleting a preset leaves the gap. A client that assumed 1..n would
        // address the wrong slot from here on.
        let presets: Presets = quick_xml::de::from_str(REAL).expect("parses");
        let slots: Vec<u32> = presets.presets.iter().map(|p| p.id).collect();
        assert_eq!(slots, vec![1, 2, 4]);
    }

    #[test]
    fn an_empty_list_is_not_an_error() {
        // What a player with no presets answers, and the state every player
        // starts in.
        let presets: Presets =
            quick_xml::de::from_str(r#"<presets prid="0"></presets>"#).expect("parses");
        assert!(presets.presets.is_empty());
        assert_eq!(presets.prid, Some(0));
    }

    #[test]
    fn a_url_already_saved_is_found() {
        let presets: Presets = quick_xml::de::from_str(REAL).expect("parses");
        let held = presets.holding("TuneIn:s24752").expect("already there");
        assert_eq!(held.id, 4);
        assert_eq!(Presets::label(held), "VOCM");
    }

    #[test]
    fn a_url_not_saved_is_not_found() {
        let presets: Presets = quick_xml::de::from_str(REAL).expect("parses");
        assert!(presets.holding("TuneIn:s99999").is_none());
        // Exactly, not loosely: a prefix is a different station.
        assert!(presets.holding("TuneIn:s2475").is_none());
        assert!(presets.holding("RadioParadise:/2:20").is_none());
    }

    #[test]
    fn a_preset_with_no_name_is_shown_by_its_address() {
        let one: Preset =
            quick_xml::de::from_str(r#"<preset id="1" url="http://example.invalid/s"></preset>"#)
                .expect("parses");
        assert_eq!(Presets::label(&one), "http://example.invalid/s");
    }
}
