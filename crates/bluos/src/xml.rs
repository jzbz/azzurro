//! The small pieces both XML parsers need.
//!
//! [`crate::screen`] and [`crate::settings`] read different grammars from the
//! same player and in the same way — a pull parser over `quick_xml`, a stack of
//! contexts, attributes pulled out by name. Three helpers fell out of that
//! shape identically in both, and sat duplicated in the two files, which is how
//! two copies of a thing quietly stop agreeing: the next fix to entity
//! handling, or to the schema attributes filtered out below, would have had to
//! be remembered twice.

use std::collections::BTreeMap;

use quick_xml::XmlVersion;
use quick_xml::events::BytesStart;

/// An element's name without whatever namespace prefix it carried.
///
/// Nothing observed on a player actually uses one. The comment this replaced
/// said element names "arrive namespaced in these documents (`xsi:…`)", which
/// is not so: every `<screen>`, `<settings>`, `<queue>` and `<contextMenu>`
/// fetched from a Powernode on BluOS 4.16.6 has bare element names, and the
/// only `xsi:` in any of them is on two attributes that [`attributes`] throws
/// away. The split stays anyway — it costs nothing on a name with no colon in
/// it, and a parser matching `"item"` would silently stop recognising the same
/// element the day a firmware did start writing `<ns:item>`.
pub(crate) fn local_name(raw: &[u8]) -> &str {
    // Borrowed from the reader's buffer rather than copied: every caller wants
    // a `&str` to match on and then drops it, so the owned `String` this used
    // to build was allocated and freed once per element for nothing.
    //
    // A name that is not UTF-8 reads as empty. The lossy conversion this
    // replaced could not be borrowed, and the two behave the same where it
    // matters: no branch in either parser matches a mangled name, so both an
    // empty string and a string full of replacement characters fall through to
    // "an element this does not know about", which is already the common case.
    let full = std::str::from_utf8(raw).unwrap_or_default();
    match full.split_once(':') {
        Some((_, local)) => local,
        None => full,
    }
}

/// Every attribute on an element, by name.
///
/// Owned rather than borrowed: most of these strings end up in the parsed
/// document itself — an item's `extra` map, an action's parameters — so the
/// copy is the model, not waste.
pub(crate) fn attributes(e: &BytesStart<'_>) -> BTreeMap<String, String> {
    e.attributes()
        .flatten()
        .filter_map(|attr| {
            let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
            // Drop the schema noise the player puts on every <screen>.
            if key.starts_with("xmlns") || key.starts_with("xsi:") {
                return None;
            }
            // These documents all declare XML 1.0, and quick-xml wants to be
            // told which rules to normalise entities under.
            let value = attr
                .normalized_value(XmlVersion::Explicit1_0)
                .ok()?
                .into_owned();
            Some((key, value))
        })
        .collect()
}

/// An attribute read as a boolean.
///
/// The player writes `"true"` in most places and `"1"` in a few, and an absent
/// attribute means false everywhere — so all three cases collapse here rather
/// than at each of the several dozen call sites.
pub(crate) fn flag(value: Option<String>) -> bool {
    matches!(value.as_deref(), Some("true" | "1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(raw: &str) -> BytesStart<'_> {
        BytesStart::from_content(raw, raw.find(' ').unwrap_or(raw.len()))
    }

    #[test]
    fn a_prefix_is_dropped_and_a_plain_name_is_kept() {
        assert_eq!(local_name(b"item"), "item");
        assert_eq!(local_name(b"ns:item"), "item");
        assert_eq!(local_name(b""), "");
        // Not UTF-8, so not a name any branch can match.
        assert_eq!(local_name(&[b'i', 0xff, b'm']), "");
    }

    #[test]
    fn schema_attributes_are_not_part_of_the_document() {
        let e = element(
            r#"screen xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="screen.xsd" screenTitle="Home""#,
        );
        let attrs = attributes(&e);
        assert_eq!(attrs.get("screenTitle").map(String::as_str), Some("Home"));
        assert_eq!(attrs.len(), 1, "schema noise survived: {attrs:?}");
    }

    #[test]
    fn entities_in_an_attribute_are_normalised() {
        let e = element(r#"action URI="/Play?a=1&amp;b=2""#);
        assert_eq!(
            attributes(&e).get("URI").map(String::as_str),
            Some("/Play?a=1&b=2")
        );
    }

    #[test]
    fn both_spellings_of_true_and_nothing_at_all() {
        assert!(flag(Some("true".to_owned())));
        assert!(flag(Some("1".to_owned())));
        assert!(!flag(Some("false".to_owned())));
        assert!(!flag(Some(String::new())));
        assert!(!flag(None));
    }
}
