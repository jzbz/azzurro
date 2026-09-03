//! The forms on the player's own web pages, read so they can be drawn.
//!
//! Three of the things a controller has to offer live only here: joining a
//! wireless network, signing into a music service, and mounting a network
//! share. All three are plain server-rendered forms on port 80 — no JavaScript
//! decides what they contain — which is what makes drawing them natively
//! honest rather than a guess.
//!
//! The player marks what does not apply. Qobuz's sign-in page leaves a username
//! and a password visible and hides its captcha and its Logout button, because
//! nobody is signed in yet; TuneIn's hides all three, because it wants none of
//! them. That marking is the whole of the page's logic, so honoring it is the
//! whole of the work.
//!
//! Like [`crate::reports`] this is scraping rather than an API, and it degrades
//! to "no form here" rather than to an error, so a caller can still fall back
//! to opening the page.

use std::collections::BTreeMap;

use crate::html::unescape;

/// A form on one of those pages.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Form {
    /// Where the filled-in form goes.
    pub action: String,
    /// `POST` unless the page says otherwise. A few of these are `GET`.
    pub post: bool,
    /// Everything to draw, in the order the page has them.
    pub fields: Vec<Field>,
    /// What the page carries but never shows — a service name, a schema
    /// version — which has to be sent back untouched.
    pub hidden: BTreeMap<String, String>,
    /// The buttons at the end. More than one, usually: Login and Logout.
    pub submits: Vec<Submit>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Field {
    pub name: String,
    /// The element's `id`, which is what a `<label for=…>` points at and is
    /// not always the same word as `name`.
    pub id: String,
    pub kind: Kind,
    /// The page's own words for it, from the `<label>` beside it.
    pub label: String,
    pub value: String,
    pub placeholder: String,
    /// For a choice: what there is to choose from.
    pub choices: Vec<Choice>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Text,
    /// Never echoed back and never logged. Drawn masked.
    Password,
    /// One of a list, from a `<select>` or a set of radio buttons.
    Choice,
    Switch,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Choice {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Submit {
    /// Sent as a field of its own, which is how the page tells Login from
    /// Logout: both post the same form and only the name differs.
    pub name: String,
    pub label: String,
}

/// Read every form on a page.
///
/// Anything the player has marked hidden is left out, fields and buttons
/// alike — see the note at the top of this module for why that marking is
/// trustworthy.
pub fn parse(html: &str) -> Vec<Form> {
    let mut forms: Vec<Form> = Vec::new();
    let mut open: Option<Form> = None;
    // Field name to its index in `open`'s fields. Radios sharing a name are
    // one choice, and finding the earlier one used to be a scan of everything
    // collected so far — once per input, so quadratic in a page's field count.
    let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
    // The tag that started the hidden region, and how deep inside it we are.
    let mut hiding: Option<(String, usize)> = None;
    // A `<label>` is written before the input it names, and its `for` is on
    // the opening tag while its words arrive after it, so both wait here.
    let mut label: Option<(String, String)> = None;
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    // The `<select>` being filled in, if one is open.
    let mut choosing: Option<Field> = None;
    let mut option: Option<Choice> = None;

    for piece in pieces(html) {
        match piece {
            Piece::Text(text) => {
                // Decoded here as well as in `attributes`. Without it a label
                // and a value read from the same page disagreed: the value of
                // an `<option>` came back decoded and the words between its
                // tags did not, so a network really called `AT&T` offered
                // itself in the list as `AT&amp;T`.
                let text = unescape(text.trim());
                if let Some(option) = option.as_mut() {
                    option.label.push_str(&text);
                } else if let Some((_, words)) = label.as_mut() {
                    words.push_str(&text);
                }
            }

            Piece::Tag {
                name,
                closing,
                self_closing,
                attrs,
            } => {
                // Track the hidden region first: everything inside one is
                // skipped, including the tags that would open another.
                if let Some((tag, depth)) = hiding.as_mut() {
                    if &name == tag {
                        if closing {
                            if *depth == 0 {
                                hiding = None;
                            } else {
                                *depth -= 1;
                            }
                        } else {
                            *depth += 1;
                        }
                    }
                    continue;
                }
                if !closing && is_hidden(&attrs) {
                    // A tag that hides nothing but itself: one that closed
                    // itself, or one of the void elements that never had a
                    // closing tag to wait for. Anything else opens a region
                    // that runs to its own `</…>`.
                    if !self_closing && !matches!(name.as_str(), "input" | "img" | "br") {
                        hiding = Some((name, 0));
                    }
                    continue;
                }

                match (name.as_str(), closing) {
                    ("form", false) => {
                        by_name.clear();
                        open = Some(Form {
                            action: attrs.get("action").cloned().unwrap_or_default(),
                            post: attrs
                                .get("method")
                                .map(|m| m.eq_ignore_ascii_case("post"))
                                .unwrap_or(true),
                            ..Form::default()
                        });
                    }
                    ("form", true) => {
                        if let Some(mut form) = open.take() {
                            name_the_fields(&mut form, &labels);
                            forms.push(form);
                        }
                    }

                    ("label", false) => {
                        label =
                            Some((attrs.get("for").cloned().unwrap_or_default(), String::new()));
                    }
                    ("label", true) => {
                        if let Some((target, words)) = label.take()
                            && !target.is_empty()
                        {
                            labels.insert(target, tidy(&words));
                        }
                    }

                    ("select", false) => {
                        choosing = Some(Field {
                            name: attrs.get("name").cloned().unwrap_or_default(),
                            id: attrs.get("id").cloned().unwrap_or_default(),
                            kind: Kind::Choice,
                            ..Field::default()
                        });
                    }
                    ("select", true) => {
                        if let (Some(field), Some(form)) = (choosing.take(), open.as_mut())
                            && !field.name.is_empty()
                        {
                            form.fields.push(field);
                        }
                    }

                    ("option", false) => {
                        option = Some(Choice {
                            value: attrs.get("value").cloned().unwrap_or_default(),
                            selected: attrs.contains_key("selected"),
                            ..Choice::default()
                        });
                    }
                    ("option", true) => {
                        if let (Some(mut choice), Some(field)) = (option.take(), choosing.as_mut())
                        {
                            choice.label = tidy(&choice.label);
                            if choice.label.is_empty() {
                                choice.label.clone_from(&choice.value);
                            }
                            field.choices.push(choice);
                        }
                    }

                    ("input", _) => {
                        let Some(form) = open.as_mut() else { continue };
                        let value = attrs.get("value").cloned().unwrap_or_default();
                        // Folded here rather than compared case-sensitively
                        // below: an input's type is case-insensitive in HTML,
                        // and `type="Password"` classified as Kind::Text drew
                        // the secret with a text control and rendered the held
                        // value back into it.
                        let kind = attrs
                            .get("type")
                            .map(|t| t.to_ascii_lowercase())
                            .unwrap_or_else(|| "text".to_owned());
                        let kind = kind.as_str();

                        // A submit is the one input that does not need a name.
                        // The wireless form's Update button has none, because
                        // it is the only thing that form can do; a page with
                        // two buttons names them to tell them apart.
                        if kind == "submit" {
                            form.submits.push(Submit {
                                name: attrs.get("name").cloned().unwrap_or_default(),
                                label: if value.is_empty() {
                                    "Submit".to_owned()
                                } else {
                                    value
                                },
                            });
                            continue;
                        }

                        let Some(field) = attrs.get("name").cloned() else {
                            continue;
                        };

                        match kind {
                            "hidden" => {
                                form.hidden.insert(field, value);
                            }
                            "radio" => {
                                let choice = Choice {
                                    label: attrs
                                        .get("id")
                                        .and_then(|id| labels.get(id))
                                        .cloned()
                                        .unwrap_or_else(|| value.clone()),
                                    selected: attrs.contains_key("checked"),
                                    value,
                                };
                                // Radios sharing a name are one choice, so the
                                // second one joins the first rather than
                                // starting another field.
                                match by_name.get(&field) {
                                    Some(at) => form.fields[*at].choices.push(choice),
                                    None => {
                                        by_name.insert(field.clone(), form.fields.len());
                                        form.fields.push(Field {
                                            name: field,
                                            kind: Kind::Choice,
                                            choices: vec![choice],
                                            ..Field::default()
                                        });
                                    }
                                }
                            }
                            "checkbox" => form.fields.push(Field {
                                id: attrs.get("id").cloned().unwrap_or_default(),
                                name: field,
                                kind: Kind::Switch,
                                value: if attrs.contains_key("checked") {
                                    "on".to_owned()
                                } else {
                                    String::new()
                                },
                                ..Field::default()
                            }),
                            other => form.fields.push(Field {
                                id: attrs.get("id").cloned().unwrap_or_default(),
                                name: field,
                                kind: if other == "password" {
                                    Kind::Password
                                } else {
                                    Kind::Text
                                },
                                placeholder: attrs.get("placeholder").cloned().unwrap_or_default(),
                                label: attrs
                                    .get("id")
                                    .and_then(|id| labels.get(id))
                                    .cloned()
                                    .unwrap_or_default(),
                                value,
                                ..Field::default()
                            }),
                        }
                    }

                    _ => {}
                }
            }
        }
    }

    forms
}

/// Give every field the label the page wrote for it.
///
/// Labels are matched by the input's `id`, which is not its `name` — a page
/// writes `<label for="passtext">` above `<input id="passtext" name="password">`
/// — and some of them appear after the field they name.
fn name_the_fields(form: &mut Form, labels: &BTreeMap<String, String>) {
    for field in &mut form.fields {
        // By id first, because that is what a label points at.
        if field.label.is_empty()
            && let Some(found) = labels.get(&field.id).or_else(|| labels.get(&field.name))
        {
            field.label.clone_from(found);
        }
        if field.label.is_empty() {
            field.label.clone_from(&field.placeholder);
        }
        // Last resort, the field's own name: `<select name="quality">` on the
        // Qobuz page carries neither a label nor a placeholder. Capitalized,
        // because a name written for a form post is lower case by convention
        // and reads as a mistake beside "Username" and "Password".
        if field.label.is_empty() {
            field.label = capitalize(&field.name);
        }
    }
}

/// The first character upper-cased, the rest left exactly as it is — a name
/// that is already capitalized, or one like `ipAddress` with a capital inside
/// it, must come through unharmed.
fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Whether the player has marked this element as not applying.
fn is_hidden(attrs: &BTreeMap<String, String>) -> bool {
    attrs.contains_key("hidden")
        || attrs
            .get("style")
            .is_some_and(|style| style.replace(' ', "").contains("display:none"))
}

/// Trailing colons and collapsed whitespace: the pages write "Password:".
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut gap = false;
    for c in text.chars() {
        if c.is_whitespace() {
            gap = !out.is_empty();
        } else {
            if gap {
                out.push(' ');
            }
            gap = false;
            out.push(c);
        }
    }
    out.trim_end_matches(':').trim().to_owned()
}

enum Piece<'a> {
    Text(&'a str),
    Tag {
        name: String,
        closing: bool,
        /// Whether the tag closed itself: `<br/>`, not `</br>`.
        ///
        /// Kept apart from `closing`, which means a leading slash. A tag that
        /// closes itself opens nothing, so it cannot start a region either —
        /// which is what `<div hidden/>` used to do, swallowing the rest of
        /// the page because no `</div>` was ever coming.
        self_closing: bool,
        attrs: BTreeMap<String, String>,
    },
}

/// Walk a page as a flat run of tags and the text between them.
///
/// Not an HTML parser: it does not build a tree and does not need to. These
/// pages are generated by one program from one template, and the only structure
/// that matters is the order things appear in.
fn pieces(html: &str) -> impl Iterator<Item = Piece<'_>> {
    let mut rest = html;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        match rest.find('<') {
            Some(0) => {
                // `rest.len()` and not `len() - 1`: a tag the page never
                // closed runs to the end of the input, and on a page whose
                // last byte is a bare `<` the subtraction made this `0`, so
                // `&rest[1..0]` sliced backwards and panicked. The player's own
                // web UI is the input here, so a page truncated mid-tag took
                // the scrape down instead of degrading to "no form here" the
                // way this module promises to.
                //
                // `end` cannot be below 1: this arm only runs when `rest`
                // starts with `<`, so any `>` is at index 1 or later, and the
                // fallback is a length that is at least 1 for the same reason.
                let end = rest.find('>').unwrap_or(rest.len());
                let raw = &rest[1..end];
                rest = &rest[(end + 1).min(rest.len())..];

                let closing = raw.starts_with('/');
                let self_closing = !closing && raw.ends_with('/');
                let raw = raw.trim_start_matches('/').trim_end_matches('/');
                let mut words = raw.splitn(2, |c: char| c.is_whitespace());
                let name = words.next().unwrap_or_default().to_ascii_lowercase();
                Some(Piece::Tag {
                    attrs: attributes(words.next().unwrap_or_default()),
                    name,
                    self_closing,
                    closing,
                })
            }
            Some(at) => {
                let text = &rest[..at];
                rest = &rest[at..];
                Some(Piece::Text(text))
            }
            None => {
                let text = rest;
                rest = "";
                Some(Piece::Text(text))
            }
        }
    })
}

