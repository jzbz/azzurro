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
    const ENTITIES: [(&str, char); 5] = [
        ("&amp;", '&'),
        ("&quot;", '"'),
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
        if let Some((entity, decoded)) =
            ENTITIES.iter().find(|(entity, _)| rest.starts_with(entity))
        {
            out.push(*decoded);
            rest = &rest[entity.len()..];
        } else if let Some((reference, decoded)) = numeric(rest) {
            out.push_str(&decoded);
            rest = &rest[reference.len()..];
        } else {
            // An `&` that begins nothing this knows — a bare ampersand, or an
            // entity these templates do not emit. Kept as it is rather than
            // dropped, and stepped over so it cannot match again.
            let mut chars = rest.chars();
            out.extend(chars.next());
            rest = chars.as_str();
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

/// A numeric reference at the start of `rest` — `&#34;` or `&#x22;` — and what
/// it stands for.
///
/// Numbers rather than names because that is what Go's `html/template` writes
/// for most of what it escapes: a double quote as `&#34;`, a plus as `&#43;`.
/// Only `&#39;` used to be known, so a network called `My "Den"` came back as
/// `My &#34;Den&#34;` and was posted to the player with the entity in it.
///
/// Decoded by [`crate::xml::entity`], which already reads both spellings for
/// the XML documents, so the two readers cannot disagree about a number. A
/// reference that does not name a character — a surrogate, a number past
/// U+10FFFF, digits that do not parse — comes back from there as written, and
/// is then treated like any other `&` this does not know: kept.
fn numeric(rest: &str) -> Option<(&str, String)> {
    let digits = rest.strip_prefix("&#")?;
    let len = digits
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(digits.len());
    if !digits[len..].starts_with(';') {
        return None;
    }
    // `&#…;` whole, and the name `entity` wants, which is the part between the
    // ampersand and the semicolon.
    let reference = &rest[..len + 3];
    let decoded = crate::xml::entity(&reference[1..reference.len() - 1]);
    (decoded != reference).then_some((reference, decoded))
}
