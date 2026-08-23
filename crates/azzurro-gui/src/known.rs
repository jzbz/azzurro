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

use std::path::PathBuf;

use bluos::DeviceId;

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

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("azzurro").join("players"))
}

/// Every address worth trying, oldest first.
pub fn load() -> Vec<DeviceId> {
    let Some(path) = path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };

    let mut players = Vec::new();
    for line in text.lines().map(str::trim) {
        // `#` so the file can carry a note about why an address is pinned.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.parse::<DeviceId>() {
            Ok(id) if players.contains(&id) => {}
            Ok(id) => players.push(id),
            Err(_) => tracing::warn!("ignoring {line:?} in {}: not an address", path.display()),
        }
    }

    // A file edited by hand, or written by a version of this that had no
    // bound. Trimmed from the front, which is the oldest end.
    if players.len() > MAX_REMEMBERED {
        let dropping = players.len() - MAX_REMEMBERED;
        tracing::warn!(
            "{} lists {} players; trying the {MAX_REMEMBERED} most recent",
            path.display(),
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
    players.push(id);
    if players.len() > MAX_REMEMBERED {
        let dropping = players.len() - MAX_REMEMBERED;
        players.drain(..dropping);
    }
    true
}

/// Write the set back, if there is anywhere to write it.
///
/// Failure is logged and swallowed: not being able to remember a player is a
/// smaller problem than refusing to run.
pub fn save(players: &[DeviceId]) {
    let Some(path) = path() else { return };

    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::debug!("cannot remember players: {e}");
        return;
    }

    let mut body = String::from("# Players Azzurro has seen. One host:port per line.\n");
    for id in players {
        body.push_str(&id.to_string());
        body.push('\n');
    }

    if let Err(e) = std::fs::write(&path, body) {
        tracing::debug!("cannot write {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: &str) -> DeviceId {
        raw.parse().unwrap()
    }

    #[test]
    fn round_trips_through_the_file_format() {
        // The parsing half, without touching the real config directory.
        let text = "# a comment\n\n10.0.0.155:11000\n  10.0.0.9:11000  \nnonsense\n";
        let parsed: Vec<DeviceId> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.parse().ok())
            .collect();

        assert_eq!(parsed.len(), 2, "the comment, the blank and the rubbish go");
        assert_eq!(parsed[0], id("10.0.0.155:11000"));
        assert_eq!(
            parsed[1],
            id("10.0.0.9"),
            "bare host means the default port"
        );
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
