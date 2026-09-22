//! What each player's decoder last said, and about which track.
//!
//! The player names MQA only while the decoder is running: an MQA-authored
//! FLAC is `mqaAuthored` in every status while it plays and in none while it
//! is paused. (A 24/96 FLAC goes on saying `hd` through a pause, and needs
//! none of this.) Within a run the stored status carries the tier across the
//! pause (see `carry_decoded`), but a window opened on a player that is
//! already paused has no stored status to carry it from, and drew the
//! library's word — no badge, and CD on the row — for a file it had shown as
//! MQA a minute before it was closed.
//!
//! So the last tier each player's decoder named is written down, with what
//! says which track it named it for, and the first status of the next run
//! carries it the same way a status carries it mid-run: only onto the same
//! track (see [`bluos::Status::same_track`]), and never over a word the
//! player does say.
//!
//! Kept beside the remembered players, under `~/.config/azzurro/decoded`, one
//! player per line and tab-separated: the address, then the tier, the queue,
//! the position, the stream, the title and the file. Any of the last five is
//! empty where the player did not say; the tier never is, since a line without
//! one is not written, and is dropped when read. A player's line is rewritten
//! when the tier or the track differs from the one already there, which is
//! once a track and not once a status. A value with a tab or a line break in
//! it — a file name may hold either — is not written at all: the file has no
//! escapes, and that track showing no badge after a restart is all it costs.

use std::sync::LazyLock;

use bluos::{DeviceId, Status};

use crate::store::{Durability, Store};

/// The file, and whatever is waiting to go into it. `Rename` rather than
/// `Synced`: what it holds is the player's to say again the moment it plays.
static STORE: LazyLock<Store> = LazyLock::new(|| Store::named("decoded", Durability::Rename));

/// The last tier each player's decoder named, as a status holding that and
/// nothing but the fields that say which track it is about. One per player,
/// oldest first.
pub type Heard = Vec<(DeviceId, Status)>;

/// How many players' lines are kept.
///
/// Oldest first and trimmed from the front, as the players file is, so a full
/// file makes room for the player heard most recently rather than refusing it:
/// a refusal would be for good, since nothing else ever takes a line out, and
/// the player refused would be back to the library's word on every restart.
/// A player that moves is followed (see [`moved`]), but one that took a new
/// address while the app was closed leaves its old line to be pushed off the
/// end.
const MAX_REMEMBERED: usize = crate::known::MAX_REMEMBERED;

/// What was last heard from the player at `id`.
pub fn about(heard: &Heard, id: DeviceId) -> Option<&Status> {
    heard
        .iter()
        .find(|(player, _)| *player == id)
        .map(|(_, status)| status)
}

/// Put `status` down as the newest line, for `id` and no other.
fn keep(heard: &mut Heard, id: DeviceId, status: Status) {
    heard.retain(|(player, _)| *player != id);
    heard.push((id, status));
    if heard.len() > MAX_REMEMBERED {
        let dropping = heard.len() - MAX_REMEMBERED;
        heard.drain(..dropping);
    }
}

/// What of `status` is worth writing down: the tier it names and the track it
/// names it for, or `None` where it names no tier, names no track, or holds
/// something the file cannot.
fn keepable(status: &Status) -> Option<Status> {
    fn text(field: &Option<String>) -> Option<String> {
        field
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }
    let kept = Status {
        quality: Some(text(&status.quality)?),
        pid: status.pid,
        song: status.song,
        stream_url: text(&status.stream_url),
        title1: text(&status.title1),
        file: text(&status.file),
        ..Default::default()
    };
    let writable = [&kept.quality, &kept.stream_url, &kept.title1, &kept.file]
        .into_iter()
        .flatten()
        .all(|value| !value.contains(['\t', '\n', '\r']));
    // A status that names no track is the same as nothing to `same_track`, so
    // one written down would never be carried anywhere.
    (writable && kept.same_track(&kept)).then_some(kept)
}

/// Note what `status` says for the player at `id`, and say whether that
/// changed anything worth writing.
///
/// Nothing is forgotten here. A status that names no tier — a pause, or a
/// track the decoder has not reached — leaves the last one as it was, since
/// the next run may open on exactly that pause.
pub fn remember(heard: &mut Heard, id: DeviceId, status: &Status) -> bool {
    let Some(kept) = keepable(status) else {
        return false;
    };
    if about(heard, id).is_some_and(|was| was.same_track(&kept) && was.quality == kept.quality) {
        return false;
    }
    keep(heard, id, kept);
    true
}

/// Follow a player from the address it left to the one it answers on now, and
/// say whether that changed anything worth writing.
///
/// The line is the player's and not the address's: left where it was, a
/// player whose lease moved it came back to the library's word at the next
/// start, paused on the very track the file had the tier for. What the new
/// address has already said for itself is kept over what the old one said.
/// See [`crate::Backend::moved_here`].
pub fn moved(heard: &mut Heard, from: DeviceId, to: DeviceId) -> bool {
    let Some(at) = heard.iter().position(|(player, _)| *player == from) else {
        return false;
    };
    let (_, status) = heard.remove(at);
    if about(heard, to).is_none() {
        keep(heard, to, status);
    }
    true
}

