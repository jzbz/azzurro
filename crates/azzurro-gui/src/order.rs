//! The order the user put a screen's sections in.
//!
//! The home screen arrives as a list of rows the player chose the order of,
//! and "Customise Home" lets that order be changed. Nothing about it is stored
//! on the player — the official controller keeps the same preference in its
//! own local storage, so a rearranged home screen follows the *app*, not the
//! speaker, and two controllers on one system can disagree about it. This
//! matches that behavior rather than inventing a place on the player to put
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

/// Render the map as the file's contents.
///
/// Separate from `save` for the same reason as `parse`: the whole of the
/// formatting can be tested without a config directory to write into.
fn body(orders: &Orders) -> String {
    let mut text = String::from("# Section order per screen, set by Customise Home.\n");
    for (screen, rows) in orders {
        let Some(rows) = savable(screen, rows) else {
            tracing::debug!(screen, "not saving an order the file cannot hold");
            continue;
        };
        text.push_str(screen);
        text.push_str(": ");
        text.push_str(&rows.join(", "));
        text.push('\n');
    }
    text
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

    if let Err(e) = std::fs::write(&path, body(orders)) {
        tracing::debug!("cannot save the screen order: {e}");
    }
}

/// The part of one screen's order the file can hold, or `None` if it can hold
/// none of it.
///
/// The ids come from the player's documents, and this file's structure is one
/// line per screen with a colon and commas inside it. An id carrying any of
/// those would be written out and read back as structure: a newline in a
/// screen id forges a whole entry for another screen, and it persists after
/// the player that supplied it is gone.
///
/// Refused rather than escaped. These are opaque identifiers, one containing a
/// newline is already wrong, and a format with real quoting would be a bigger
/// change than the problem deserves.
///
/// A single unwritable *row* costs only itself. Dropping the whole line over
/// one of them threw away an arrangement made by hand, silently, at the next
/// start; what the row loses instead is its place in the preference, which
/// puts it after the ones still named — where [`arrange`] already puts a
/// section it has never heard of. An unwritable *screen* id has nowhere to go,
/// and neither does a screen with no writable rows left.
///
/// Asked by the caller as well as here, because being told an arrangement was
/// saved when none of it was is worse than it not lasting.
pub fn savable(screen: &str, rows: &[String]) -> Option<Vec<String>> {
    if !writable(screen) {
        return None;
    }
    let rows: Vec<String> = rows.iter().filter(|row| writable(row)).cloned().collect();
    (!rows.is_empty()).then_some(rows)
}

/// Whether an id survives the round trip through the file above.
///
/// The separators the format uses, plus anything that would split a line.
fn writable(id: &str) -> bool {
    !id.is_empty() && !id.contains(['\n', '\r', ':', ',', '#'])
}

/// Sections that are never drawn.
///
/// The home screen's `teaser` row is BluOS advertising itself — "Add your
/// Music Services", "Queue Builder Mode" — pinned to the top by the player
/// with `noReorder`, so Customise Home cannot move it and nothing else can
/// get above it. It says nothing about the music on the system, and this app
/// is not the place the vendor gets to promote its features.
///
/// `presets` used to be here too, on the grounds that the shelf was a heading
/// over nothing and a `+` that reported itself unbuilt. That was true when the
/// app had no preset support of any kind. It is not true now: the rows the
/// player serves are ordinary `player-link`s to `/Preset?id=N`, each with a
/// context menu of the player's own offering Play, Edit and Delete, so the
/// shelf works through the same machinery as every other row on the screen.
/// Only the `+` is still the client's to build.
pub fn is_hidden(id: Option<&str>) -> bool {
    matches!(id, Some("teaser"))
}

/// The order a screen takes before anyone has arranged it.
///
/// Only Home has one, and only to lift Recently Played above the shelves the
/// player happens to list first. Anything not named keeps the player's own
/// order, after the ones that are.
pub fn default_for(screen: &str) -> Vec<String> {
    match screen {
        "screen-home" => vec!["recent".to_owned()],
        _ => Vec::new(),
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

    /// Screen and section ids come from the player's documents, and this file
    /// is line- and comma-structured. An id carrying that structure would be
    /// read back as structure — and would outlive the player that sent it.
    #[test]
    fn an_id_that_would_forge_a_line_is_not_written() {
        let mut orders = Orders::new();
        orders.insert("screen-home\nlibrary".to_owned(), vec!["recent".to_owned()]);
        orders.insert("screen-real".to_owned(), vec!["ok".to_owned()]);
        orders.insert("screen-colon:x".to_owned(), vec!["ok".to_owned()]);
        orders.insert("screen-comma".to_owned(), vec!["a,b".to_owned()]);

        let back = parse(&body(&orders));
        assert_eq!(
            back.keys().collect::<Vec<_>>(),
            vec!["screen-real"],
            "only the id that survives the format is kept"
        );
        assert!(
            !back.contains_key("library"),
            "and the forged second line never appears"
        );
    }

    #[test]
    fn one_unwritable_row_does_not_lose_the_screens_whole_order() {
        let mut orders = Orders::new();
        orders.insert(
            "screen-home".to_owned(),
            vec!["a".to_owned(), "b,c".to_owned(), "d".to_owned()],
        );

        // The row the format cannot hold is the only thing dropped. Losing the
        // whole line over it would undo an arrangement the user made by hand,
        // silently, at the next start.
        let back = parse(&body(&orders));
        assert_eq!(back["screen-home"], vec!["a", "d"]);

        // What it costs is that row's place: unnamed now, it falls in after
        // the ones that are named, which is where a section the preference has
        // never heard of goes anyway.
        let ids = ids(&["a", "b,c", "d"]);
        assert_eq!(
            arrange(&ids, &[false; 3], &back["screen-home"]),
            vec![0, 2, 1]
        );
    }

    #[test]
    fn a_line_per_screen() {
        let orders = parse("screen-home: mostUsed, recent , presets\n# a note\n\nother:a");
        assert_eq!(orders["screen-home"], vec!["mostUsed", "recent", "presets"]);
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
    #[test]
    fn only_the_vendors_own_advertising_is_never_drawn() {
        assert!(is_hidden(Some("teaser")));
        assert!(!is_hidden(Some("recent")));
        assert!(!is_hidden(Some("mostUsed")));
        assert!(!is_hidden(None));
    }

    #[test]
    fn the_presets_shelf_is_drawn() {
        // It was hidden while the app could do nothing with a preset. The rows
        // the player serves are plain player-links to /Preset?id=N, each with
        // a context menu of the player's own — Play, Edit, Delete — so the
        // shelf works through the machinery every other row already uses.
        assert!(!is_hidden(Some("presets")));
    }

    #[test]
    fn home_leads_with_what_was_last_played() {
        let wanted = default_for("screen-home");
        let ids = ids(&["teaser", "mostUsed", "presets", "recent", "playlists"]);
        let pinned = [true, false, false, false, false];
        // Teaser is still first here — `arrange` keeps what the player pinned —
        // and the caller drops it afterwards. Of what is left, recent leads and
        // the rest hold the player's order.
        assert_eq!(arrange(&ids, &pinned, &wanted), vec![0, 3, 1, 2, 4]);
        assert!(default_for("screen-favourites").is_empty());
    }
}
