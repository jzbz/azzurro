//! The question the player asks before it does something you cannot undo.
//!
//! Some actions do not simply happen. Playing a track when the queue already
//! holds something answers 200 with this instead of doing anything:
//!
//! ```xml
//! <dialog title="Playing replaces Play Queue"
//!         body="Playing this will clear your existing queue.">
//!   <button text="Replace" textColor="#FF3B30">
//!     <action type="player-link" URI="/Add?playnow=1&amp;file=…" haptic="true"/>
//!   </button>
//!   <button text="Cancel"><action type="nil"/></button>
//!   <closeAction type="nil"/>
//! </dialog>
//! ```
//!
//! The reply is the whole of the player's answer — the action it was asked to
//! run has *not* run. Whoever throws the body away has told the user nothing
//! and done nothing, which is exactly how pressing a track came to do nothing
//! at all once there was a queue to replace.
//!
//! The wording, the colour of the dangerous button and which action each
//! button carries are all the player's to decide, the same as everywhere else
//! in this crate. Nothing here invents a phrase.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::error::{Error, Result};
use crate::screen::{Action, action as action_from};
use crate::xml::{attributes, local_name};

/// One of the answers on offer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Choice {
    /// What the button says. The player's wording.
    pub text: String,
    /// `#RRGGBB` where the player marks one out as the dangerous one, which it
    /// does for the button that discards something.
    pub color: Option<String>,
    /// What pressing it does. `None`, or an action of type `nil`, means it
    /// only dismisses.
    pub action: Option<Action>,
}

impl Choice {
    /// Whether pressing this does anything beyond closing the question.
    pub fn is_cancel(&self) -> bool {
        match &self.action {
            None => true,
            Some(action) => action.uri.as_deref().unwrap_or_default().is_empty(),
        }
    }
}

/// A question, and the answers the player will accept.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dialog {
    pub title: Option<String>,
    pub body: Option<String>,
    pub choices: Vec<Choice>,
    /// What dismissing it should run, where the player names one. Usually
    /// nothing.
    pub close: Option<Action>,
}

/// Read a reply as a dialog, or say it is not one.
///
/// Returns `Ok(None)` for anything whose root is not `<dialog>`, which is the
/// ordinary case: most actions just happen and answer with something else.
pub fn parse(xml: &str) -> Result<Option<Dialog>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut dialog: Option<Dialog> = None;
    let mut button: Option<Choice> = None;

    loop {
        // `Start` and `Empty` carry the same attributes and differ only in
        // whether an `End` follows, so they are read together and the
        // difference is kept in `closed`: a `<button/>` with nothing inside it
        // has to be filed here, because no `End` will come to do it.
        let (e, closed) = match reader.read_event() {
            Err(e) => return Err(Error::Screen(format!("dialog: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => (e, false),
            Ok(Event::Empty(e)) => (e, true),
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == "button"
                    && let (Some(done), Some(dialog)) = (button.take(), dialog.as_mut())
                {
                    dialog.choices.push(done);
                }
                continue;
            }
            Ok(_) => continue,
        };

        let qname = e.name();
        let name = local_name(qname.as_ref());
        let mut a = attributes(&e);

        match name {
            "dialog" => {
                dialog = Some(Dialog {
                    title: a.remove("title"),
                    body: a.remove("body"),
                    ..Dialog::default()
                });
            }
            // Anything before a `<dialog>` means the reply is something else.
            _ if dialog.is_none() => return Ok(None),
            "button" => {
                let choice = Choice {
                    text: a.remove("text").unwrap_or_default(),
                    color: a.remove("textColor"),
                    action: None,
                };
                match (closed, dialog.as_mut()) {
                    (true, Some(dialog)) => dialog.choices.push(choice),
                    _ => button = Some(choice),
                }
            }
            // Innermost wins, the same rule the screen parser follows: inside
            // a button it is that button's, otherwise it is the dialog's.
            "action" => {
                let built = action_from(name, a);
                match (button.as_mut(), dialog.as_mut()) {
                    (Some(button), _) => button.action = Some(built),
                    (None, Some(dialog)) => dialog.close = Some(built),
                    (None, None) => {}
                }
            }
            "closeAction" => {
                if let Some(dialog) = dialog.as_mut() {
                    dialog.close = Some(action_from("action", a));
                }
            }
            _ => {}
        }
    }

    Ok(dialog)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reply a Powernode on BluOS 4.16.6 gives when playing a track would
    /// discard a queue that has something in it, captured verbatim.
    const REPLACE_QUEUE: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<dialog title="Playing replaces Play Queue" body="Playing this will clear your existing queue.">
  <button text="Replace" textColor="#FF3B30">
    <action type="player-link" URI="/Add?playnow=1&amp;file=%2Fvar%2Fmnt%2Fx.flac" haptic="true"></action>
  </button>
  <button text="Cancel">
    <action type="nil"></action>
  </button>
  <closeAction type="nil"></closeAction>
</dialog>"##;

    #[test]
    fn the_question_and_both_answers() {
        let dialog = parse(REPLACE_QUEUE).expect("parses").expect("is a dialog");

        assert_eq!(dialog.title.as_deref(), Some("Playing replaces Play Queue"));
        assert_eq!(
            dialog.body.as_deref(),
            Some("Playing this will clear your existing queue.")
        );
        assert_eq!(dialog.choices.len(), 2);

        let replace = &dialog.choices[0];
        assert_eq!(replace.text, "Replace");
        assert_eq!(replace.color.as_deref(), Some("#FF3B30"));
        assert!(!replace.is_cancel(), "Replace carries the action to run");
        assert_eq!(
            replace.action.as_ref().and_then(|a| a.uri.as_deref()),
            Some("/Add?playnow=1&file=%2Fvar%2Fmnt%2Fx.flac"),
            "the entity in the URI is resolved, and the action is the button's"
        );

        let cancel = &dialog.choices[1];
        assert_eq!(cancel.text, "Cancel");
        assert!(cancel.is_cancel(), "a nil action only dismisses");
    }

    /// The ordinary case: an action that simply happened.
    #[test]
    fn a_reply_that_is_not_a_dialog_is_not_one() {
        assert_eq!(parse("<status/>").expect("parses"), None);
        assert_eq!(parse("").expect("parses"), None);
        assert_eq!(
            parse(r#"<?xml version="1.0"?><screen screenTitle="Home"/>"#).expect("parses"),
            None
        );
    }

    /// A question with no buttons is still a question, and must not be read as
    /// "not a dialog" — that would put us back to doing nothing silently.
    #[test]
    fn a_dialog_with_nothing_to_press_is_still_a_dialog() {
        let dialog = parse(r#"<dialog title="Wait" body="Hold on"/>"#)
            .expect("parses")
            .expect("is a dialog");
        assert_eq!(dialog.title.as_deref(), Some("Wait"));
        assert!(dialog.choices.is_empty());
    }
}
