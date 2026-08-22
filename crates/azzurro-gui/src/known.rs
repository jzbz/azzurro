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

use std::collections::BTreeSet;
use std::path::PathBuf;

use bluos::DeviceId;

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("azzurro").join("players"))
}

/// Every address worth trying, newest write order irrelevant — a set, because
/// the same player being listed twice would start two pollers for it.
pub fn load() -> BTreeSet<DeviceId> {
    let Some(path) = path() else {
        return BTreeSet::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeSet::new();
    };

    text.lines()
        .map(str::trim)
        // `#` so the file can carry a note about why an address is pinned.
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| match line.parse::<DeviceId>() {
            Ok(id) => Some(id),
            Err(_) => {
                tracing::warn!("ignoring {line:?} in {}: not an address", path.display());
                None
            }
        })
        .collect()
}

/// Write the set back, if there is anywhere to write it.
///
/// Failure is logged and swallowed: not being able to remember a player is a
/// smaller problem than refusing to run.
pub fn save(players: &BTreeSet<DeviceId>) {
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

    #[test]
    fn round_trips_through_the_file_format() {
        // The parsing half, without touching the real config directory.
        let text = "# a comment\n\n10.0.0.155:11000\n  10.0.0.9:11000  \nnonsense\n";
        let parsed: BTreeSet<DeviceId> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.parse().ok())
            .collect();

        assert_eq!(parsed.len(), 2, "the comment, the blank and the rubbish go");
        assert!(parsed.contains(&"10.0.0.155:11000".parse().unwrap()));
        assert!(
            parsed.contains(&"10.0.0.9".parse().unwrap()),
            "bare host means the default port"
        );
    }
}
