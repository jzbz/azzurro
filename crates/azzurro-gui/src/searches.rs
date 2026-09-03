//! The searches this machine has made before.
//!
//! The player keeps no search history — the official controller keeps its own,
//! which is why the recent list is empty on a player you have used for years.
//! This is that list, and until now it lasted only as long as the process: the
//! app asks for a query on every keystroke, so re-finding an artist looked up
//! yesterday meant typing the whole name again.
//!
//! Kept beside the remembered players, under `~/.config/azzurro/searches`,
//! newest first. Unlike that file this one holds what the user typed rather
//! than an address they configured, so it is written no more eagerly than it
//! has to be: on a search committed and on the list being cleared, and never
//! on a keystroke. Clearing the list in the app empties the file, which is the
//! only meaning "clear" could honestly have once the list outlives the run.
//!
//! One query per line and **no comment syntax at all**, which is the one place
//! this departs from `known`: a search for `#1 hits` is a perfectly ordinary
//! thing to type, and a file that treated a leading `#` as a note would eat it
//! on the way back in. Nothing here is meant to be edited by hand, so nothing
//! needs a header explaining itself.

use std::path::PathBuf;

/// How many past searches to keep.
///
/// The list is drawn under the search field rather than in a pane of its own,
/// so it is bounded by what fits there as much as by what is useful. Lives
/// here rather than beside the search code because this is what has to hold
/// the line: an unbounded file grows for the life of the install.
pub const KEEP: usize = 8;

/// The longest query worth writing down.
///
/// A search field takes as much as is pasted into it, and a query nobody could
/// read back is not a shortcut to anything. Bounded so one paste cannot make a
/// file that is slow to read at every startup.
const MAX_LEN: usize = 200;

/// Whether a query is one this file will keep.
///
/// Asked before a query joins the recent list as well as on the way out to
/// disk, and that is the point of it being here rather than inline in either:
/// the list in the window is drawn from the same values the file holds, so a
/// query only one of them accepts is a row that disappears at the next start,
/// having pushed a real one off the end of an eight-long list to get there.
pub fn keepable(query: &str) -> bool {
    // A query cannot contain a newline — the field is one line — but it
    // arrives from outside this module, and one that did would turn a single
    // entry into two on the way back in. Dropped rather than escaped: there is
    // no reader for an escape, and this file is only ever written by the code
    // that reads it.
    !query.contains('\n')
        && !query.contains('\r')
        && !query.trim().is_empty()
        && query.chars().count() <= MAX_LEN
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("azzurro").join("searches"))
}

/// Read the file's contents into a list, newest first.
///
/// Separate from `load` so the whole of the parsing can be tested without a
/// config directory to write into.
fn read(text: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim) {
        if !keepable(line) {
            continue;
        }
        // Written by this app, so a duplicate means the file was edited or a
        // version of this wrote one. The list is a set in spirit — the same
        // query offered twice is one shortcut and one dead row.
        if seen.iter().any(|kept| kept == line) {
            continue;
        }
        seen.push(line.to_owned());
    }

    // Trimmed from the back, which is the oldest end: this list is newest
    // first, the opposite of the players file.
    seen.truncate(KEEP);
    seen
}

/// Render the list as the file's contents.
///
/// Separate from `save` for the same reason as `read`.
fn body(searches: &[String]) -> String {
    let mut body = String::new();
    for query in searches.iter().take(KEEP) {
        if !keepable(query) {
            continue;
        }
        body.push_str(query);
        body.push('\n');
    }
    body
}

/// Every past query, newest first.
pub fn load() -> Vec<String> {
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
/// Failure is logged and swallowed, for the same reason as the players file:
/// not being able to remember a search is a smaller problem than refusing to
/// run.
pub fn save(searches: &[String]) {
    let Some(path) = path() else { return };

    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::debug!("cannot remember searches: {e}");
        return;
    }

    // An empty list writes an empty file rather than removing it. Clearing the
    // list is a thing the user asked for, and leaving yesterday's file on disk
    // to be read at the next startup would undo it.
    if let Err(e) = std::fs::write(&path, body(searches)) {
        tracing::debug!("cannot write {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_query_that_starts_with_a_hash_is_a_query() {
        // The whole reason this file has no comment syntax.
        assert_eq!(
            read("#1 hits\nvan halen\n"),
            owned(&["#1 hits", "van halen"])
        );
    }

    #[test]
    fn blank_lines_are_not_searches() {
        assert_eq!(read("\n\nvan halen\n   \n"), owned(&["van halen"]));
    }

    #[test]
    fn the_same_query_twice_is_one_row() {
        assert_eq!(
            read("van halen\nrush\nvan halen\n"),
            owned(&["van halen", "rush"])
        );
    }

    #[test]
    fn the_list_is_bounded_and_the_oldest_go() {
        let many: Vec<String> = (0..40).map(|i| format!("query {i}")).collect();
        let round = read(&body(&many));
        assert_eq!(round.len(), KEEP);
        // Newest first, so the survivors are the front of the list.
        assert_eq!(round[0], "query 0");
        assert_eq!(round[KEEP - 1], format!("query {}", KEEP - 1));
    }

    #[test]
    fn a_query_with_a_newline_in_it_does_not_become_two() {
        let written = body(&owned(&["one\ntwo", "van halen"]));
        assert_eq!(read(&written), owned(&["van halen"]));
    }

    #[test]
    fn the_list_in_the_window_holds_what_the_file_would() {
        // The recent list is drawn from the same values that go to disk, so a
        // query the file refuses must not reach the list either: it would draw
        // a row that vanishes at the next start, having pushed a real one off
        // the end of an eight-long list on the way there.
        let cases = [
            "van halen",
            "#1 hits",
            "",
            "   ",
            "one\ntwo",
            "one\rtwo",
            &"x".repeat(MAX_LEN),
            &"x".repeat(MAX_LEN + 1),
        ];
        for case in cases {
            assert_eq!(
                keepable(case),
                !body(&owned(&[case])).is_empty(),
                "keepable disagrees with the file about {case:?}"
            );
        }
    }

    #[test]
    fn a_pasted_essay_is_not_written_down() {
        let essay = "x".repeat(MAX_LEN + 1);
        assert_eq!(body(&owned(&[&essay, "rush"])), "rush\n");
        // And is refused on the way back in too, for a file written by a
        // version of this that had no bound.
        assert_eq!(read(&format!("{essay}\nrush\n")), owned(&["rush"]));
    }

    #[test]
    fn a_query_survives_the_round_trip_intact() {
        let queries = owned(&["  van halen", "AC/DC", "Beyoncé", "日本語", "a & b"]);
        // Written trimmed, which is what the search field sends anyway.
        assert_eq!(
            read(&body(&queries)),
            owned(&["van halen", "AC/DC", "Beyoncé", "日本語", "a & b"])
        );
    }

    #[test]
    fn clearing_the_list_writes_a_file_that_reads_back_empty() {
        assert_eq!(body(&[]), "");
        assert!(read(&body(&[])).is_empty());
    }
}