/// Read the file's contents.
///
/// Separate from `load` so the whole of the parsing can be tested without a
/// config directory to write into.
fn read(text: &str) -> Heard {
    fn field(value: &str) -> Option<String> {
        Some(value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }
    fn number(value: &str) -> Option<u32> {
        value.trim().parse().ok()
    }

    let mut heard = Heard::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let [address, tier, queue, song, stream, title, file] = fields[..] else {
            continue;
        };
        let Ok(id) = address.trim().parse::<DeviceId>() else {
            continue;
        };
        let status = Status {
            quality: field(tier),
            pid: number(queue),
            song: number(song),
            stream_url: field(stream),
            title1: field(title),
            file: field(file),
            ..Default::default()
        };
        // Through the same door as a status off the wire, so a line that was
        // edited into something the file would never have written is dropped
        // rather than carried.
        if let Some(kept) = keepable(&status) {
            keep(&mut heard, id, kept);
        }
    }
    heard
}

/// Render what has been heard as the file's contents.
///
/// Separate from `save` for the same reason as `read`.
fn body(heard: &Heard) -> String {
    fn field(value: &Option<String>) -> &str {
        value.as_deref().unwrap_or_default()
    }
    fn number(value: Option<u32>) -> String {
        value.map(|n| n.to_string()).unwrap_or_default()
    }

    let mut body =
        String::from("# What each player's decoder last named, for the track it named it for.\n");
    let skip = heard.len().saturating_sub(MAX_REMEMBERED);
    for (id, status) in heard.iter().skip(skip) {
        let Some(kept) = keepable(status) else {
            continue;
        };
        let line = [
            id.to_string(),
            field(&kept.quality).to_owned(),
            number(kept.pid),
            number(kept.song),
            field(&kept.stream_url).to_owned(),
            field(&kept.title1).to_owned(),
            field(&kept.file).to_owned(),
        ]
        .join("\t");
        body.push_str(&line);
        body.push('\n');
    }
    body
}

/// Everything heard before, by player.
///
/// Empty when the file is missing, and empty for this run when it is there but
/// unreadable — in which case nothing is written back either; see [`store`].
///
/// [`store`]: crate::store
pub fn load() -> Heard {
    STORE.read().map(|text| read(&text)).unwrap_or_default()
}

/// Write the whole of it back, off whatever thread asked. Failure is logged
/// and swallowed: a badge that will not survive a restart is not a reason to
/// stop.
#[cfg(not(test))]
pub fn save(heard: &Heard) {
    STORE.save(body(heard));
}

/// Nothing, under test. The poll loop is spawned by tests against fake players
/// on the loopback, and a status from one of those would otherwise be written
/// into the config directory of whoever runs them.
#[cfg(test)]
pub fn save(heard: &Heard) {
    let _ = body(heard);
}

