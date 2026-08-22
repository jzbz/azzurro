//! Scraping helpers shared by the two HTML readers.
//!
//! [`crate::forms`] reads the player's configuration pages and
//! [`crate::reports`] reads its status pages. Both are scraping the same
//! generated HTML, so both need the same entity decoding — and it has to be
//! the same decoding, or a label and the value beside it disagree about what
//! the page said.

/// Turn the entities these pages use back into the characters they stand for.
///
/// One left-to-right pass rather than a chain of `replace` calls. A chain
/// re-reads its own output: `&amp;lt;` — an escaped ampersand followed by the
/// literal text `lt;` — became `&lt;` after the first replacement and then `<`
/// after the fourth, so a share named `a&lt;b` came back as `a<b` and was
/// posted to the player wrong. Scanning once cannot do that, because what has
/// been written is never looked at again.
pub(crate) fn unescape(raw: &str) -> String {
    const ENTITIES: [(&str, char); 6] = [
        ("&amp;", '&'),
        ("&quot;", '"'),
        ("&#39;", '\''),
        ("&lt;", '<'),
        ("&gt;", '>'),
        // A real non-breaking space, not a plain one. Callers that collapse
        // whitespace turn it into a plain space themselves, because Unicode
        // counts U+00A0 as whitespace; callers that do not get what the page
        // actually meant.
        ("&nbsp;", '\u{a0}'),
    ];

    let Some(first) = raw.find('&') else {
        return raw.to_owned();
    };

    let mut out = String::with_capacity(raw.len());
    out.push_str(&raw[..first]);
    let mut rest = &raw[first..];

    while !rest.is_empty() {
        match ENTITIES.iter().find(|(entity, _)| rest.starts_with(entity)) {
            Some((entity, decoded)) => {
                out.push(*decoded);
                rest = &rest[entity.len()..];
            }
            // An `&` that begins nothing this knows — a bare ampersand, or an
            // entity these templates do not emit. Kept as it is rather than
            // dropped, and stepped over so it cannot match again.
            None => {
                let mut chars = rest.chars();
                out.extend(chars.next());
                rest = chars.as_str();
            }
        }

        match rest.find('&') {
            Some(at) => {
                out.push_str(&rest[..at]);
                rest = &rest[at..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }

    out
}
