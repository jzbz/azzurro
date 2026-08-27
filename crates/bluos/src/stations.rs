//! The tree an alarm's source is chosen from.
//!
//! `GET /RadioBrowse?service=Alarms` opens it, and every level below is the
//! same route with the item's own `service`, `url` and sometimes `key`. It is
//! not the `<screen>` grammar the rest of the app browses — this one is the
//! player's radio directory, and it is much smaller:
//!
//! ```xml
//! <radiotime service="RadioParadise">
//!   <category text="MQA" key="20">
//!     <item text="The Main Mix" type="audio"
//!           URL="RadioParadise%3A%2F0%3A20" image="https://…/0.jpg"/>
//!   </category>
//! </radiotime>
//! ```
//!
//! Two kinds of row. `type="audio"` is a leaf: its `URL` is the thing an alarm
//! would play, and picking it is the end of the walk. Anything else is a
//! folder, and following it means asking the same route again.
//!
//! The `service` on the root is inherited by rows that do not carry their own,
//! which is how the Capture list can name two inputs without repeating itself.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::error::{Error, Result};
use crate::xml::{attributes, local_name};

/// One row: something to play, or somewhere to go.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Station {
    /// What it is called. The alarm stores this as its `source`.
    pub text: String,
    /// Which service it belongs to, inherited from the document where the row
    /// does not say.
    pub service: Option<String>,
    /// Percent-encoded on the wire. Held decoded, because that is the form the
    /// alarm routes want it in.
    pub url: Option<String>,
    pub image: Option<String>,
    /// Some services page by a key rather than by URL alone.
    pub key: Option<String>,
    /// Whether this plays, as opposed to opening another level.
    pub playable: bool,
    /// The `<category>` this sat under, where it sat under one. Drawn as a
    /// heading; the player groups Radio Paradise's channels by quality this
    /// way.
    /// The heading this row sits under, shared rather than copied.
    ///
    /// One heading is repeated across every row beneath it, and copying it per
    /// row made the memory a listing costs the product of the two: a document
    /// inside the 4 MiB reply cap could name a heading of a couple of megabytes
    /// and then a hundred thousand rows to hang under it. An `Arc` makes each
    /// row cost a pointer.
    pub group: Option<std::sync::Arc<str>>,
}

impl Station {
    /// Where following this leads, as a path on the player.
    ///
    /// `None` for a leaf, which leads nowhere: it is the answer.
    pub fn into_path(&self) -> Option<String> {
        if self.playable {
            return None;
        }
        let service = self.service.as_deref().unwrap_or_default();
        let mut path = format!(
            "/RadioBrowse?service={}",
            percent_encoding::utf8_percent_encode(service, QUERY)
        );
        if let Some(key) = &self.key {
            path.push_str(&format!(
                "&key={}",
                percent_encoding::utf8_percent_encode(key, QUERY)
            ));
        }
        if let Some(url) = &self.url {
            path.push_str(&format!(
                "&url={}",
                percent_encoding::utf8_percent_encode(url, QUERY)
            ));
        }
        Some(path)
    }
}

/// What may go in a query value without being escaped.
const QUERY: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// One level of the tree.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stations {
    /// The service this level belongs to, where the document says.
    pub service: Option<String>,
    pub rows: Vec<Station>,
}

