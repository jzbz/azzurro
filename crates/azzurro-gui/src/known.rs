//! The players this machine has seen before, and who each one was.
//!
//! Discovery finds a player that is awake and reachable by broadcast. Neither
//! is guaranteed: a player asleep when the app starts never announces itself,
//! and a broadcast does not cross a subnet or a router that filters it. So the
//! addresses that have worked are remembered and tried again at startup,
//! alongside the sweep — which is what the official controller does with its
//! own stored device list.
//!
//! The file is one player per line under `~/.config/azzurro/players`: the
//! `host:port`, then after a space the MAC that address last answered
//! `/SyncStatus` with, as `10.0.0.155:11000 90:56:82:80:34:7a`. Deliberately
//! plain text so an address can be pinned by hand on a machine that cannot
//! discover it at all, and the MAC can be left off — it is written in once the
//! address answers. Addresses added that way survive, because the set is
//! seeded from the file before anything is written back; `#` comments are
//! read but not preserved, since the file is regenerated rather than edited in
//! place.
//!
//! The MAC is what a player that took a new address while the app was closed
//! is recognized by. The address it left never answers again, so nothing in
//! the next run would ever say whose it was: with only the address written
//! down, the player announcing from its new one matched nothing, and the old
//! address stayed as a second row that answered nothing, in the file for good.
//! See [`crate::Backend::moved_here`].
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

/// One player as the file remembers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remembered {
    pub id: DeviceId,
    /// The MAC this address last answered `/SyncStatus` with, as the plain
    /// lowercase hex [`crate::identity_from_mac`] makes of it. `None` for an
    /// address that has not answered with one — pinned by hand and not yet
    /// heard from, or a player whose MAC is not a name.
    pub identity: Option<String>,
}

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

/// Every player worth trying, oldest first.
///
/// Empty when the file is missing, and empty for this run when it is there but
/// unreadable — in which case nothing is written back either; see [`store`].
///
/// [`store`]: crate::store
pub fn load() -> Vec<Remembered> {
    STORE.read().map(|text| read(&text)).unwrap_or_default()
}

