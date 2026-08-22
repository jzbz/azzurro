//! The pages a player serves that are worth reading rather than opening.
//!
//! Some of what the official controller reaches for outside its own screens is
//! an interactive web page — signing into a music service asks for a password
//! and sometimes a captcha — and that belongs in a browser. The rest is a list:
//! the upgrade check answers one line of text, diagnostics is a table of facts,
//! the services page is twenty-five names each linking to its own sign-in form,
//! and the shares page is whatever is currently mounted. Those can be drawn in
//! the app instead of handing the user off to read five values or pick a name
//! from a list.
//!
//! This is scraping, and it is the only scraping in the crate. Every one of
//! these is hand-written HTML from the player's own web UI rather than an API,
//! so a firmware update could change any of them; everything here therefore
//! degrades to "nothing found" rather than to an error, and the caller can
//! still offer the page itself.

/// A track's technical details, as the label and value pairs the player prints.
///
/// `/Info?category=technical` answers an HTML table rather than a document with
/// a grammar: `<tr><td>Format:</td><td><small>FLAC 24/96</small></td></tr>`.
/// Five facts about a file, so it is worth reading rather than opening.
///
/// Note that plain `/Info` is a different thing entirely — a redirect out to
/// last.fm — and belongs in a browser.
pub fn technical_info(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for row in html.split("<tr>").skip(1) {
        let mut cells = row.split("<td").skip(1).map(|cell| {
            strip_tags(cell.split_once('>').map(|(_, text)| text).unwrap_or(cell))
                .trim_end_matches(':')
                .trim()
                .to_owned()
        });
        match (cells.next(), cells.next()) {
            // Two cells is a fact. One is the file name, which the player
            // writes across the whole width and which the row above already
            // names.
            (Some(label), Some(value)) if !label.is_empty() && !value.is_empty() => {
                out.push((label, value));
            }
            (Some(only), None) if !only.is_empty() => {
                if let Some((label, value)) = only.split_once(':') {
                    out.push((label.trim().to_owned(), value.trim().to_owned()));
                }
            }
            _ => {}
        }
    }
    out
}

