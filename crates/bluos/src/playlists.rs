//! Where a track can be filed: the `<addToPlaylistOptions>` document.
//!
//! The seventh grammar the player speaks, and the smallest. "Add to playlist…"
//! on a track's context menu is a `browse` whose `resultType` is
//! `AddToPlaylistOptions`, and what comes back is not a screen at all — it is
//! the ingredients for a request the client has to assemble:
//!
//! ```xml
//! <addToPlaylistOptions service="LocalMusic">
//!   <urlPath>/AddToPlaylist</urlPath>
//!   <requestParameter>sourceService=LocalMusic</requestParameter>
//!   <requestParameter>songid=%2Fvar%2Fmnt%2F…%2FGang+Shit.flac</requestParameter>
//!   <playlists service="LocalMusic" serviceName="BluOS" create="1"/>
//! </addToPlaylistOptions>
//! ```
//!
//! One `<playlists>` group per service that will take the track, each holding
//! whatever playlists already exist there. `create="1"` says that service will
//! also make a new one.
//!
//! The parameters arrive percent-encoded and are decoded here, because they go
//! back out as ordinary query parameters and would otherwise be encoded twice.
//! That is what the official controller does with them too.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::error::{Error, Result};
use crate::xml::{attributes, entity, flag, local_name};

/// Everything needed to put one track on a playlist.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AddToPlaylist {
    /// Which service the track came from.
    pub service: Option<String>,
    /// The path the add is sent to — `/AddToPlaylist` on every player seen.
    pub url_path: String,
    /// Decoded `(name, value)` pairs the player wants echoed back. These carry
    /// the identity of the track, so an add without them files nothing.
    pub parameters: Vec<(String, String)>,
    /// One per service that will accept the track.
    pub groups: Vec<Group>,
}

/// One service's playlists.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Group {
    pub service: Option<String>,
    /// What to call it on screen — "BluOS" where the service id is
    /// "LocalMusic".
    pub service_name: Option<String>,
    pub icon: Option<String>,
    /// Whether this service will make a new playlist on the spot.
    pub can_create: bool,
    pub playlists: Vec<Playlist>,
}

/// One existing playlist.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Playlist {
    pub name: String,
    /// Some services identify a playlist by id rather than by name; the add
    /// sends whichever is present.
    pub id: Option<String>,
    /// Artwork for it, which BluOS makes from the first track on it.
    pub image: Option<String>,
}

impl AddToPlaylist {
    /// Whether there is anywhere at all to put the track.
    pub fn is_empty(&self) -> bool {
        self.groups
            .iter()
            .all(|group| group.playlists.is_empty() && !group.can_create)
    }
}

