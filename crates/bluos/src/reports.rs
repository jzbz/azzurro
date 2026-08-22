//! The two pages a player serves that are worth reading rather than opening.
//!
//! Most of what the official controller reaches for outside its own screens is
//! an interactive web page — signing into a service, submitting logs — and
//! those belong in a browser. Two are not: the upgrade check answers one line
//! of text, and diagnostics is a table of facts. Both are read-only, so they
//! can be shown in the app instead of handing the user off to a browser to
//! read five values.
//!
//! This is scraping, and it is the only scraping in the crate. Both pages are
//! hand-written HTML from the player's own web UI rather than an API, so a
//! firmware update could change them; everything here therefore degrades to
//! "nothing found" rather than to an error, and the caller can still offer the
//! page itself.

/// The result of an upgrade check, as one line.
///
/// The page is jQuery Mobile with a single `data-role="content"` block holding
/// one paragraph — "No update available." when there is nothing to do.
pub fn upgrade_status(html: &str) -> Option<String> {
    let content = html.split("data-role=\"content\"").nth(1)?;
    let text = between(content, "<p>", "</p>")?;
    let text = strip_tags(text);
    (!text.is_empty()).then_some(text)
}

/// The player's diagnostics, as the label/value pairs it prints.
///
/// Laid out as alternating `ui-block-a` (label) and `ui-block-b` (value)
/// divs — a jQuery Mobile two-column grid.
pub fn diagnostics(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = html;

    while let Some(at) = rest.find("ui-block-a\">") {
        rest = &rest[at + "ui-block-a\">".len()..];
        let Some(label) = rest.find("</div>").map(|end| &rest[..end]) else {
            break;
        };
        let Some(after) = rest.find("ui-block-b\">") else {
            break;
        };
        rest = &rest[after + "ui-block-b\">".len()..];
        let Some(value) = rest.find("</div>").map(|end| &rest[..end]) else {
            break;
        };

        let label = strip_tags(label);
        let value = strip_tags(value);
        if !label.is_empty() {
            // The player writes "IP Address:" with the colon; the colon is
            // presentation and belongs to whoever draws the row.
            out.push((label.trim_end_matches(':').to_owned(), value));
        }
    }
    out
}

/// The action the upgrade page offers, when it has one.
///
/// Returned as the player's own href rather than an endpoint of this crate's
/// choosing. Starting a firmware upgrade is the one operation where a guessed
/// URL could leave somebody with a brick, so nothing here is invented: if the
/// page offers a link, it is followed; if it does not, there is nothing to do.
///
/// **Untested.** The player this was written against has no update to install,
/// so its page carries no link to have been read.
pub fn upgrade_action(html: &str) -> Option<(String, String)> {
    let content = html.split("data-role=\"content\"").nth(1)?;
    let anchor = content.split("<a ").nth(1)?;
    let href = between(anchor, "href=\"", "\"")?;
    let label = between(anchor, ">", "</a>")
        .map(strip_tags)
        .unwrap_or_default();

    // A jQuery Mobile page is full of navigation chrome; only a link that says
    // what it does is worth offering as an action.
    (!href.is_empty() && !label.is_empty()).then(|| (label, href.to_owned()))
}

fn between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = haystack.find(open)? + open.len();
    let end = haystack[start..].find(close)? + start;
    Some(&haystack[start..end])
}

/// Text with any tags removed and whitespace collapsed. Enough for two pages
/// of plain values; not an HTML parser and not pretending to be one.
fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut inside = false;
    for c in raw.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/upgrade?noheader=1` from a real player, with nothing to install.
    const UPGRADE: &str = r#"<html><body>
        <div data-role="page" id="upgrade">
            <div data-theme="b" data-role="header"><h1>Check For Upgrade</h1></div>
            <div data-role="content"><p>No update available.</p></div>
            <div data-theme="b" data-role="footer"><h1>Please wait...</h1></div>
        </div></body></html>"#;

    /// `/diagnostics`, with the address and MAC replaced.
    const DIAGNOSTICS: &str = r#"<div class="diags-wrap"><div class="ui-grid-a">
        <div class="ui-block-a">IP Address:</div>
<div class="ui-block-b">192.0.2.155</div>
        <div class="ui-block-a">MAC Address:</div>
<div class="ui-block-b">aa:bb:cc:dd:ee:ff</div>
        <div class="ui-block-a">BluOS Version:</div>
<div class="ui-block-b">4.16.6</div>
        <div class="ui-block-a">Uptime:</div>
<div class="ui-block-b">2133h53m40s</div>
        <div class="ui-block-a">Total Songs:</div>
<div class="ui-block-b">2059</div>
        </div></div>"#;

    #[test]
    fn reads_the_upgrade_answer() {
        assert_eq!(
            upgrade_status(UPGRADE).as_deref(),
            Some("No update available.")
        );
        // The header also holds an <h1>; taking the first <p> after the
        // content marker is what keeps "Check For Upgrade" out of the answer.
        assert!(!upgrade_status(UPGRADE).unwrap().contains("Check For"));
    }

    #[test]
    fn reads_the_diagnostics_table() {
        let pairs = diagnostics(DIAGNOSTICS);
        assert_eq!(pairs.len(), 5);
        assert_eq!(
            pairs[0],
            ("IP Address".to_owned(), "192.0.2.155".to_owned())
        );
        assert_eq!(pairs[2], ("BluOS Version".to_owned(), "4.16.6".to_owned()));
        assert_eq!(pairs[4].1, "2059");
        // The colon is presentation, and belongs to whoever draws the row.
        assert!(pairs.iter().all(|(label, _)| !label.ends_with(':')));
    }

    #[test]
    fn a_page_that_has_changed_yields_nothing_rather_than_rubbish() {
        // Both of these are the player's own hand-written HTML, so a firmware
        // update can move them. Nothing found has to read as nothing found.
        assert_eq!(
            upgrade_status("<html><body>Something else</body></html>"),
            None
        );
        assert_eq!(upgrade_status(""), None);
        assert!(diagnostics("<html>nothing here</html>").is_empty());
        // Truncated mid-table: keep what parsed, drop the rest.
        let cut = DIAGNOSTICS.split("BluOS Version").next().unwrap();
        assert_eq!(diagnostics(cut).len(), 2);
    }

    #[test]
    fn an_upgrade_offers_the_players_own_link_or_nothing() {
        // Nothing to install: no link, so nothing to press.
        assert_eq!(upgrade_action(UPGRADE), None);

        // What a page with an update is expected to look like. The href is the
        // player's own, not one this crate made up.
        let available = r#"<div data-role="page"><div data-role="content">
            <p>Update 4.18.2 available.</p>
            <a href="/upgrade?doit=1" data-role="button">Install</a>
            </div></div>"#;
        assert_eq!(
            upgrade_action(available),
            Some(("Install".to_owned(), "/upgrade?doit=1".to_owned()))
        );
        assert_eq!(
            upgrade_status(available).as_deref(),
            Some("Update 4.18.2 available.")
        );
    }

    #[test]
    fn strips_markup_and_collapses_space() {
        assert_eq!(strip_tags("  a   <b>bold</b>  b\n"), "a bold b");
        assert_eq!(strip_tags("<span/>"), "");
    }
}
