//! What tells this run of the app apart from another one on the same account.
//!
//! Nothing stops a second window of the app being opened beside the first, and
//! three things each of them does would land on the other's: the bus name each
//! player is exported under over MPRIS, the `.part` file a downloaded cover is
//! written into, and the temporary a store file is assembled in before it is
//! renamed into place. All three are named for the process, so that two
//! processes never pick the same name.
//!
//! The process id alone does not do that inside a sandbox. Flatpak starts every
//! launch in a PID namespace of its own, where its init is process 1 and the
//! app is the first process after it — so every copy of the Flatpak is process
//! 2 to itself. Two of them would claim the same bus names, and the second
//! would be refused every one; and they would write the same temporary names
//! into the cache and config directories they share under `~/.var/app`.
//!
//! So each of those names carries [`token`] as well, drawn once per process
//! from a source that differs between processes even where their ids do not.
//! The process id stays beside it: outside a sandbox it is unique already, and
//! in a stray `.part` it still says which process left it behind.

use std::hash::{BuildHasher, RandomState};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static TOKEN: LazyLock<u64> = LazyLock::new(|| {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    draw(std::process::id(), since_epoch)
});

/// A number this process draws once and another process almost certainly did
/// not, the same however often it is asked for.
pub fn token() -> u64 {
    *TOKEN
}

/// Hash the process id and the time it was asked at with a fresh
/// [`RandomState`].
///
/// The randomness is what does the work. The standard library seeds those keys
/// from the operating system's random source, so two copies of the app started
/// in the same instant and both calling themselves process 2 still hash to
/// different numbers. A plain `DefaultHasher::new()` would not do: its keys
/// are fixed, and it would turn the same id and the same instant into the same
/// token in both. The id and the clock go in anyway, which costs nothing and
/// still tells two processes apart by one or the other on a platform where that
/// source turns out not to be random.
fn draw(pid: u32, since_epoch: Duration) -> u64 {
    RandomState::new().hash_one((pid, since_epoch.as_nanos()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_drawn_once() {
        assert_eq!(token(), token(), "every name of one process agrees");
    }

    /// A hasher with fixed keys would give two copies of the Flatpak, both
    /// process 2 and started in one clock tick, the same token; a fresh
    /// RandomState does not. That two separate processes draw apart rests on
    /// its keys coming from the operating system, as the module says.
    #[test]
    fn the_same_id_at_the_same_instant_draws_differently() {
        let instant = Duration::from_nanos(1_789_000_000_123_456_789);
        assert_ne!(draw(2, instant), draw(2, instant));
    }
}