/// Write whatever is left, here and now. For the way out.
pub fn flush() {
    STORE.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(port: u16) -> DeviceId {
        DeviceId::new([192u8, 0, 2, 155], port)
    }

    fn playing_mqa() -> Status {
        Status {
            state: Some("play".to_owned()),
            secs: Some(12),
            quality: Some("mqaAuthored".to_owned()),
            stream_format: Some("16/44.1".to_owned()),
            pid: Some(7),
            song: Some(0),
            title1: Some("A Song".to_owned()),
            title2: Some("A Band".to_owned()),
            file: Some("/var/mnt/share/A Band - A Song.flac".to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn what_was_heard_comes_back_from_the_file() {
        let mut heard = Heard::new();
        assert!(remember(&mut heard, player(11000), &playing_mqa()));

        let station = Status {
            quality: Some("320000".to_owned()),
            pid: Some(7),
            song: Some(0),
            stream_url: Some("http://stream.example/radio".to_owned()),
            title1: Some("A Station".to_owned()),
            ..Default::default()
        };
        assert!(remember(&mut heard, player(11010), &station));

        let back = read(&body(&heard));
        assert_eq!(back.len(), 2);

        let track = about(&back, player(11000)).expect("the first player");
        assert_eq!(track.quality.as_deref(), Some("mqaAuthored"));
        assert!(track.same_track(&playing_mqa()));
        // Only what names the track, and the tier. The rest was never written.
        assert_eq!(track.state, None);
        assert_eq!(track.title2, None);
        assert_eq!(track.stream_format, None);

        let tuned = about(&back, player(11010)).expect("the second player");
        assert!(tuned.same_track(&station));
        assert_eq!(tuned.quality.as_deref(), Some("320000"));
    }

    #[test]
    fn a_line_is_written_once_a_track_and_not_once_a_status() {
        let mut heard = Heard::new();
        assert!(remember(&mut heard, player(11000), &playing_mqa()));

        let later = Status {
            secs: Some(90),
            ..playing_mqa()
        };
        assert!(
            !remember(&mut heard, player(11000), &later),
            "the same word"
        );

        let paused = Status {
            state: Some("pause".to_owned()),
            quality: None,
            ..playing_mqa()
        };
        assert!(!remember(&mut heard, player(11000), &paused));
        assert_eq!(
            about(&heard, player(11000)).and_then(|s| s.quality.as_deref()),
            Some("mqaAuthored"),
            "a pause forgets nothing: the next run may open on it"
        );

        let changed = Status {
            quality: Some("cd".to_owned()),
            ..playing_mqa()
        };
        assert!(remember(&mut heard, player(11000), &changed), "a new word");

        let next = Status {
            song: Some(1),
            title1: Some("The Next Song".to_owned()),
            file: Some("/var/mnt/share/A Band - The Next Song.flac".to_owned()),
            quality: Some("cd".to_owned()),
            ..playing_mqa()
        };
        assert!(remember(&mut heard, player(11000), &next), "a new track");
    }

    #[test]
    fn what_the_file_cannot_hold_is_not_written() {
        let mut heard = Heard::new();
        let tabbed = Status {
            file: Some("/var/mnt/share/A\tSong.flac".to_owned()),
            ..playing_mqa()
        };
        assert!(!remember(&mut heard, player(11000), &tabbed));

        let untitled = Status {
            quality: Some("cd".to_owned()),
            ..Default::default()
        };
        assert!(!remember(&mut heard, player(11000), &untitled), "no track");
        assert!(heard.is_empty());
    }

    /// Each malformed line on a port of its own, so one that got in could not
    /// hide behind a good line for the same player; and the good line read
    /// column by column, which is what pins the order the module describes.
    #[test]
    fn a_line_the_file_would_not_have_written_is_dropped() {
        let text = "# a note\n\
                    \n\
                    192.0.2.155:11000\tmqaAuthored\t7\t0\thttp://s.example/a\tA Song\t/a.flac\n\
                    not an address\tcd\t7\t0\t\tA Song\t\n\
                    192.0.2.155:11001\tcd\t7\n\
                    192.0.2.155:11002\t\t7\t0\t\tA Song\t\n\
                    192.0.2.155:11003\tcd\t\t\t\t\t\n";
        let heard = read(text);
        assert_eq!(heard.len(), 1, "{heard:?}");

        let line = about(&heard, player(11000)).expect("the good line");
        assert_eq!(line.quality.as_deref(), Some("mqaAuthored"));
        assert_eq!(line.pid, Some(7));
        assert_eq!(line.song, Some(0));
        assert_eq!(line.stream_url.as_deref(), Some("http://s.example/a"));
        assert_eq!(line.title1.as_deref(), Some("A Song"));
        assert_eq!(line.file.as_deref(), Some("/a.flac"));
    }

    /// Full, the file makes room for the newest player rather than refusing
    /// it: nothing else ever takes a line out, so a refusal would be for good.
    #[test]
    fn a_full_file_lets_the_oldest_line_go() {
        let mut heard = Heard::new();
        for port in 0..MAX_REMEMBERED as u16 {
            assert!(remember(&mut heard, player(20000 + port), &playing_mqa()));
        }
        assert!(remember(&mut heard, player(11000), &playing_mqa()));
        assert_eq!(heard.len(), MAX_REMEMBERED);
        assert!(about(&heard, player(20000)).is_none(), "the oldest went");
        assert!(about(&heard, player(11000)).is_some(), "the newest came in");

        // And a line rewritten is a line made newest.
        let next = Status {
            song: Some(1),
            title1: Some("The Next Song".to_owned()),
            file: Some("/var/mnt/share/A Band - The Next Song.flac".to_owned()),
            ..playing_mqa()
        };
        assert!(remember(&mut heard, player(20001), &next));
        assert!(remember(&mut heard, player(11001), &playing_mqa()));
        assert!(
            about(&heard, player(20001)).is_some(),
            "kept for being heard"
        );
        assert!(about(&heard, player(20002)).is_none());

        assert_eq!(read(&body(&heard)).len(), MAX_REMEMBERED);
    }

    #[test]
    fn a_player_that_moves_takes_its_line_with_it() {
        let mut heard = Heard::new();
        assert!(remember(&mut heard, player(11000), &playing_mqa()));

        assert!(moved(&mut heard, player(11000), player(11010)));
        assert!(about(&heard, player(11000)).is_none());
        assert!(about(&heard, player(11010)).is_some_and(|s| s.same_track(&playing_mqa())));

        assert!(
            !moved(&mut heard, player(11000), player(11020)),
            "nothing to move"
        );

        // What the new address has said for itself stands.
        let cd = Status {
            quality: Some("cd".to_owned()),
            ..playing_mqa()
        };
        assert!(remember(&mut heard, player(11030), &cd));
        assert!(moved(&mut heard, player(11010), player(11030)));
        assert_eq!(heard.len(), 1);
        assert_eq!(
            about(&heard, player(11030)).and_then(|s| s.quality.as_deref()),
            Some("cd")
        );
    }
}