/// `name="value"`, `name='value'` and bare `name`, which is how `hidden`,
/// `checked` and `selected` are written.
fn attributes(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = raw.trim();

    while !rest.is_empty() {
        let end = rest
            .find(['=', ' ', '\t', '\n', '\r'])
            .unwrap_or(rest.len());
        let key = rest[..end].trim().to_ascii_lowercase();
        rest = rest[end..].trim_start();

        let value = if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            let (quote, after) = match after.strip_prefix('"') {
                Some(after) => ('"', after),
                None => match after.strip_prefix('\'') {
                    Some(after) => ('\'', after),
                    None => (' ', after),
                },
            };
            let stop = after.find(quote).unwrap_or(after.len());
            let value = &after[..stop];
            rest = after[(stop + 1).min(after.len())..].trim_start();
            unescape(value)
        } else {
            String::new()
        };

        if !key.is_empty() {
            out.insert(key, value);
        }
    }
    out
}

/// The five entities these pages use. Anything else is left alone rather than
/// guessed at.
#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of `/credentials?service=Qobuz`, trimmed. Username, password
    /// and the quality choice are offered; the captcha and Logout are marked
    /// hidden because nobody is signed in yet.
    const CREDENTIALS: &str = r#"
<form id="credentialsForm" action="/credentials" method="POST">
    <input type="hidden" name="noheader" value="1">
    <input type="hidden" name="schemaVersion" value="35">
    <div data-role="fieldcontain">
        <label for="usertext" id="userlabel">Username:</label>
        <input id="usertext" name="user" type="text" value="" placeholder="Username"/>
    </div>
    <div id="passdiv" data-role="fieldcontain">
        <label for="passtext" id="passlabel">Password:</label>
        <input id="passtext" name="password" type="password" value="" placeholder="Password"/>
    </div>
    <span hidden>
        <div id="captchadiv">
            <label for="captchaguess">Enter characters:</label>
            <input id="captchaguess" name="captchaguess" type="text" placeholder="Enter characters"/>
            <input type="hidden" name="captchaid" value="">
        </div>
    </span>
    <fieldset>
        <label for="q0">MP3</label>
        <input type="radio" name="quality" id="q0" value="MP3"/>
        <label for="q1">CD</label>
        <input type="radio" name="quality" id="q1" value="CD" checked/>
    </fieldset>
    <input type="hidden" name="service" value="Qobuz">
    <input type="submit" name="login" value="Login"/>
    <span hidden>
        <input type="submit" name="logoutAction" value="Logout"/>
    </span>