/// Read one level.
pub fn parse(xml: &str) -> Result<Stations> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = Stations::default();
    let mut seen_root = false;
    let mut group: Option<std::sync::Arc<str>> = None;

    loop {
        let (e, closed) = match reader.read_event() {
            Err(e) => return Err(Error::Screen(format!("stations: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => (e, false),
            Ok(Event::Empty(e)) => (e, true),
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == "category" {
                    group = None;
                }
                continue;
            }
            Ok(_) => continue,
        };

        let qname = e.name();
        let name = local_name(qname.as_ref());
        let mut a = attributes(&e);

        match name {
            "radiotime" => {
                out.service = a.remove("service");
                seen_root = true;
            }
            _ if !seen_root => {
                return Err(Error::Screen(format!("stations: root is <{name}>")));
            }
            "category" => {
                let text = a.remove("text");
                // A self-closing category holds nothing, so it opens nothing.
                if !closed {
                    group = text.map(std::sync::Arc::from);
                }
            }
            "item" => {
                let url = a.remove("URL").map(|raw| {
                    // Decoded here rather than at each use: the alarm routes
                    // want the plain form, and every caller would otherwise
                    // have to remember to do this.
                    percent_encoding::percent_decode_str(&raw)
                        .decode_utf8_lossy()
                        .into_owned()
                });
                out.rows.push(Station {
                    text: a.remove("text").unwrap_or_default(),
                    // The row's own service wins; the document's stands in.
                    service: a.remove("service").or_else(|| out.service.clone()),
                    url,
                    image: a.remove("image"),
                    key: a.remove("key"),
                    playable: a.remove("type").as_deref() == Some("audio"),
                    group: group.clone(),
                });
            }
            _ => {}
        }
    }

    if !seen_root {
        return Err(Error::Screen(
            "stations: no <radiotime> in the reply".to_owned(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The top of the tree, captured from a Powernode.
    const TOP: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<radiotime>
  <item text="Current play queue or station" type="audio" key="music"></item>
  <item text="Inputs" type="link" URL="presets" service="Capture" image="/images/Inputs.png"></item>
  <item text="Radio Paradise" type="link" service="RadioParadise" image="/Sources/images/RP.png"></item>
</radiotime>"#;

    #[test]
    fn the_top_of_the_tree() {
        let out = parse(TOP).expect("parses");
        assert_eq!(out.rows.len(), 3);

        // The default: it plays, so it is an answer rather than a door.
        let queue = &out.rows[0];
        assert_eq!(queue.text, "Current play queue or station");
        assert!(queue.playable);
        assert_eq!(queue.key.as_deref(), Some("music"));
        assert_eq!(queue.into_path(), None, "a leaf leads nowhere");

        // A folder carrying both a service and a relative URL.
        let inputs = &out.rows[1];
        assert!(!inputs.playable);
        assert_eq!(
            inputs.into_path().as_deref(),
            Some("/RadioBrowse?service=Capture&url=presets")
        );

        // A folder with a service and no URL at all.
        assert_eq!(
            out.rows[2].into_path().as_deref(),
            Some("/RadioBrowse?service=RadioParadise")
        );
    }

    /// A level whose rows inherit the document's service, and whose URLs are
    /// percent-encoded on the wire.
    #[test]
    fn a_level_that_inherits_and_decodes() {
        let out = parse(
            r#"<radiotime service="Capture">
                 <item text="Bluetooth" id="input5" type="audio"
                       URL="Capture%3Abluez%3Abluetooth" image="/images/bt.png"/>
               </radiotime>"#,
        )
        .expect("parses");

        let bt = &out.rows[0];
        assert_eq!(
            bt.service.as_deref(),
            Some("Capture"),
            "inherited from the root"
        );
        assert_eq!(
            bt.url.as_deref(),
            Some("Capture:bluez:bluetooth"),
            "held decoded, which is the form the alarm routes want"
        );
        assert!(bt.playable);
    }

    /// Categories become a heading on the rows inside them, and close again.
    #[test]
    fn a_category_names_the_rows_under_it() {
        let out = parse(
            r#"<radiotime service="RadioParadise">
                 <category text="MQA" key="20">
                   <item text="The Main Mix" type="audio" URL="RadioParadise%3A%2F0%3A20"/>
                 </category>
                 <item text="Loose" type="audio" URL="RadioParadise%3A%2F9"/>
               </radiotime>"#,
        )
        .expect("parses");

        assert_eq!(out.rows[0].group.as_deref(), Some("MQA"));
        assert_eq!(out.rows[0].url.as_deref(), Some("RadioParadise:/0:20"));
        assert_eq!(
            out.rows[1].group, None,
            "the category closed before this one"
        );
    }

    #[test]
    fn a_reply_from_somewhere_else_is_refused() {
        assert!(parse("<screen screenTitle=\"Home\"/>").is_err());
        assert!(parse("").is_err());
    }
}