/// Read the file's contents into a list, oldest first.
///
/// Separate from [`load`] so the whole of the parsing can be tested without a
/// config directory to read from.
fn read(text: &str) -> Vec<Remembered> {
    let mut players: Vec<Remembered> = Vec::new();
    for line in text.lines().map(str::trim) {
        // `#` so the file can carry a note about why an address is pinned.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(Ok(id)) = words.next().map(str::parse::<DeviceId>) else {
            tracing::warn!("ignoring {line:?} in the players file: not an address");
            continue;
        };
        if players.iter().any(|kept| kept.id == id) {
            continue;
        }
        // A MAC that cannot be read costs only itself. The address is what
        // gets a player tried at all, and an identity is only ever a help in
        // recognizing it — while a wrong one retires a player that has not
        // moved — so the line is kept without one rather than dropped.
        // Through the same door as a MAC off the wire, which is what turns
        // whatever separators a hand-edited line has into the one spelling,
        // and refuses the values that name no player.
        let identity = match (words.next(), words.next()) {
            (None, _) => None,
            (Some(mac), None) => {
                let identity = crate::identity_from_mac(mac);
                if identity.is_none() {
                    tracing::warn!("ignoring the MAC on {line:?} in the players file");
                }
                identity
            }
            (Some(_), Some(_)) => {
                tracing::warn!("ignoring all but the address on {line:?} in the players file");
                None
            }
        };
        players.push(Remembered { id, identity });
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
/// nothing. Who answered there is [`identify`]'s to write down.
pub fn remember(players: &mut Vec<Remembered>, id: DeviceId) -> bool {
    if players.iter().any(|kept| kept.id == id) {
        return false;
    }

    // A player is an address *and* a port, so one machine answering on many
    // ports is many players as far as this file is concerned — enough of them
    // to push every real speaker out of a list that drops the oldest first.
    // Answering /SyncStatus is what gets an address this far, which a hostile
    // host on the subnet can do as readily as a speaker can.
    let from_host = players
        .iter()
        .filter(|kept| kept.id.host == id.host)
        .count();
    if from_host >= MAX_PER_HOST {
        tracing::debug!(%id, "not remembering: {MAX_PER_HOST} already from that address");
        return false;
    }

    players.push(Remembered { id, identity: None });
    if players.len() > MAX_REMEMBERED {
        let dropping = players.len() - MAX_REMEMBERED;
        players.drain(..dropping);
    }
    true
}

/// Write down who answered at an address, and say whether that changed
/// anything.
///
/// Only for an address already listed: one [`remember`] turned away stays
/// unlisted. And only where it differs from what is written, which is what
/// keeps an answer that says what the file already says from rewriting it.
/// `None` replaces an identity as readily as another identity does: an
/// address that now answers without a MAC that names anyone is no longer
/// evidence of who it was, and a stale identity left on it could retire
/// whichever player announces with it next.
pub fn identify(players: &mut [Remembered], id: DeviceId, identity: Option<&str>) -> bool {
    let Some(player) = players.iter_mut().find(|kept| kept.id == id) else {
        return false;
    };
    if player.identity.as_deref() == identity {
        return false;
    }
    player.identity = identity.map(str::to_owned);
    true
}

/// Drop every address on another host that is written down as `identity`,
/// now that `id` has answered `/SyncStatus` as it, and say whether that
/// changed anything.
///
/// An answer naming a player at one address is the evidence that a move
/// waits for before the address it left goes — see [`crate::Backend::vacated`]
/// — and it says the same about every other address that player is written
/// down at, whether or not a move was ever seen. Left alone they pile up:
/// an address typed in by hand is written down as soon as it answers, with
/// nothing to say a line for the address the player left is the same
/// player, so both lines end up naming it; and the next time it moves, an
/// announcement then finds two entries answering to it on two hosts, which
/// is no evidence about which of them moved, and moves neither.
///
/// Only other hosts. A box that presents several zones answers for all of
/// them with one MAC, on one host and several ports, and each of those is a
/// player of its own.
pub fn claim(players: &mut Vec<Remembered>, id: DeviceId, identity: &str) -> bool {
    let before = players.len();
    players.retain(|kept| kept.id.host == id.host || kept.identity.as_deref() != Some(identity));
    players.len() != before
}

/// Drop an address, and who answered there with it, and say whether that
/// changed anything.
///
/// For a player that has turned up somewhere else: the address it used to
/// answer on is not a player any more, and left in the file it is probed at
/// every start until it is pushed off the end. See [`crate::Backend::moved_here`].
/// And for one the user has told the app to forget, for the same reason; see
/// [`crate::Backend::forget`].
pub fn forget(players: &mut Vec<Remembered>, id: DeviceId) -> bool {
    let before = players.len();
    players.retain(|kept| kept.id != id);
    players.len() != before
}

/// Render the list as the file's contents.
fn body(players: &[Remembered]) -> String {
    let mut body = String::from(
        "# Players Azzurro has seen: host:port, then the MAC it last answered with.\n",
    );
    for player in players {
        body.push_str(&player.id.to_string());
        if let Some(identity) = &player.identity {
            body.push(' ');
            // Written the way a player writes its own, so a line can be
            // checked against the label on the back of the box.
            for (at, digit) in identity.chars().enumerate() {
                if at > 0 && at % 2 == 0 {
                    body.push(':');
                }
                body.push(digit);
            }
        }
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
#[cfg(not(test))]
pub fn save(players: &[Remembered]) {
    STORE.save(body(players));
}

/// Nothing, under test. Tests drive `/SyncStatus` against fake players on the
/// loopback, and an answer from one of those would otherwise be written into
/// the players file of whoever runs them.
#[cfg(test)]
pub fn save(players: &[Remembered]) {
    let _ = body(players);
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

    fn ids(players: &[Remembered]) -> Vec<DeviceId> {
        players.iter().map(|player| player.id).collect()
    }

    fn listed(players: &[Remembered], wanted: DeviceId) -> bool {
        players.iter().any(|player| player.id == wanted)
    }

    fn at(raw: &str, identity: Option<&str>) -> Remembered {
        Remembered {
            id: id(raw),
            identity: identity.map(str::to_owned),
        }
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
            listed(&players, mine),
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
        assert_eq!(parsed[0], at("10.0.0.155:11000", None));
        assert_eq!(
            parsed[1],
            at("10.0.0.9", None),
            "bare host means the default port"
        );
        assert_eq!(read(&body(&parsed)), parsed, "and back out again unchanged");
    }

    /// Who answered at each address goes out and comes back with it, and an
    /// address that has not answered with anyone yet sits beside them as it
    /// always did.
    #[test]
    fn who_answered_where_round_trips_through_the_file() {
        let players = vec![
            at("10.0.0.156:11000", Some("90568280347a")),
            at("10.0.0.7:11000", None),
            at("[fe80::1]:11000", Some("aabbccddeeff")),
        ];

        let text = body(&players);
        assert!(
            text.contains("10.0.0.156:11000 90:56:82:80:34:7a\n"),
            "written the way the player writes its own MAC: {text:?}"
        );
        assert!(text.contains("10.0.0.7:11000\n"));
        assert_eq!(read(&text), players);
    }

    /// The MAC is read however a hand-edited line separates it, and a line
    /// whose MAC cannot be read still gets its player tried.
    #[test]
    fn a_mac_that_cannot_be_read_costs_only_itself() {
        let text = "\
            10.0.0.1:11000 90-56-82-80-34-7A\n\
            10.0.0.2:11000 90568280347a\n\
            10.0.0.3:11000 not-a-mac\n\
            10.0.0.4:11000 00:00:00:00:00:00\n\
            10.0.0.5:11000 90:56:82:80:34:7a and a note\n\
            10.0.0.1:11000 aa:bb:cc:dd:ee:ff\n";
        let parsed = read(text);

        assert_eq!(
            parsed,
            vec![
                at("10.0.0.1:11000", Some("90568280347a")),
                at("10.0.0.2:11000", Some("90568280347a")),
                at("10.0.0.3:11000", None),
                // The value a player whose MAC was never written reports: a
                // name every such player shares, so no name at all.
                at("10.0.0.4:11000", None),
                at("10.0.0.5:11000", None),
            ],
            "every address is kept, the first of a duplicate wins, and only \
             a MAC that names one player comes with it"
        );
    }

    #[test]
    fn an_address_already_written_down_is_not_written_down_again() {
        let mut players = vec![at("10.0.0.1", Some("90568280347a")), at("10.0.0.2", None)];
        assert!(!remember(&mut players, id("10.0.0.1")));
        assert_eq!(
            players,
            vec![at("10.0.0.1", Some("90568280347a")), at("10.0.0.2", None)],
            "nor is who answered there lost by trying"
        );
        assert!(remember(&mut players, id("10.0.0.3")));
        assert_eq!(players.len(), 3, "and a new one goes on the end");
    }

    /// The file is rewritten when who answered at an address changes, and only
    /// then — which is every status that re-reads `/SyncStatus` otherwise.
    #[test]
    fn who_answered_is_written_down_only_when_it_changes() {
        let mut players = vec![at("10.0.0.1", None), at("10.0.0.2", Some("aabbccddeeff"))];

        assert!(identify(&mut players, id("10.0.0.1"), Some("90568280347a")));
        assert!(
            !identify(&mut players, id("10.0.0.1"), Some("90568280347a")),
            "the same answer twice is nothing to write"
        );
        assert!(
            identify(&mut players, id("10.0.0.2"), Some("90568280347a")),
            "a lease that went to another speaker is"
        );
        assert!(
            identify(&mut players, id("10.0.0.2"), None),
            "and so is an answer that names nobody"
        );
        assert!(
            !identify(&mut players, id("10.0.0.9"), Some("90568280347a")),
            "an address that is not listed is not listed by this"
        );
        assert_eq!(
            players,
            vec![at("10.0.0.1", Some("90568280347a")), at("10.0.0.2", None)]
        );
    }

    /// A forgotten address takes who answered there with it, so it cannot come
    /// back carrying an identity it no longer has any evidence for.
    #[test]
    fn a_forgotten_address_takes_its_identity_with_it() {
        let mut players = vec![at("10.0.0.1", Some("90568280347a")), at("10.0.0.2", None)];

        assert!(forget(&mut players, id("10.0.0.1")));
        assert!(
            !forget(&mut players, id("10.0.0.1")),
            "nothing left to drop"
        );
        assert_eq!(ids(&players), vec![id("10.0.0.2")]);

        assert!(remember(&mut players, id("10.0.0.1")));
        assert_eq!(
            players.last(),
            Some(&at("10.0.0.1", None)),
            "back as an address nobody has answered at yet"
        );
    }

    /// An answer naming a player takes that player off every address it has
    /// left, and off nothing else.
    #[test]
    fn an_answer_takes_its_player_off_the_addresses_it_has_left() {
        let mut players = vec![
            // Where it was before two leases, and before one, the second
            // typed in by hand and so never matched to the first.
            at("10.0.0.155:11000", Some("90568280347a")),
            at("10.0.0.157:11000", Some("90568280347a")),
            // Another player, and an address nobody has answered at.
            at("10.0.0.7:11000", Some("aabbccddeeff")),
            at("10.0.0.8:11000", None),
            // And the other zone of the box answering now, beside it.
            at("10.0.0.160:11010", Some("90568280347a")),
        ];

        assert!(claim(&mut players, id("10.0.0.160:11000"), "90568280347a"));
        assert_eq!(
            players,
            vec![
                at("10.0.0.7:11000", Some("aabbccddeeff")),
                at("10.0.0.8:11000", None),
                at("10.0.0.160:11010", Some("90568280347a")),
            ],
            "both addresses it left go; another player, an address with nobody \
             on it, and a second zone on the same box stay"
        );
        assert!(
            !claim(&mut players, id("10.0.0.160:11000"), "90568280347a"),
            "nothing left to take"
        );
        assert!(
            !claim(&mut players, id("10.0.0.9:11000"), "001122334455"),
            "and a player written down nowhere else takes nothing"
        );
    }

    #[test]
    fn the_list_stops_growing_and_loses_its_oldest() {
        let mut players: Vec<Remembered> = (0..MAX_REMEMBERED)
            .map(|n| at(&format!("10.0.{}.{}:11000", n / 256, n % 256), None))
            .collect();
        let oldest = players[0].id;

        assert!(remember(&mut players, id("192.168.1.1:11000")));
        assert_eq!(players.len(), MAX_REMEMBERED);
        assert!(!listed(&players, oldest), "the front should have gone");
        assert_eq!(players.last().unwrap().id, id("192.168.1.1:11000"));
    }
}
