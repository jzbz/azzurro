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
//! them. That marking is the whole of the page's logic, so honouring it is the
//! whole of the work.
//!
//! Like [`crate::reports`] this is scraping rather than an API, and it degrades
//! to "no form here" rather than to an error, so a caller can still fall back
//! to opening the page.

use std::collections::BTreeMap;

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
                if let Some(option) = option.as_mut() {
                    option.label.push_str(text.trim());
                } else if let Some((_, words)) = label.as_mut() {
                    words.push_str(text.trim());
                }
            }

            Piece::Tag {
                name,
                closing,
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
                    // A self-closing tag hides nothing but itself.
                    if !matches!(name.as_str(), "input" | "img" | "br") {
                        hiding = Some((name, 0));
                    }
                    continue;
                }

                match (name.as_str(), closing) {
                    ("form", false) => {
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
                        let kind = attrs.get("type").map(String::as_str).unwrap_or("text");

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
                                match form.fields.iter_mut().find(|f| f.name == field) {
                                    Some(existing) => existing.choices.push(choice),
                                    None => form.fields.push(Field {
                                        name: field,
                                        kind: Kind::Choice,
                                        choices: vec![choice],
                                        ..Field::default()
                                    }),
                                }
                            }
                            "checkbox" => form.fields.push(Field {
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
                let end = rest.find('>').unwrap_or(rest.len() - 1);
                let raw = &rest[1..end];
                rest = &rest[(end + 1).min(rest.len())..];

                let closing = raw.starts_with('/');
                let raw = raw.trim_start_matches('/').trim_end_matches('/');
                let mut words = raw.splitn(2, |c: char| c.is_whitespace());
                let name = words.next().unwrap_or_default().to_ascii_lowercase();
                Some(Piece::Tag {
                    attrs: attributes(words.next().unwrap_or_default()),
                    name,
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
fn unescape(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

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
}
