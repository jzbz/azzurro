//! Radio stations typed in by hand, rather than found in the player's tree.
//!
//! The player browses TuneIn, Radio Paradise and whatever else the account is
//! linked to, and offers no way at all to name a stream itself — the official
//! controller has no such box either. A small or local station missing from
//! those directories is therefore unreachable, and pasting its stream URL is
//! the standard escape hatch everywhere else in the world.
//!
//! What makes this possible is that `/Play?url=` takes an **ordinary URL** and
//! not only the player's own `RadioParadise:/…` scheme. Confirmed against a
//! Powernode on 4.16.22 by handing it an unreachable `http://` address: the
//! player answered `<state>stream</state>`, tried to fetch it, and fell back to
//! `stop` with the queue untouched. So the stream is the player's to play and
//! the list is ours to keep.
//!
//! Kept in `~/.config/azzurro/stations`, one per line, as the URL followed by
//! a space and then the name. That way round because a URL cannot contain a
//! space — it would have to be percent-encoded — while a station name almost
//! always does, so the split is unambiguous with no quoting or escaping.
//!
//! These live on this machine and nowhere else. The player is never told about
//! them, so they do not appear in the official app or on another computer.
//! Putting them in the player's presets would fix that and is the obvious next
//! step, but presets are their own unfinished story.

use std::path::PathBuf;

/// How many stations are worth keeping.
///
/// Generous — this is a hand-typed list and nobody types thousands — but
/// bounded, because the file is read at every startup.
pub const MAX_STATIONS: usize = 128;

/// The longest name worth keeping. A name is a label on a row, not a note.
const MAX_NAME: usize = 120;

/// And the longest URL. Well past any real stream address, and short enough
/// that a pasted document cannot become a station.
const MAX_URL: usize = 2000;

/// One station somebody typed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Station {
    pub name: String,
    pub url: String,
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("azzurro").join("stations"))
}

/// Whether a URL is one this app will hand to a player.
///
/// **`http` and `https` only.** The player accepts a good deal more — its own
/// `Capture:bluez:bluetooth` and `RadioParadise:/5:20/…` among them — and
/// those are the player's to hand out through its screens, not a user's to
/// type into a box. A scheme this app does not understand reaching `/Play`
/// from a text field is how a pasted string ends up addressing a part of the
/// player nothing in the interface exposes.
///
/// Deliberately not a full URL parse. What matters is the scheme and that
/// there is a host after it; the player is the thing that has to resolve it,
/// and it is better at that than a check here would be.
pub fn playable(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() || url.chars().count() > MAX_URL {
        return false;
    }
    // A URL with a space in it is either two things or a mistake, and it is
    // also what the file format cannot represent.
    if url.chars().any(char::is_whitespace) {
        return false;
    }

    // Case-insensitive: schemes are, and "HTTP://" is a thing people paste.
    let lowered = url.to_ascii_lowercase();
    let rest = match () {
        _ if lowered.starts_with("http://") => &url[7..],
        _ if lowered.starts_with("https://") => &url[8..],
        _ => return false,
    };

    // Something has to follow the scheme, and it must not start another one:
    // "http:///etc" names no host at all.
    !rest.is_empty() && !rest.starts_with('/')
}

/// Tidy a name into something that can be a row, and be written down.
///
/// An empty one is not an error — a station with no name is shown by its URL,
/// which is better than refusing to save what somebody just pasted.
///
/// Every way a name reaches the list goes through here: [`remember`] on the
/// way in, [`read`] at startup, [`body`] on the way to the file, and renaming
/// from the window.
///
/// Idempotent, and it has to be: a rename tidies into the row model and
/// [`body`] tidies again on the way to the file, so a second pass that changed
/// anything would leave the window holding a different name than the one
/// written down.
pub fn tidy(name: &str) -> String {
    let name: String = name
        .trim()
        // Newlines and tabs would break the file; a name is one line.
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    // Cut first and trim after, not the other way round: a cut that lands on a
    // space would otherwise leave it on the end, where the next pass would take
    // it off and disagree with this one.
    let name: String = name.chars().take(MAX_NAME).collect();
    name.trim().to_owned()
}