/// What a configuration page says to the reader, if anything.
///
/// These pages carry a sentence above their form — what the service wants, or
/// what went wrong with the last attempt — in a `bs-header-text` block. Shown
/// as-is: the player is better placed than this app to explain its own pages,
/// and a wrong password is reported in the service's own words.
pub fn message(html: &str) -> String {
    // Only a visible one. The page keeps hidden blocks in the same class for
    // errors it is not reporting yet.
    for block in html.split("class=\"bs-header-text\"").skip(1) {
        let Some(text) = block.split_once('>').map(|(_, rest)| rest) else {
            continue;
        };
        let text = strip_tags(text.split("</div>").next().unwrap_or_default());
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

/// One music service the player can be signed into.
#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    /// The player's own name for it — `AmazonAlexa`, `TuneIn`.
    pub id: String,
    /// What to call it on screen: "Amazon Music", not `Amazon`.
    pub name: String,
    /// Where signing in happens. A form with a password on it, so this is a
    /// page to open rather than one to draw.
    pub href: String,
}

/// One network share the player is indexing.
#[derive(Debug, Clone, PartialEq)]
pub struct Share {
    /// The checkbox's name, which is the UNC path and what the remove form
    /// wants back.
    pub field: String,
    /// The sentence the player writes for it.
    pub label: String,
}

/// Every music service the player offers to sign into.
///
/// The page is a jQuery Mobile listview of `<li id=Service><a href=…>Name<span
/// class="bs-list-logo">…`. The id is the player's name for the service and the
/// text before the logo is the human one; they differ often enough to matter
/// — `Amazon` is "Amazon Music".
pub fn services(html: &str) -> Vec<Service> {
    let mut out = Vec::new();
    for row in html.split("<li id=").skip(1) {
        let Some(id) = row.split(['>', ' ']).next().filter(|id| !id.is_empty()) else {
            continue;
        };
        let Some(href) = between(row, "href=\"", "\"") else {
            continue;
        };
        // Everything between the anchor opening and the logo that follows it.
        let after = match row.find('>').map(|at| &row[at + 1..]) {
            Some(after) => after,
            None => continue,
        };
        let name = strip_tags(after.split("<span").next().unwrap_or_default());
        if name.is_empty() {
            continue;
        }
        out.push(Service {
            id: id.trim_matches('"').to_owned(),
            name,
            href: href.to_owned(),
        });
    }
    out
}

/// The shares the player is indexing, and where to post a change to them.
///
/// One checkbox per share, named for its UNC path, inside a form whose action
/// is where a removal goes. The label is the player's own sentence about it.
pub fn shares(html: &str) -> (Option<String>, Vec<Share>) {
    let action = between(html, "<form id=\"configshareform\"", ">")
        .and_then(|form| between(form, "action=\"", "\""))
        .map(str::to_owned);

    let mut out = Vec::new();
    for field in html.split("<input name=\"").skip(1) {
        let Some(name) = field.split('"').next().filter(|n| !n.is_empty()) else {
            continue;
        };
        // Only the checkboxes are shares; the submits carry a value instead.
        if !field
            .split('>')
            .next()
            .unwrap_or_default()
            .contains("checkbox")
        {
            continue;
        }
        let label = between(field, "<label", "</label>")
            .map(|label| strip_tags(label.split_once('>').map(|(_, t)| t).unwrap_or(label)))
            .unwrap_or_default();
        out.push(Share {
            field: name.to_owned(),
            label: if label.is_empty() {
                name.to_owned()
            } else {
                label
            },
        });
    }
    (action, out)
}

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

    /// `/services?noheader=1` from a real player, trimmed to two of its
    /// twenty-five rows. Note that the id and the name differ — `Amazon` is
    /// "Amazon Music" — which is why both are kept.
    const SERVICES: &str = r#"<ul data-role="listview">
        <li id=AmazonAlexa><a href="/credentials?service=AmazonAlexa&noheader=1&schemaVersion=35" data-transition="slide">Amazon Alexa <span class="bs-list-logo" data-inline="true"><img src="/Sources/images/AmazonAlexaIcon.png" class="bs-list-img"></span></a></li>
        <li id=Amazon><a href="/credentials?service=Amazon&noheader=1&schemaVersion=35" data-transition="slide">Amazon Music <span class="bs-list-logo" data-inline="true"><img src="/Sources/images/AmazonMusicIcon.png" class="bs-list-img"></span></a></li>
    </ul>"#;

    /// `/sharecfg?noheader=1` from a real player with one share mounted. The
    /// checkbox's name is the UNC path, backslashes and all.
    const SHARES: &str = r#"<form id="configshareform" method="POST" action="/findremoveshares?noheader=1" data-ajax="false">
        <fieldset data-role="controlgroup" data-type="vertical">
            <legend>Current music shares:</legend>
            <input name="\\10.0.0.100\media\music" id="checkbox1" type="checkbox" />
            <label for="checkbox1">media\music on 10.0.0.100 (\\10.0.0.100\media\music)</label>
        </fieldset>
        <input type="submit" value="Remove selected shares" name="remove">
        <input type="submit" value="Add shares" name="doaddshares" data-inline="true">
    </form>"#;

    #[test]
    fn reads_the_services_a_player_offers() {
        let found = services(SERVICES);
        assert_eq!(found.len(), 2);

        assert_eq!(found[0].id, "AmazonAlexa");
        assert_eq!(found[0].name, "Amazon Alexa");
        assert_eq!(
            found[0].href,
            "/credentials?service=AmazonAlexa&noheader=1&schemaVersion=35"
        );

        // The player's name for it and the one to show are not the same word.
        assert_eq!(found[1].id, "Amazon");
        assert_eq!(found[1].name, "Amazon Music");
    }

    #[test]
    fn reads_the_shares_a_player_is_indexing() {
        let (action, found) = shares(SHARES);

        assert_eq!(action.as_deref(), Some("/findremoveshares?noheader=1"));
        assert_eq!(found.len(), 1, "the submit buttons are not shares");
        assert_eq!(found[0].field, r"\\10.0.0.100\media\music");
        assert_eq!(
            found[0].label,
            r"media\music on 10.0.0.100 (\\10.0.0.100\media\music)"
        );
    }

    /// `/Info?category=technical` for one FLAC, trimmed of its stylesheet.
    const TECHNICAL: &str = r#"<br><table><head></head>
<tr><td valign="top" colspan="2">File: <small>/var/mnt/media/Prince - 1999.flac</small></td></tr>
<tr><td valign="top">Format:</td><td valign="bottom"><small>FLAC 24/96</small></td></tr>
<tr><td valign="top">Sample&nbsp;rate:</td><td valign="bottom"><small>96000</small></td></tr>
<tr><td valign="top">Channels:</td><td valign="bottom"><small>2</small></td></tr></table>"#;

    #[test]
    fn reads_a_track_technical_info() {
        let facts = technical_info(TECHNICAL);
        assert_eq!(facts.len(), 4);
        assert_eq!(facts[0].0, "File");
        assert_eq!(facts[1], ("Format".to_owned(), "FLAC 24/96".to_owned()));
        assert_eq!(facts[3], ("Channels".to_owned(), "2".to_owned()));
    }

    #[test]
    fn a_page_that_changed_shape_finds_nothing_rather_than_erroring() {
        assert!(services("<html><body>Not that page any more</body></html>").is_empty());
        let (action, found) = shares("<html><body></body></html>");
        assert!(action.is_none());
        assert!(found.is_empty());
    }

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
