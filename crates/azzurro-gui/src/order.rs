//! The order the user put a screen's sections in.
//!
//! The home screen arrives as a list of rows the player chose the order of,
//! and "Customise Home" lets that order be changed. Nothing about it is stored
//! on the player — the official controller keeps the same preference in its
//! own local storage, so a rearranged home screen follows the *app*, not the
//! speaker, and two controllers on one system can disagree about it. This
//! matches that behaviour rather than inventing a place on the player to put
//! it, which would be a guess at an API that does not exist.
//!
//! The file is `~/.config/azzurro/screen-order`, one screen per line:
//!
//! ```text
//! screen-home: mostUsed, recent, presets
//! ```
//!
//! Rows the player sends that are missing from a saved line keep their own
//! order, after the ones that are listed — so a service added later appears at
//! the bottom rather than vanishing.

use std::collections::BTreeMap;
use std::path::PathBuf;

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("azzurro").join("screen-order"))
}

/// Screen id to the row ids on it, in the order they should be drawn.
pub type Orders = BTreeMap<String, Vec<String>>;

/// Read the file, or nothing at all if it is missing or unreadable.
pub fn load() -> Orders {
    let Some(path) = path() else {
        return Orders::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Orders::new();
    };
    parse(&text)
}

fn parse(text: &str) -> Orders {
    let mut orders = Orders::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((screen, rows)) = line.split_once(':') else {
            continue;
        };
        let screen = screen.trim();
        if screen.is_empty() {
            continue;
        }
        let rows: Vec<String> = rows
            .split(',')
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_owned)
            .collect();
        if !rows.is_empty() {
            orders.insert(screen.to_owned(), rows);
        }
    }
    orders
}

/// Write the whole map back. Failure is logged and swallowed: a preference
/// that will not persist is not a reason to stop.
pub fn save(orders: &Orders) {
    let Some(path) = path() else { return };

    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::debug!("cannot save the screen order: {e}");
        return;
    }

    let mut text = String::from("# Section order per screen, set by Customise Home.\n");
    for (screen, rows) in orders {
        text.push_str(screen);
        text.push_str(": ");
        text.push_str(&rows.join(", "));
        text.push('\n');
    }

    if let Err(e) = std::fs::write(&path, text) {
        tracing::debug!("cannot save the screen order: {e}");
    }
}

/// Put `sections` in the saved order.
///
/// `pinned` marks the sections the player will not have moved; they keep the
/// front of the list whatever the preference says. Everything else sorts by
/// its position in `wanted`, and anything `wanted` has never heard of holds
/// its original position relative to the other unknowns and follows them all.
///
/// Returns the permutation rather than reordering in place, so the same
/// routine serves both the browse screen and the editor that produces it.
pub fn arrange(ids: &[Option<String>], pinned: &[bool], wanted: &[String]) -> Vec<usize> {
    let rank = |i: usize| -> Option<usize> {
        let id = ids.get(i)?.as_deref()?;
        wanted.iter().position(|w| w == id)
    };

    let mut movable: Vec<usize> = (0..ids.len())
        .filter(|i| !pinned.get(*i).copied().unwrap_or(false))
        .collect();

    // A stable sort, so two sections the preference does not mention stay in
    // the order the player sent them.
    movable.sort_by_key(|i| match rank(*i) {
        Some(place) => (0, place),
        None => (1, *i),
    });

    let mut out: Vec<usize> = (0..ids.len())
        .filter(|i| pinned.get(*i).copied().unwrap_or(false))
        .collect();
    out.extend(movable);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<Option<String>> {
        names
            .iter()
            .map(|n| (!n.is_empty()).then(|| (*n).to_owned()))
            .collect()
    }

    #[test]
    fn a_line_per_screen() {
        let orders = parse("screen-home: mostUsed, recent , presets\n# a note\n\nother:a");
        assert_eq!(
            orders["screen-home"],
            vec!["mostUsed", "recent", "presets"]
        );
        assert_eq!(orders["other"], vec!["a"]);
        assert_eq!(orders.len(), 2);
    }

    #[test]
    fn a_line_with_no_rows_is_not_an_order() {
        assert!(parse("screen-home:").is_empty());
        assert!(parse("no colon here").is_empty());
        assert!(parse(": rows, but no screen").is_empty());
    }

    #[test]
    fn what_was_written_reads_back() {
        let mut orders = Orders::new();
        orders.insert("screen-home".into(), vec!["b".into(), "a".into()]);
        let mut text = String::new();
        for (screen, rows) in &orders {
            text.push_str(&format!("{screen}: {}\n", rows.join(", ")));
        }
        assert_eq!(parse(&text), orders);
    }

    #[test]
    fn the_saved_order_is_applied() {
        let ids = ids(&["teaser", "mostUsed", "presets", "recent"]);
        let pinned = [true, false, false, false];
        let wanted = vec!["recent".to_owned(), "presets".to_owned()];
        // Teaser stays first because the player pinned it; the two named rows
        // take the order asked for; mostUsed was not mentioned so it follows.
        assert_eq!(arrange(&ids, &pinned, &wanted), vec![0, 3, 2, 1]);
    }

    #[test]
    fn sections_the_preference_never_heard_of_keep_their_own_order() {
        let ids = ids(&["a", "new-one", "b", "another"]);
        let pinned = [false; 4];
        let wanted = vec!["b".to_owned(), "a".to_owned()];
        assert_eq!(arrange(&ids, &pinned, &wanted), vec![2, 0, 1, 3]);
    }

    #[test]
    fn a_section_with_no_id_cannot_be_placed_and_so_holds_still() {
        let ids = ids(&["a", "", "b"]);
        let pinned = [false; 3];
        let wanted = vec!["b".to_owned()];
        // "b" is named and leads; the other two are unplaceable and hold the
        // order the player sent them in.
        assert_eq!(arrange(&ids, &pinned, &wanted), vec![2, 0, 1]);
    }

    #[test]
    fn no_preference_changes_nothing() {
        let ids = ids(&["a", "b", "c"]);
        let pinned = [true, false, false];
        assert_eq!(arrange(&ids, &pinned, &[]), vec![0, 1, 2]);
    }
}
