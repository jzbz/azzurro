//! The players this machine has seen before.
//!
//! Discovery finds a player that is awake and reachable by broadcast. Neither
//! is guaranteed: a player asleep when the app starts never announces itself,
//! and a broadcast does not cross a subnet or a router that filters it. So the
//! addresses that have worked are remembered and tried again at startup,
//! alongside the sweep — which is what the official controller does with its
//! own stored device list.
//!
//! The file is one `host:port` per line under `~/.config/azzurro/players`,
//! deliberately plain text so an address can be pinned by hand on a machine
//! that cannot discover it at all. Addresses added that way survive, because
//! the set is seeded from the file before anything is written back; `#`
//! comments are read but not preserved, since the file is regenerated rather
//! than edited in place.
//!
//! Oldest first, so the file can be trimmed from the front. It is a list and
//! not a set because it has to be bounded, and bounding it means knowing which
//! address to drop; a set knows only that it has them. Duplicates are still
//! dropped on the way in — the same player listed twice would start two
//! pollers for it.

use std::sync::LazyLock;

use bluos::DeviceId;

use crate::store::{Durability, Store};

/// The file, and whatever is waiting to go into it. `Rename` rather than
/// `Synced`: every address here announced itself once and will again, so the
/// list rebuilds itself, and a disk flush per status poll would be a real cost
/// for that.
static STORE: LazyLock<Store> = LazyLock::new(|| Store::named("players", Durability::Rename));

/// How many addresses are worth carrying between runs.
///
/// Every one of them is tried at startup, and each try is a connection that
/// has to time out before the next player is heard from. Without a bound the
/// file only grows: a player that takes a new address from DHCP leaves the old
/// one behind, written down and dead, and nothing ever removes it.
///
/// The same order of magnitude as the players discovery will adopt, for the
/// same reason — a large BluOS install is a few dozen zones.
pub const MAX_REMEMBERED: usize = 256;

/// And how many of those one address may account for.
///
/// A real machine runs one player. A handful of ports on one address is
/// already unusual and is allowed; the whole file's worth is what this stops,
/// since the eviction below drops the oldest and would otherwise let one host
/// displace every speaker the user actually owns.
const MAX_PER_HOST: usize = 4;

/// Every address worth trying, oldest first.
///
/// Empty when the file is missing, and empty for this run when it is there but
/// unreadable — in which case nothing is written back either; see [`store`].
///
/// [`store`]: crate::store
pub fn load() -> Vec<DeviceId> {
    STORE.read().map(|text| read(&text)).unwrap_or_default()
}

/// Read the file's contents into a list, oldest first.
///
/// Separate from [`load`] so the whole of the parsing can be tested without a
/// config directory to read from.
fn read(text: &str) -> Vec<DeviceId> {
    let mut players = Vec::new();
    for line in text.lines().map(str::trim) {
        // `#` so the file can carry a note about why an address is pinned.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.parse::<DeviceId>() {
            Ok(id) if players.contains(&id) => {}
            Ok(id) => players.push(id),
            Err(_) => tracing::warn!("ignoring {line:?} in the players file: not an address"),
        }
    }

    // A file edited by hand, or written by a version of this that had no
    // bound. Trimmed from the front, which is the oldest end.
    if players.len() > MAX_REMEMBERED {
        let dropping = players.len() - MAX_REMEMBERED;
        tracing::warn!(
            "the players file lists {} players; trying the {MAX_REMEMBERED} most recent",
            players.len()
        );
        players.drain(..dropping);
    }
    players
}

/// Add an address, and say whether that changed anything.
///
/// One already listed keeps its place rather than moving to the back: the
/// order is when an address was first written down, and re-sorting it on every
/// status poll would mean rewriting the file every few seconds to record
/// nothing.
pub fn remember(players: &mut Vec<DeviceId>, id: DeviceId) -> bool {
    if players.contains(&id) {
        return false;
    }

    // A player is an address *and* a port, so one machine answering on many
    // ports is many players as far as this file is concerned — enough of them
    // to push every real speaker out of a list that drops the oldest first.
    // Answering /SyncStatus is what gets an address this far, which a hostile
    // host on the subnet can do as readily as a speaker can.
    let from_host = players.iter().filter(|kept| kept.host == id.host).count();
    if from_host >= MAX_PER_HOST {
        tracing::debug!(%id, "not remembering: {MAX_PER_HOST} already from that address");
        return false;
    }

    players.push(id);
    if players.len() > MAX_REMEMBERED {
        let dropping = players.len() - MAX_REMEMBERED;
        players.drain(..dropping);
    }
    true
}

