//! Firmware upgrades: asking whether one is available, and watching one run.
//!
//! The player exposes this on the control port, not on the web page a browser
//! sees. `GET /upgrade?upgrade=check` answers
//!
//! ```xml
//! <upgrade inProgress="false" available="true"/>
//! ```
//!
//! and `GET /upgrade?upgrade=this` starts one. There is a second scope,
//! `upgrade=all`, and an optional `&slave=<host>&port=<port>` for addressing a
//! zone member through its master; neither is offered here — see
//! [`crate::client::Client::start_upgrade`].
//!
//! None of this is documented. It was read out of the official controller's
//! own bundle, where one function builds every upgrade request it makes:
//!
//! ```js
//! function bp(e,t,n){let r={upgrade:t};return n&&(r.slave=n.host,r.port=n.port),
//! $.get(`upgrade`,{params:r,baseURL:e.toString(),timeout:6e4})
//! .then(e=>({inProgress:e.data.upgrade?._inProgress===`true`,
//!            isAvailable:e.data.upgrade?._available===`true`}))}
//! ```
//!
//! Note what that page at `/upgrade?noheader=1` is *not*: the official app
//! loads it in a webview on port 80 and never reads it. Anything parsed out of
//! it is a guess at a page meant for a person.
//!
//! # While an upgrade runs
//!
//! `/SyncStatus` stops answering with `<SyncStatus>` and answers with
//! `<UpgradeStatusStage1>` or `<UpgradeStatusStage2>` instead. That is not an
//! error and must not be read as one — a parser that insists on the usual root
//! sees a player that has gone mad at exactly the moment it needs watching.
//!
//! Stage 1 carries only the name, model and error flag. Stage 2 adds
//! `step`, `total` and `percent`. So an upgrade in its first stage has no
//! progress to show, which is a fact about the player rather than a gap here.

use crate::error::{Error, Result};
use crate::xml::{attributes, local_name};
use quick_xml::Reader;
use quick_xml::events::Event;

/// What the player says when asked about upgrades.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Availability {
    /// An upgrade is already running. Starting another is the one thing that
    /// must not happen, so this is checked before any request that starts one.
    pub in_progress: bool,
    /// The player has something newer to install.
    pub available: bool,
    /// The version being offered, where the player names one.
    ///
    /// The official controller reads only the two flags above and discards
    /// this, so there was nothing to copy — but a Powernode on 4.16.6 does
    /// send it, captured in the test below. Still optional: one player saying
    /// it is not every player saying it, and nothing here depends on it.
    pub version: Option<String>,
}

/// How far along a running upgrade is.
///
/// Fields beyond `name` and `model` are absent in stage 1; see the module
/// note. `total` of zero means the player has not said how many steps there
/// are, so a percentage of the whole cannot be worked out from the steps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub name: Option<String>,
    pub model: Option<String>,
    pub step: u32,
    pub total: u32,
    /// The player's own percentage for the step it is on, 0 to 100.
    pub percent: u32,
    pub error: bool,
    /// The player says this upgrade could be stopped.
    ///
    /// Parsed because the player sends it. Nothing acts on it: the official
    /// controller reads this field and then never looks at it again, and no
    /// route for stopping an upgrade appears anywhere in its bundle. Recorded
    /// so the next person does not have to go looking for one.
    pub abortable: bool,
}

impl Progress {
    /// What to tell someone watching, in the words the official app uses.
    pub fn stage(&self) -> Stage {
        if self.error {
            return Stage::Failed;
        }
        match self.step {
            0 => Stage::Preparing,
            1 => Stage::Downloading,
            step if self.total > 1 && step >= self.total => Stage::Rebooting,
            _ => Stage::Installing,
        }
    }
}

/// The steps an upgrade goes through, as the player counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Preparing,
    Downloading,
    Installing,
    Rebooting,
    Failed,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Preparing => "Preparing the upgrade",
            Stage::Downloading => "Downloading",
            Stage::Installing => "Installing",
            Stage::Rebooting => "Restarting the player",
            Stage::Failed => "The upgrade failed",
        }
    }
}