</form>"#;

    /// An input's type is case-insensitive in HTML. Classified as text, a
    /// password field is drawn with a text control and the held value is
    /// rendered back into it.
    #[test]
    fn a_password_is_a_password_however_it_is_spelled() {
        for spelling in ["password", "Password", "PASSWORD", "PaSsWoRd"] {
            let html = format!(
                r#"<form action="/x"><input name="pw" type="{spelling}" value=""/></form>"#
            );
            let forms = parse(&html);
            assert_eq!(
                forms[0].fields[0].kind,
                Kind::Password,
                "type={spelling:?} should still be a password"
            );
        }

        let forms = parse(r#"<form action="/x"><input name="t" type="TEXT"/></form>"#);
        assert_eq!(forms[0].fields[0].kind, Kind::Text, "and text stays text");
    }

    /// A tag that closes itself has no `</…>` coming, so treating it as the
    /// start of a hidden region swallowed the rest of the page.
    #[test]
    fn a_self_closing_hidden_tag_hides_only_itself() {
        let forms = parse(
            r#"<form action="/x">
                 <div hidden/>
                 <input name="after" type="text" value="v"/>
               </form>"#,
        );

        assert_eq!(
            forms[0]
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["after"],
            "the field after a self-closed hidden div survives"
        );

        let paired = parse(
            r#"<form action="/x">
                 <div hidden><input name="inside" type="text"/></div>
                 <input name="outside" type="text"/>
               </form>"#,
        );
        assert_eq!(
            paired[0]
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["outside"],
            "a properly paired hidden region still hides what is in it"
        );
    }

    #[test]
    fn reads_a_sign_in_form() {
        let forms = parse(CREDENTIALS);
        assert_eq!(forms.len(), 1);
        let form = &forms[0];

        assert_eq!(form.action, "/credentials");
        assert!(form.post);

        // What the page carries but never shows has to go back untouched.
        assert_eq!(
            form.hidden.get("service").map(String::as_str),
            Some("Qobuz")
        );
        assert_eq!(
            form.hidden.get("schemaVersion").map(String::as_str),
            Some("35")
        );
        // Inside a hidden region, so not even as a hidden value.
        assert!(!form.hidden.contains_key("captchaid"));

        let names: Vec<&str> = form.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["user", "password", "quality"],
            "the captcha is hidden, so it is not offered"
        );

        assert_eq!(form.fields[0].label, "Username");
        assert_eq!(form.fields[1].kind, Kind::Password);
        assert_eq!(form.fields[1].label, "Password");

        // Radios sharing a name are one choice between them.
        let quality = &form.fields[2];
        assert_eq!(quality.kind, Kind::Choice);
        assert_eq!(quality.choices.len(), 2);
        assert_eq!(quality.choices[0].label, "MP3");
        assert!(quality.choices[1].selected, "CD is the one checked");

        // Logout is hidden; only Login is offered.
        assert_eq!(form.submits.len(), 1);
        assert_eq!(form.submits[0].name, "login");
        assert_eq!(form.submits[0].label, "Login");
    }

    /// The shape of `/wificfg`, trimmed to three networks. The value carries
    /// the encryption after a tab, which is the player's business and goes back
    /// exactly as it came.
    const WIFI: &str = "