/// Render the list as the file's contents.
fn body(players: &[DeviceId]) -> String {
    let mut body = String::from("# Players Azzurro has seen. One host:port per line.\n");
    for id in players {
        body.push_str(&id.to_string());
        body.push('\n');
    }
    body
}

/// Write the set back, off whatever thread asked.
///
/// Called in the order the list changed and written off the loop, which is the
/// order that matters: the body handed over last is the one that reaches the
/// disk, whenever the writers happen to be scheduled.
///
/// Failure is logged and swallowed: not being able to remember a player is a
/// smaller problem than refusing to run.
pub fn save(players: &[DeviceId]) {
    STORE.save(body(players));
}

/// Write whatever is left, here and now. For the way out.
pub fn flush() {
    STORE.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: &str) -> DeviceId {
        raw.parse().unwrap()
    }

    /// A player is an address and a port, so one host answering on many ports
    /// fills a list that drops the oldest first — taking the user's real
    /// speakers with it.
    #[test]
    fn one_address_cannot_fill_the_whole_file() {
        let mut players = Vec::new();
        let mine: DeviceId = "10.0.0.155:11000".parse().expect("an address");
        assert!(remember(&mut players, mine));

        let flood: Vec<DeviceId> = (0..MAX_REMEMBERED + 16)
            .map(|n| {
                format!("10.0.0.99:{}", 11000 + n)
                    .parse()
                    .expect("an address")
            })
            .collect();
        let taken = flood
            .into_iter()
            .filter(|id| remember(&mut players, *id))
            .count();

        assert_eq!(taken, MAX_PER_HOST, "one host gets a handful, not the file");
        assert!(
            players.contains(&mine),
            "and the speaker that was there first is still there"
        );

        // A genuinely different address is unaffected by the cap.
        let other: DeviceId = "10.0.0.42:11000".parse().expect("an address");
        assert!(remember(&mut players, other), "other hosts still fit");
    }

    #[test]
    fn round_trips_through_the_file_format() {
        // The parsing half, without touching the real config directory.
        let text = "# a comment\n\n10.0.0.155:11000\n  10.0.0.9:11000  \nnonsense\n";
        let parsed = read(text);

        assert_eq!(parsed.len(), 2, "the comment, the blank and the rubbish go");
        assert_eq!(parsed[0], id("10.0.0.155:11000"));
        assert_eq!(
            parsed[1],
            id("10.0.0.9"),
            "bare host means the default port"
        );
        assert_eq!(read(&body(&parsed)), parsed, "and back out again unchanged");
    }

    #[test]
    fn an_address_already_written_down_is_not_written_down_again() {
        let mut players = vec![id("10.0.0.1"), id("10.0.0.2")];
        assert!(!remember(&mut players, id("10.0.0.1")));
        assert_eq!(players, vec![id("10.0.0.1"), id("10.0.0.2")]);
        assert!(remember(&mut players, id("10.0.0.3")));
        assert_eq!(players.len(), 3, "and a new one goes on the end");
    }

    #[test]
    fn the_list_stops_growing_and_loses_its_oldest() {
        let mut players: Vec<DeviceId> = (0..MAX_REMEMBERED)
            .map(|n| id(&format!("10.0.{}.{}:11000", n / 256, n % 256)))
            .collect();
        let oldest = players[0];

        assert!(remember(&mut players, id("192.168.1.1:11000")));
        assert_eq!(players.len(), MAX_REMEMBERED);
        assert!(!players.contains(&oldest), "the front should have gone");
        assert_eq!(*players.last().unwrap(), id("192.168.1.1:11000"));
    }
}