/// Read the answer to `?upgrade=…`.
///
/// Absent attributes read as false, which is the safe direction for both: an
/// upgrade that cannot be confirmed available is not started, and one that
/// cannot be confirmed finished is not treated as finished.
pub fn availability(xml: &str) -> Result<Availability> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    loop {
        let e = match reader.read_event() {
            Err(e) => return Err(Error::Screen(format!("upgrade: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => e,
            Ok(_) => continue,
        };

        let qname = e.name();
        if local_name(qname.as_ref()) != "upgrade" {
            continue;
        }

        let mut a = attributes(&e);
        return Ok(Availability {
            in_progress: flag(a.remove("inProgress")),
            available: flag(a.remove("available")),
            version: a.remove("version").filter(|v| !v.is_empty()),
        });
    }

    Err(Error::Screen("upgrade: no <upgrade> element".to_owned()))
}

/// An upgrade in progress, if that is what this `/SyncStatus` body is.
///
/// `Ok(None)` means the body is an ordinary SyncStatus and should be parsed as
/// one. An error means it is neither, which is worth distinguishing from "not
/// upgrading" — during a reboot the player answers with whatever its web
/// server has, and that is not a reason to conclude the upgrade finished.
pub fn in_sync_status(xml: &str) -> Result<Option<Progress>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    loop {
        let e = match reader.read_event() {
            Err(e) => return Err(Error::Screen(format!("upgrade status: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => e,
            Ok(_) => continue,
        };

        let qname = e.name();
        let name = local_name(qname.as_ref());

        if name == "SyncStatus" {
            return Ok(None);
        }

        if name != "UpgradeStatusStage1" && name != "UpgradeStatusStage2" {
            continue;
        }

        let mut a = attributes(&e);
        return Ok(Some(Progress {
            name: a.remove("name"),
            model: a.remove("model"),
            step: number(a.remove("step")),
            total: number(a.remove("total")),
            percent: number(a.remove("percent")),
            error: flag(a.remove("error")),
            abortable: flag(a.remove("abortable")),
        }));
    }

    Err(Error::Screen(
        "upgrade status: neither a SyncStatus nor an upgrade stage".to_owned(),
    ))
}

/// `1` and `true` both mean yes here — the player uses each in places.
fn flag(raw: Option<String>) -> bool {
    matches!(raw.as_deref(), Some("1") | Some("true"))
}

fn number(raw: Option<String>) -> u32 {
    raw.and_then(|v| v.parse().ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_the_player_says_about_upgrades() {
        let both =
            availability(r#"<upgrade inProgress="false" available="true"/>"#).expect("parses");
        assert_eq!(
            both,
            Availability {
                in_progress: false,
                available: true,
                version: None,
            }
        );

        let running =
            availability(r#"<upgrade inProgress="true" available="false"/>"#).expect("parses");
        assert!(running.in_progress);

        // Absent reads as false in both directions, which is what keeps a
        // missing attribute from starting an upgrade.
        let bare = availability("<upgrade/>").expect("parses");
        assert_eq!(bare, Availability::default());

        // Taken where the player names one, and absent is not an error: no
        // player is known to send this, so nothing may depend on it.
        let named =
            availability(r#"<upgrade available="true" version="4.18.2"/>"#).expect("parses");
        assert_eq!(named.version.as_deref(), Some("4.18.2"));
        assert_eq!(both.version, None, "and it is optional");

        assert!(
            availability("<something-else/>").is_err(),
            "a document that is not an upgrade answer is an error, not a no"
        );
    }

    /// Captured verbatim from an NAD Powernode running 4.16.6, which is where
    /// the `version` attribute is known from — the official controller reads
    /// past it.
    #[test]
    fn reads_a_real_check_from_a_powernode() {
        let real = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<upgrade inProgress="false" version="4.16.22" available="true">check</upgrade>"#
        );

        let ready = availability(real).expect("parses");
        assert!(ready.available);
        assert!(!ready.in_progress);
        assert_eq!(
            ready.version.as_deref(),
            Some("4.16.22"),
            "the player names what it is offering"
        );
    }

    #[test]
    fn an_ordinary_sync_status_is_not_an_upgrade() {
        let ordinary = in_sync_status(r#"<SyncStatus etag="x" id="10.0.0.155" name="Powernode"/>"#)
            .expect("parses");
        assert_eq!(ordinary, None);
    }

    /// Stage 1 carries no step, total or percent — there is genuinely nothing
    /// to show yet, and reading zeros as progress would draw a bar at nought.
    #[test]
    fn stage_one_has_a_name_and_nothing_to_measure() {
        let one = in_sync_status(
            r#"<UpgradeStatusStage1 name="Powernode" model="N330" error="0" abortable="1"/>"#,
        )
        .expect("parses")
        .expect("is an upgrade");

        assert_eq!(one.name.as_deref(), Some("Powernode"));
        assert_eq!((one.step, one.total, one.percent), (0, 0, 0));
        assert!(!one.error);
        assert!(
            one.abortable,
            "the player says so, even if nothing acts on it"
        );
        assert_eq!(one.stage(), Stage::Preparing);
    }

    #[test]
    fn stage_two_counts_its_way_through() {
        let at = |step: u32, total: u32, percent: u32| {
            in_sync_status(&format!(
                r#"<UpgradeStatusStage2 name="Powernode" model="N330" step="{step}"
                     total="{total}" percent="{percent}" error="0" abortable="0"/>"#
            ))
            .expect("parses")
            .expect("is an upgrade")
        };

        assert_eq!(at(0, 4, 0).stage(), Stage::Preparing);
        assert_eq!(at(1, 4, 30).stage(), Stage::Downloading);
        assert_eq!(at(2, 4, 60).stage(), Stage::Installing);
        assert_eq!(at(4, 4, 100).stage(), Stage::Rebooting, "the last step");
        assert_eq!(at(3, 4, 90).percent, 90);
    }

    #[test]
    fn an_error_beats_whatever_step_it_says() {
        let failed = in_sync_status(
            r#"<UpgradeStatusStage2 name="Powernode" step="2" total="4" percent="50" error="1"/>"#,
        )
        .expect("parses")
        .expect("is an upgrade");

        assert!(failed.error);
        assert_eq!(
            failed.stage(),
            Stage::Failed,
            "a failure is not 'installing' however far it got"
        );
    }

    /// A player mid-reboot answers with whatever its web server has, which is
    /// neither document. Saying so is different from saying the upgrade ended.
    #[test]
    fn something_that_is_neither_is_an_error_and_not_a_finished_upgrade() {
        assert!(in_sync_status("<html><body>Please wait</body></html>").is_err());
        assert!(in_sync_status("").is_err());
    }
}