/// Read the file's contents into a list, in the order they were written.
fn read(text: &str) -> Vec<Station> {
    let mut out: Vec<Station> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        // The URL is everything up to the first space; the name is the rest.
        let (url, name) = match line.split_once(' ') {
            Some((url, name)) => (url, name),
            None => (line, ""),
        };
        if !playable(url) {
            continue;
        }
        if out.iter().any(|kept| kept.url == url) {
            continue;
        }
        out.push(Station {
            name: tidy(name),
            url: url.to_owned(),
        });
    }
    out.truncate(MAX_STATIONS);
    out
}

/// Render the list as the file's contents.
fn body(stations: &[Station]) -> String {
    let mut body = String::new();
    for station in stations.iter().take(MAX_STATIONS) {
        if !playable(&station.url) {
            continue;
        }
        body.push_str(station.url.trim());
        let name = tidy(&station.name);
        if !name.is_empty() {
            body.push(' ');
            body.push_str(&name);
        }
        body.push('\n');
    }
    body
}

/// Add one, newest last, and say whether that changed anything.
///
/// A URL already on the list keeps its place and its name rather than being
/// added twice: the same stream under two names is one station and one of them
/// is wrong.
pub fn remember(stations: &mut Vec<Station>, name: &str, url: &str) -> bool {
    let url = url.trim();
    if !playable(url) || stations.len() >= MAX_STATIONS {
        return false;
    }
    if stations.iter().any(|kept| kept.url == url) {
        return false;
    }
    stations.push(Station {
        name: tidy(name),
        url: url.to_owned(),
    });
    true
}

/// Every station typed in on this machine.
pub fn load() -> Vec<Station> {
    let Some(path) = path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    read(&text)
}