<form id=\"nodeNameForm\" action=\"/wificfg\" method=\"POST\">
    <input name=\"noheader\" type=\"hidden\" value=\"1\"/>
    <label for=\"essid\">Configure Wireless:</label>
    <select id=\"essid\" name=\"essid\">
        <option value=\"one\tWPA3\">one</option>
        <option value=\"two\tWPA2\" selected>two</option>
        <option value=\"private\">Other...</option>
    </select>
    <label for=\"keytext\" id=\"keylabel\">Enter password or key (if protected):</label>
    <input id=\"keytext\" name=\"key\" type=\"password\"/>
    <input type=\"submit\" name=\"update\" value=\"Update\"/>
</form>";

    #[test]
    fn a_nameless_submit_is_still_a_submit() {
        let forms = parse(
            r#"<form action="/wificfg" method="POST"><input type="submit" value="Update"/></form>"#,
        );
        assert_eq!(forms[0].submits.len(), 1);
        assert_eq!(forms[0].submits[0].label, "Update");
        assert!(forms[0].submits[0].name.is_empty());
    }

    #[test]
    fn reads_the_wireless_form() {
        let forms = parse(WIFI);
        assert_eq!(forms.len(), 1);
        let form = &forms[0];

        assert_eq!(form.action, "/wificfg");
        let networks = &form.fields[0];
        assert_eq!(networks.name, "essid");
        assert_eq!(networks.id, "essid");
        assert_eq!(networks.kind, Kind::Choice);
        assert_eq!(networks.label, "Configure Wireless");
        assert_eq!(networks.choices.len(), 3);
        assert_eq!(networks.choices[0].value, "one\tWPA3");
        assert!(networks.choices[1].selected);

        let key = &form.fields[1];
        assert_eq!(key.kind, Kind::Password);
        assert_eq!(key.label, "Enter password or key (if protected)");
    }

    #[test]
    fn a_page_with_no_form_reads_as_no_forms() {
        assert!(parse("<html><body><p>Nothing here</p></body></html>").is_empty());
        assert!(parse("").is_empty());
    }
    #[test]
    fn a_page_truncated_mid_tag_degrades_instead_of_panicking() {
        // Every one of these ends inside a tag the page never closed. The
        // contract is "no form here", not a panic.
        for page in [
            "<",
            "x<",
            "<form></form>x<",
            "<form><input name=\"a\"",
            "<<<",
        ] {
            let forms = parse(page);
            assert!(
                forms.iter().all(|f| f.fields.is_empty()),
                "{page:?} produced a form with fields"
            );
        }
    }

    #[test]
    fn entities_decode_once_and_only_once() {
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape("a &amp; b"), "a & b");
        assert_eq!(unescape("&lt;tag&gt;"), "<tag>");
        assert_eq!(unescape("say &quot;hi&quot;"), "say \"hi\"");
        assert_eq!(unescape("it&#39;s"), "it's");

        // The ordering bug: chained replaces turned an escaped ampersand and
        // the letters after it into a second entity.
        assert_eq!(unescape("a&amp;lt;b"), "a&lt;b");
        assert_eq!(unescape("&amp;amp;"), "&amp;");

        // Ampersands that begin nothing are left alone rather than eaten.
        assert_eq!(unescape("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(unescape("&"), "&");
        // A real non-breaking space. Callers that collapse whitespace fold it
        // into an ordinary one; callers that do not keep what the page meant.
        assert_eq!(unescape("&nbsp;"), "\u{a0}");
        assert_eq!(unescape("Sample&nbsp;rate"), "Sample\u{a0}rate");
        // Still left alone: an entity these pages do not emit.
        assert_eq!(unescape("&copy;"), "&copy;");
        assert_eq!(unescape("trailing &"), "trailing &");

        // Multi-byte input must survive being stepped over a byte at a time.
        assert_eq!(unescape("café & bar"), "café & bar");
    }

    #[test]
    fn a_label_is_decoded_the_same_as_a_value() {
        let page = r#"
<form action="/x" method="POST">
  <select name="carrier">
    <option value="AT&amp;T">AT&amp;T</option>
  </select>
</form>"#;
        let form = parse(page).pop().expect("a form");
        let field = form.fields.first().expect("a field");
        let choice = field.choices.first().expect("a choice");
        assert_eq!(choice.value, "AT&T");
        assert_eq!(choice.label, "AT&T", "the label kept its entity");
    }
    #[test]
    fn a_field_with_no_label_of_its_own_is_named_after_itself() {
        // Qobuz's quality picker: a bare `<select>` with no label and no
        // placeholder anywhere near it.
        let page = r#"
<form action="/credentials" method="POST">
  <select name="quality">
    <option value="6">CD</option>
    <option value="27">Hi-Res</option>
  </select>
</form>"#;
        let form = parse(page).pop().expect("a form");
        assert_eq!(form.fields[0].label, "Quality");
    }

    #[test]
    fn capitalizing_leaves_the_rest_of_a_name_alone() {
        assert_eq!(capitalize("quality"), "Quality");
        assert_eq!(capitalize("Quality"), "Quality");
        assert_eq!(capitalize("ipAddress"), "IpAddress");
        assert_eq!(capitalize(""), "");
    }
}
