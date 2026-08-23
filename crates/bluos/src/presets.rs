//! The player's presets: the numbered slots its hardware buttons play.
//!
//! Forty of them, numbered from one, each holding a name and a URL to play.
//! They are the only part of the player a client can write that has nothing to
//! do with the queue, and the only one addressed by number — the numbers are
//! printed on the remote, so which slot a thing goes in is a real choice
//! rather than an implementation detail.
//!
//! Everything here was read off a Powernode on BluOS 4.16.6 rather than from
//! documentation, which does not exist: `/Presets` lists them, `/SetPreset`
//! both writes and deletes depending on whether `delete=1` is present, and
//! `/Preset?id=N` plays one. Each of the four was run against the hardware.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::error::{Error, Result};
use crate::xml::{attributes, local_name};

/// The lowest and highest slot the player accepts.
///
/// Forty is what the official controller offers, and it fills the list from
/// one — slot zero is not a slot.
pub const FIRST_SLOT: u32 = 1;
pub const LAST_SLOT: u32 = 40;

/// One numbered slot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preset {
    pub id: u32,
    pub name: String,
    /// What it plays. The player's own scheme — `RadioParadise:/5:20/Beyond...`
    /// for a station — and not something this crate can build, only carry.
    pub url: Option<String>,
    pub image: Option<String>,
}

/// Read `/Presets`.
pub fn parse(xml: &str) -> Result<Vec<Preset>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut seen_root = false;

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                let mut a = attributes(&e);

                match name {
                    "presets" => seen_root = true,
                    "preset" => {
                        // A slot with no number is not addressable, so it is
                        // not a preset — nothing could play or replace it.
                        let Some(id) = a.remove("id").and_then(|id| id.parse().ok()) else {
                            continue;
                        };
                        out.push(Preset {
                            id,
                            name: a.remove("name").unwrap_or_default(),
                            url: a.remove("url"),
                            image: a.remove("image"),
                        });
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(e) => return Err(Error::Screen(format!("{e}"))),
        }
    }

    if !seen_root {
        return Err(Error::Screen("not a presets document".to_owned()));
    }
    Ok(out)
}

/// The lowest slot nothing is using, if there is one.
pub fn first_free(presets: &[Preset]) -> Option<u32> {
    (FIRST_SLOT..=LAST_SLOT).find(|slot| !presets.iter().any(|p| p.id == *slot))
}

/// Every slot nothing is using.
pub fn free_slots(presets: &[Preset]) -> Vec<u32> {
    (FIRST_SLOT..=LAST_SLOT)
        .filter(|slot| !presets.iter().any(|p| p.id == *slot))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/Presets` from a Powernode, with one slot filled — captured by making
    /// a preset on it and reading it back.
    const ONE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<presets prid="3">
  <preset id="1" name="Azzurro probe" url="RadioParadise:/5:20/Beyond..."></preset>
</presets>"#;

    #[test]
    fn reads_a_slot() {
        let presets = parse(ONE).expect("parses");
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0].id, 1);
        assert_eq!(presets[0].name, "Azzurro probe");
        assert_eq!(
            presets[0].url.as_deref(),
            Some("RadioParadise:/5:20/Beyond...")
        );
    }

    #[test]
    fn a_player_with_none_is_not_an_error() {
        let empty = parse(r#"<presets prid="2"></presets>"#).expect("parses");
        assert!(empty.is_empty());
        assert_eq!(first_free(&empty), Some(1));
        assert_eq!(free_slots(&empty).len(), 40);
    }

    #[test]
    fn something_else_is_refused() {
        assert!(parse("<screen><row/></screen>").is_err());
        assert!(parse("not xml").is_err());
    }

    #[test]
    fn a_slot_with_no_number_cannot_be_addressed() {
        let presets = parse(r#"<presets><preset name="nameless"/></presets>"#).expect("parses");
        assert!(presets.is_empty());
    }

    #[test]
    fn the_first_free_slot_skips_what_is_taken() {
        let taken = |ids: &[u32]| -> Vec<Preset> {
            ids.iter()
                .map(|id| Preset {
                    id: *id,
                    ..Preset::default()
                })
                .collect()
        };
        assert_eq!(first_free(&taken(&[1, 2, 4])), Some(3));
        assert_eq!(first_free(&taken(&[2, 3])), Some(1));
        // A full player has nowhere to put another.
        let full: Vec<Preset> = (FIRST_SLOT..=LAST_SLOT)
            .map(|id| Preset {
                id,
                ..Preset::default()
            })
            .collect();
        assert_eq!(first_free(&full), None);
        assert!(free_slots(&full).is_empty());
    }
}