/// Write the list back, if there is anywhere to write it.
///
/// Failure is logged and swallowed, as with the players and searches beside
/// it: losing a station is a smaller problem than refusing to run.
pub fn save(stations: &[Station]) {
    let Some(path) = path() else { return };

    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::debug!("cannot keep stations: {e}");
        return;
    }
    if let Err(e) = std::fs::write(&path, body(stations)) {
        tracing::debug!("cannot write {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(name: &str, url: &str) -> Station {
        Station {
            name: name.to_owned(),
            url: url.to_owned(),
        }
    }

    #[test]
    fn a_line_as_the_app_actually_writes_it_keeps_its_name() {
        // The exact bytes off a real run, because a name arriving empty was
        // first blamed on this and it was innocent — the name never reached
        // the list, having been typed into a field that only reported on
        // Enter.
        let got = read("http://ice1.somafm.com/groovesalad-128-mp3 Groove Salad\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "Groove Salad");
        assert_eq!(got[0].url, "http://ice1.somafm.com/groovesalad-128-mp3");
    }

    #[test]
    fn ordinary_stream_addresses_are_playable() {
        assert!(playable("http://ice1.somafm.com/groovesalad-128-mp3"));
        assert!(playable("https://stream.example.org:8000/live.aac"));
        assert!(
            playable("HTTP://Example.COM/x"),
            "schemes are not case bound"
        );
        assert!(playable("  http://example.com/x  "), "pasted with spaces");
    }

    #[test]
    fn the_players_own_schemes_are_not_for_typing() {
        // These are the player's to hand out through its own screens. A text
        // box that accepted them would reach parts of the player nothing in
        // the interface exposes.
        assert!(!playable("Capture:bluez:bluetooth"));
        assert!(!playable("RadioParadise:/5:20/Beyond"));
        assert!(!playable("file:///etc/passwd"));
        assert!(!playable("javascript:alert(1)"));
        assert!(!playable("/Play?url=x"));
    }

    #[test]
    fn a_scheme_with_nothing_after_it_is_not_a_url() {
        assert!(!playable("http://"));
        assert!(!playable("https://"));
        assert!(!playable("http:///nohost"));
        assert!(!playable(""));
        assert!(!playable("   "));
    }

    #[test]
    fn a_url_with_a_space_is_refused() {
        // Both because it is almost certainly two things, and because the file
        // format splits on the first space.
        assert!(!playable("http://example.com/a b"));
        assert!(!playable("http://example.com/a\tb"));
    }

    #[test]
    fn tidying_a_tidy_name_changes_nothing() {
        // This has to hold: a renamed station is tidied into the row model and
        // tidied again on the way to the file, and the two must not disagree.
        let cases = [
            "Radio Paradise",
            // A cut that lands on a space: the 120th character here is one.
            &format!("{} {}", "a".repeat(MAX_NAME - 1), "b".repeat(4)),
            &format!("  \u{7}spaced out\u{7}  {}", "c".repeat(MAX_NAME)),
        ];
        for case in cases {
            let once = tidy(case);
            assert_eq!(tidy(&once), once, "tidy is not idempotent on {case:?}");
            assert!(once.chars().count() <= MAX_NAME);
            assert_eq!(once.trim(), once, "a tidy name has no loose ends");
        }
    }

    #[test]
    fn a_pasted_document_is_not_a_station() {
        let huge = format!("http://example.com/{}", "x".repeat(MAX_URL));
        assert!(!playable(&huge));
    }

    #[test]
    fn a_station_round_trips_through_the_file() {
        let list = vec![
            station(
                "SomaFM Groove Salad",
                "http://ice1.somafm.com/groovesalad-128-mp3",
            ),
            station("BBC 6 Music", "https://example.org/bbc6"),
        ];
        assert_eq!(read(&body(&list)), list);
    }

    #[test]
    fn a_name_with_spaces_survives_and_the_url_is_still_whole() {
        let list = vec![station(
            "Radio 4 — Long Wave",
            "http://example.com/r4?x=1&y=2",
        )];
        let round = read(&body(&list));
        assert_eq!(round[0].url, "http://example.com/r4?x=1&y=2");
        assert_eq!(round[0].name, "Radio 4 — Long Wave");
    }

    #[test]
    fn a_station_with_no_name_keeps_its_url() {
        let list = vec![station("", "http://example.com/anon")];
        let round = read(&body(&list));
        assert_eq!(round.len(), 1);
        assert!(round[0].name.is_empty());
        assert_eq!(round[0].url, "http://example.com/anon");
    }

    #[test]
    fn a_name_cannot_smuggle_a_newline_into_the_file() {
        // Otherwise one station becomes two, the second of them nonsense.
        let list = vec![station("One\nTwo", "http://example.com/x")];
        let written = body(&list);
        assert_eq!(written.lines().count(), 1);
        assert_eq!(read(&written)[0].name, "One Two");
    }

    #[test]
    fn a_line_the_policy_refuses_is_dropped_on_the_way_in() {
        // A file edited by hand, or written before the check existed.
        let read = read("file:///etc/passwd Nasty\nhttp://example.com/ok Fine\n");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].name, "Fine");
    }

    #[test]
    fn the_same_stream_is_not_kept_twice() {
        let mut list = Vec::new();
        assert!(remember(&mut list, "Groove", "http://example.com/gs"));
        assert!(!remember(
            &mut list,
            "Groove again",
            "http://example.com/gs"
        ));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Groove", "the first name is the one kept");
    }

    #[test]
    fn a_station_the_player_would_refuse_is_never_added() {
        let mut list = Vec::new();
        assert!(!remember(&mut list, "Sneaky", "file:///etc/passwd"));
        assert!(list.is_empty());
    }

    #[test]
    fn the_list_is_bounded() {
        let mut list = Vec::new();
        for i in 0..MAX_STATIONS + 10 {
            remember(
                &mut list,
                &format!("s{i}"),
                &format!("http://example.com/{i}"),
            );
        }
        assert_eq!(list.len(), MAX_STATIONS);
    }
}