/// Read the document. Anything that is not one is an error rather than an
/// empty set, because the caller asked for it by result type and getting
/// something else back means the player and this app disagree.
pub fn parse(xml: &str) -> Result<AddToPlaylist> {
    let mut reader = Reader::from_str(xml);
    // Not trimmed per fragment. A name containing an entity arrives as three
    // events — "Rock ", the `&`, " Roll" — and trimming each one on the way in
    // eats the spaces around the ampersand before they can be joined up.
    // Trimmed once when the element closes instead.
    reader.config_mut().trim_text(false);

    let mut out = AddToPlaylist::default();
    let mut seen_root = false;
    // Which element's text is being collected, if any.
    let mut collecting: Option<String> = None;
    // Built up across the events one parameter arrives as, and decoded when it
    // closes. Entities come as their own event, so a value was otherwise read
    // as two parameters with the entity missing between them.
    let mut parameter = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(ev @ (Event::Start(_) | Event::Empty(_))) => {
                // An element that closes itself carries no text and gets no
                // `End` of its own, so anything it arms below would still be
                // armed when the next text arrived — the whitespace between
                // elements included. `urlPath` is the request path a later
                // call is built from, so that text ended up in a URL.
                let closed = matches!(ev, Event::Empty(_));
                let e = match &ev {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => unreachable!("matched above"),
                };
                let qname = e.name();
                let name = local_name(qname.as_ref());
                let mut a = attributes(e);

                match name {
                    "addToPlaylistOptions" => {
                        seen_root = true;
                        out.service = a.remove("service");
                    }
                    "playlists" => out.groups.push(Group {
                        service: a.remove("service"),
                        service_name: a.remove("serviceName"),
                        icon: a.remove("serviceIcon"),
                        can_create: flag(a.remove("create")),
                        playlists: Vec::new(),
                    }),
                    // A playlist inside the group that is open.
                    //
                    // `<name image="…">Azzurro test</name>`: the element is
                    // called `name` and the playlist's name is its *text*, not
                    // an attribute. Verified against a Powernode after making
                    // a playlist on it — the shape is not guessable from the
                    // empty document a player with no playlists returns, which
                    // is exactly what this was first written against.
                    "name" | "playlist" if !out.groups.is_empty() => {
                        if let Some(group) = out.groups.last_mut() {
                            group.playlists.push(Playlist {
                                // An attribute where one is given, otherwise
                                // whatever the text turns out to be.
                                name: a.remove("name").unwrap_or_default(),
                                id: a.remove("id"),
                                image: a.remove("image"),
                            });
                        }
                        collecting = Some("playlistName".to_owned());
                    }
                    "urlPath" | "requestParameter" => collecting = Some(name.to_owned()),
                    _ => {}
                }

                if closed {
                    collecting = None;
                }
            }
            Ok(Event::Text(e)) => {
                let Some(what) = collecting.as_deref() else {
                    continue;
                };
                let text = e.decode().unwrap_or_default().into_owned();
                match what {
                    // Appended, not assigned: a name with an entity in it
                    // arrives as several events and has to be joined up.
                    "playlistName" => {
                        if let Some(playlist) =
                            out.groups.last_mut().and_then(|g| g.playlists.last_mut())
                        {
                            playlist.name.push_str(&text);
                        }
                    }
                    "urlPath" => out.url_path.push_str(&text),
                    "requestParameter" => parameter.push_str(&text),
                    _ => {}
                }
            }
            // quick-xml delivers `&amp;` as an event of its own rather than
            // as part of the text around it, so a parser that only handles
            // `Text` loses everything from the entity onward: a playlist
            // called "Rock & Roll" arrived as "Rock", and that truncated name
            // is what got posted back, since BluOS files by name and not by
            // id. `screen.rs` has handled this from the start; this did not.
            Ok(Event::GeneralRef(e)) => {
                let Some(what) = collecting.as_deref() else {
                    continue;
                };
                let Ok(name) = e.decode() else { continue };
                let resolved = entity(name.as_ref());
                match what {
                    "playlistName" => {
                        if let Some(playlist) =
                            out.groups.last_mut().and_then(|g| g.playlists.last_mut())
                        {
                            playlist.name.push_str(&resolved);
                        }
                    }
                    "urlPath" => out.url_path.push_str(&resolved),
                    // Handled here as well as in `Text`: a parameter carrying
                    // an entity arrives as three events, and dropping the
                    // middle one left the two halves to be read as separate
                    // parameters.
                    "requestParameter" => parameter.push_str(&resolved),
                    _ => {}
                }
            }
            Ok(Event::End(_)) => {
                // Now that the whole value is in hand.
                match collecting.as_deref() {
                    Some("playlistName") => {
                        if let Some(playlist) =
                            out.groups.last_mut().and_then(|g| g.playlists.last_mut())
                        {
                            let trimmed = playlist.name.trim().to_owned();
                            playlist.name = trimmed;
                        }
                    }
                    Some("urlPath") => {
                        let trimmed = out.url_path.trim().to_owned();
                        out.url_path = trimmed;
                    }
                    Some("requestParameter") => {
                        if let Some(pair) = decode_parameter(parameter.trim()) {
                            out.parameters.push(pair);
                        }
                    }
                    _ => {}
                }
                parameter.clear();
                collecting = None;
            }
            Ok(_) => {}
            Err(e) => return Err(Error::Screen(format!("{e}"))),
        }
    }

    for group in &mut out.groups {
        group.playlists.retain(|p| !p.name.is_empty());
    }

    if !seen_root {
        return Err(Error::Screen(
            "not an addToPlaylistOptions document".to_owned(),
        ));
    }
    Ok(out)
}

/// Split `name=value` and undo the encoding on both halves.
///
/// `split_once` and not `split`: a value may contain an `=` of its own, and
/// only the first one separates the pair. Decoded here because these go back
/// out as query parameters and would otherwise be encoded a second time —
/// `%2F` becoming `%252F`, which files the track under a name nobody meant.
fn decode_parameter(raw: &str) -> Option<(String, String)> {
    let (name, value) = raw.split_once('=')?;
    Some((unencode(name), unencode(value)))
}

fn unencode(raw: &str) -> String {
    // `+` is a space in a query string, and percent-decoding alone leaves it.
    let spaced = raw.replace('+', " ");
    percent_encoding::percent_decode_str(&spaced)
        .decode_utf8()
        .map(std::borrow::Cow::into_owned)
        .unwrap_or(spaced)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/AddToPlaylistOptions?service=LocalMusic&songid=…` from a real
    /// Bluesound Powernode with no playlists yet, which is why the group is
    /// empty and only `create` is offered.
    const OPTIONS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<addToPlaylistOptions service="LocalMusic">
  <urlPath>/AddToPlaylist</urlPath>
  <requestParameter>sourceService=LocalMusic</requestParameter>
  <requestParameter>songid=%2Fvar%2Fmnt%2F10.0.0.100-mediamusic%2Fplaylist%2F21+Savage+-+Gang+Shit.flac</requestParameter>
  <playlists service="LocalMusic" serviceName="BluOS" serviceIcon="/images/BluOSIcon.png" create="1"></playlists>
</addToPlaylistOptions>"#;

    /// `collecting` used to be armed by a self-closing element that had no
    /// text and no `End` to disarm it, so the next text — the indentation
    /// between elements — was appended to whatever it had armed.
    #[test]
    fn a_self_closing_element_does_not_collect_the_text_after_it() {
        let out = parse(
            r#"<addToPlaylistOptions service="Tidal">
                 <urlPath/>
                 <playlists service="Tidal"><playlist id="1" name="Mine"/></playlists>
               </addToPlaylistOptions>"#,
        )
        .expect("parses");

        assert_eq!(
            out.url_path, "",
            "an empty <urlPath/> leaves the path empty, not full of whitespace"
        );
    }

    /// An entity arrives as its own event, so a parameter carrying one was
    /// read as two parameters with the entity dropped between them.
    #[test]
    fn a_parameter_with_an_entity_stays_one_parameter() {
        let out = parse(
            r#"<addToPlaylistOptions service="Tidal">
                 <requestParameter>token=a&amp;b</requestParameter>
               </addToPlaylistOptions>"#,
        )
        .expect("parses");

        assert_eq!(
            out.parameters,
            vec![("token".to_owned(), "a&b".to_owned())],
            "one parameter, with the ampersand still in its value"
        );
    }

    #[test]
    fn reads_where_a_track_can_go() {
        let options = parse(OPTIONS).expect("parses");
        assert_eq!(options.service.as_deref(), Some("LocalMusic"));
        assert_eq!(options.url_path, "/AddToPlaylist");

        let group = &options.groups[0];
        assert_eq!(group.service_name.as_deref(), Some("BluOS"));
        assert!(group.can_create);
        assert!(group.playlists.is_empty());
        assert!(!options.is_empty(), "creating one is somewhere to put it");
    }

    #[test]
    fn parameters_are_decoded_so_they_are_not_encoded_twice() {
        let options = parse(OPTIONS).expect("parses");
        assert_eq!(
            options.parameters,
            vec![
                ("sourceService".to_owned(), "LocalMusic".to_owned()),
                (
                    "songid".to_owned(),
                    "/var/mnt/10.0.0.100-mediamusic/playlist/21 Savage - Gang Shit.flac".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_value_may_hold_an_equals_of_its_own() {
        assert_eq!(
            decode_parameter("token=a%3Db%3Dc"),
            Some(("token".to_owned(), "a=b=c".to_owned()))
        );
    }

    /// The same request once a playlist exists, captured off the Powernode
    /// after making one. The empty document above cannot show this and reading
    /// it alone is how the first version of this parser came to look for a
    /// `<playlist name="…">` that the player never writes.
    const WITH_ONE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<addToPlaylistOptions service="LocalMusic">
  <urlPath>/AddToPlaylist</urlPath>
  <requestParameter>sourceService=LocalMusic</requestParameter>
  <requestParameter>songid=%2Fvar%2Fmnt%2F10.0.0.100-mediamusic%2Fplaylist%2F21+Savage+-+Gang+Shit.flac</requestParameter>
  <playlists service="LocalMusic" create="1" serviceName="BluOS" serviceIcon="/images/BluOSIcon.png">
    <name image="/Artwork?service=LocalMusic&amp;fn=%2Fvar%2Fmnt%2Fx.flac">Azzurro test</name>
  </playlists>
</addToPlaylistOptions>"#;

    #[test]
    fn a_playlist_is_named_by_its_text_not_an_attribute() {
        let options = parse(WITH_ONE).expect("parses");
        let group = &options.groups[0];
        assert_eq!(group.playlists.len(), 1);
        assert_eq!(group.playlists[0].name, "Azzurro test");
        assert!(group.playlists[0].image.is_some());
        // BluOS files by name; other services hand back an id.
        assert_eq!(group.playlists[0].id, None);
        assert!(group.can_create, "still offers a new one");
    }

    #[test]
    fn an_id_is_taken_where_a_service_gives_one() {
        let xml = r#"<addToPlaylistOptions service="Qobuz">
  <urlPath>/AddToPlaylist</urlPath>
  <playlists service="Qobuz" serviceName="Qobuz" create="1">
    <name id="884">Late nights</name>
    <name>Driving</name>
  </playlists>
</addToPlaylistOptions>"#;
        let options = parse(xml).expect("parses");
        let group = &options.groups[0];
        assert_eq!(group.playlists.len(), 2);
        assert_eq!(group.playlists[0].id.as_deref(), Some("884"));
        assert_eq!(group.playlists[1].name, "Driving");
        assert_eq!(group.playlists[1].id, None);
    }

    #[test]
    fn nowhere_to_put_it_is_not_an_error_but_is_empty() {
        let xml = r#"<addToPlaylistOptions>
  <urlPath>/AddToPlaylist</urlPath>
  <playlists service="Tidal"></playlists>
</addToPlaylistOptions>"#;
        assert!(parse(xml).expect("parses").is_empty());
    }

    #[test]
    fn some_other_document_is_refused() {
        assert!(parse("<screen><row/></screen>").is_err());
        assert!(parse("not xml at all").is_err());
    }
    #[test]
    fn a_name_with_an_ampersand_in_it_survives_whole() {
        // The name is what gets posted back — BluOS files by name, not by id —
        // so losing everything after the entity filed the track under a
        // playlist nobody had.
        let xml = r#"<addToPlaylistOptions service="LocalMusic">
  <urlPath>/AddToPlaylist</urlPath>
  <playlists service="LocalMusic" create="1">
    <name>Rock &amp; Roll</name>
    <name>Drum &#38; Bass</name>
    <name>&amp;</name>
  </playlists>
</addToPlaylistOptions>"#;
        let options = parse(xml).expect("parses");
        let names: Vec<&str> = options.groups[0]
            .playlists
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["Rock & Roll", "Drum & Bass", "&"]);
    }
}
