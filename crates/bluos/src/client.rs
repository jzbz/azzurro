//! Talking to one player.
//!
//! The control API is plain HTTP on port 11000, returns XML, and has no
//! authentication of any kind: anything that can reach the player can drive it.
//! That is worth knowing before this crate is pointed at a network you do not
//! control.
//!
//! The read paths here — `/Status`, `/SyncStatus`, `/Volume` and the long-poll
//! below — have been exercised against a real player. The control verbs are
//! transcribed from the official controller's own call sites and are *not*
//! covered by the tests, because a test that skips someone's track is not a
//! test anyone wants to run.

use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::device::DeviceId;
use crate::error::{Error, Result};
use crate::queue::Queue;
use crate::screen::{Configuration, Screen};
use crate::settings::{Setting, Settings};
use crate::status::{Status, SyncStatus};

/// Ordinary requests are answered from memory, so this is generous already;
/// the long-poll below sets its own.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// What this client tells the player it understands.
///
/// The player serves different documents to different numbers, and it is not a
/// small difference: at 35 the Sources screen hands out plain `browse` actions
/// for the music services where an older client is given a `deep-link` it has
/// to know how to translate, Home gains a lazily-fetched shelf, and the play
/// queue offers a fourth button — Queue Builder Mode — that a client declaring
/// nothing never sees at all. Verified against a Powernode on BluOS 4.16.6 by
/// fetching every screen with and without the headers and diffing them.
///
/// The two numbers are what the official controller declares. Raising them
/// past what it sends would be claiming to understand documents nobody has
/// seen.
/// Everything a form field may not carry raw. A share's name is a UNC path —
/// `\\10.0.0.100\media\music` — so the backslashes matter as much as the
/// ampersands.
const FORM_FIELD: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

const SCHEMA_VERSION: &str = "35";
const UI_SCHEMA_VERSION: &str = "7";

/// How long `/Status` is asked to hold a poll open.
///
/// The player honours this exactly: a poll with an unchanged etag returns after
/// the timeout with the same document. A hundred seconds keeps the request rate
/// near zero on an idle player while staying well inside any NAT idle timeout.
pub const DEFAULT_POLL: Duration = Duration::from_secs(100);

/// Added to the poll timeout to get the HTTP timeout, so that the player's own
/// deadline is always the one that fires first and a real network stall is
/// still distinguishable from a quiet player.
const POLL_SLACK: Duration = Duration::from_secs(15);

/// What `<repeat>` means. Note that off is 2, not 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    All = 0,
    One = 1,
    Off = 2,
}

impl Repeat {
    pub fn from_status(value: u8) -> Self {
        match value {
            0 => Self::All,
            1 => Self::One,
            _ => Self::Off,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    id: DeviceId,
    base: String,
    http: reqwest::Client,
}

impl Client {
    /// A client for one player.
    ///
    /// **Panics** if the `reqwest` in the build has a TLS backend enabled with
    /// no crypto provider installed — reqwest panics rather than erroring when
    /// its `rustls-no-provider` feature is on and nothing has called
    /// `install_default`. This crate speaks plain HTTP and asks for no TLS
    /// features itself, but Cargo unifies features across a build, so anything
    /// else in the tree that wants https decides this for everyone. A binary in
    /// that position should install a provider before the first client:
    ///
    /// ```no_run
    /// let _ = rustls::crypto::ring::default_provider().install_default();
    /// ```
    ///
    /// Use [`Client::with_http`] to supply a client built to your own taste
    /// and side-step the question.
    pub fn new(id: DeviceId) -> Result<Self> {
        Ok(Self::with_http(id, reqwest::Client::builder().build()?))
    }

    /// Share one `reqwest::Client` across every player.
    ///
    /// Worth doing in an app: the connection pool and the resolver are per
    /// client, and a controller holds a long-poll open to every player at once.
    pub fn with_http(id: DeviceId, http: reqwest::Client) -> Self {
        Self {
            id,
            base: id.base_url(),
            http,
        }
    }

    pub fn id(&self) -> DeviceId {
        self.id
    }

    /// Turn an artwork path from a status document into something fetchable.
    ///
    /// Players give paths relative to themselves for their own artwork, but
    /// hand back absolute URLs for anything that came from a streaming
    /// service's CDN, so both have to be handled.
    pub fn image_url(&self, src: &str) -> String {
        if src.starts_with("http://") || src.starts_with("https://") {
            src.to_owned()
        } else if src.starts_with('/') {
            format!("{}{src}", self.base)
        } else {
            format!("{}/{src}", self.base)
        }
    }

    async fn get_text(
        &self,
        path: &str,
        query: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<String> {
        self.http
            .get(format!("{}{path}", self.base))
            .query(query)
            .header("x-sovi-schema-version", SCHEMA_VERSION)
            .header("x-sovi-ui-schema-version", UI_SCHEMA_VERSION)
            .timeout(timeout)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| Error::Http {
                device: self.id,
                source,
            })?
            .text()
            .await
            .map_err(|source| Error::Http {
                device: self.id,
                source,
            })
    }

    async fn get_xml<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<T> {
        let body = self.get_text(path, query, timeout).await?;
        quick_xml::de::from_str(&body).map_err(|source| Error::Xml {
            device: self.id,
            source,
            body: snippet(&body),
        })
    }

    /// Fire a control verb and discard the acknowledgement.
    async fn command(&self, path: &str, query: &[(&str, &str)]) -> Result<()> {
        self.get_text(path, query, REQUEST_TIMEOUT).await.map(drop)
    }

    pub async fn status(&self) -> Result<Status> {
        self.get_xml("/Status", &[], REQUEST_TIMEOUT).await
    }

    pub async fn sync_status(&self) -> Result<SyncStatus> {
        self.get_xml("/SyncStatus", &[], REQUEST_TIMEOUT).await
    }

    /// Which screens this player offers.
    ///
    /// The starting point for browsing: everything else is reached by
    /// following actions out of the screens named here.
    pub async fn ui_configuration(&self) -> Result<Configuration> {
        self.get_xml("/ui/Configuration", &[], REQUEST_TIMEOUT)
            .await
    }

    /// Fetch and parse one server-driven screen.
    ///
    /// `path` comes from `/ui/Configuration` or from a `browse` action, and
    /// already carries its own query string, so nothing is appended to it.
    pub async fn screen(&self, path: &str) -> Result<Screen> {
        let body = self.get_text(path, &[], REQUEST_TIMEOUT).await?;
        crate::screen::parse(&body)
    }

    /// Read a page of settings.
    ///
    /// `/Settings` on the control port answers 301 to the settings service,
    /// which is on a port of its own — 11001 on every player seen. Rather than
    /// hard-coding that, the redirect is followed and the address it lands on
    /// is what the returned document carries as its base, so a write goes back
    /// to wherever the read came from.
    pub async fn settings(&self, page: Option<&str>) -> Result<Settings> {
        let path = match page {
            Some(id) => format!("/Settings?id={id}"),
            None => "/Settings".to_owned(),
        };

        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| Error::Http {
                device: self.id,
                source,
            })?;

        let landed = response.url().clone();
        let base = format!("{}://{}", landed.scheme(), landed.authority());
        let body = response.text().await.map_err(|source| Error::Http {
            device: self.id,
            source,
        })?;

        crate::settings::parse(&body, &base)
    }

    /// Change one setting.
    ///
    /// A POST of `{name: value}` to the URL the setting names — the shape the
    /// official controller's own `updateSettings` uses.
    ///
    /// The URL resolves against the **control port, not the settings port**,
    /// which is the trap here: settings are *read* from 11001 and *written* to
    /// 11000, and posting a write back to where the document came from answers
    /// 404. Confirmed against a player by writing a setting the value it
    /// already held, then a different one, and watching it change and revert.
    pub async fn write_setting(
        &self,
        _settings: &Settings,
        setting: &Setting,
        value: &str,
    ) -> Result<()> {
        let (Some(url), Some(name)) = (setting.url.as_deref(), setting.name.as_deref()) else {
            return Err(Error::Screen(format!(
                "the setting {:?} says nothing about where to write it",
                setting.id
            )));
        };

        // Built by hand rather than with a serialiser: this is the only JSON
        // this crate ever sends, and it is two strings.
        let body = format!("{{{}:{}}}", json_string(name), json_string(value));

        self.http
            .post(format!("{}{url}", self.base))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .timeout(REQUEST_TIMEOUT)
            .body(body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map(drop)
            .map_err(|source| Error::Http {
                device: self.id,
                source,
            })
    }

    /// Ask the player whether there is firmware to install.
    ///
    /// Reading only. Starting an upgrade is deliberately not offered here:
    /// it is the one operation where getting it wrong leaves somebody with a
    /// brick, and the player's own page does it perfectly well.
    /// Returns what the player says, and the action it offers if any.
    #[allow(clippy::type_complexity)]
    pub async fn upgrade_check(&self) -> Result<(Option<String>, Option<(String, String)>)> {
        let body = self
            .get_text("/upgrade?noheader=1", &[], REQUEST_TIMEOUT)
            .await?;
        Ok((
            crate::reports::upgrade_status(&body),
            crate::reports::upgrade_action(&body),
        ))
    }

    /// A page of the player's own web UI, which is a different server.
    ///
    /// The control port serves XML on 11000; the configuration pages are a
    /// separate web UI on port 80. Some of them are reachable through
    /// `/redirectToCp` on the control port and some — the share configuration
    /// among them — answer 404 there and only exist on 80.
    pub fn web_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.id.host)
    }

    async fn get_web(&self, path: &str) -> Result<String> {
        self.http
            .get(self.web_url(path))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| Error::Http {
                device: self.id,
                source,
            })?
            .text()
            .await
            .map_err(|source| Error::Http {
                device: self.id,
                source,
            })
    }

    /// Read the form off one of the player's own configuration pages.
    ///
    /// The first form on the page, which is the only one on all of these.
    /// `None` where the page has none — a firmware that changed its shape, or a
    /// page that is only a message.
    pub async fn web_form(&self, path: &str) -> Result<Option<crate::forms::Form>> {
        let body = self.get_web(path).await?;
        Ok(crate::forms::parse(&body).into_iter().next())
    }

    /// Send a form back, and return the page that comes of it.
    ///
    /// The answer is another page rather than a status: signing in wrong comes
    /// back as the same form with a message on it, and asking to add a share
    /// comes back as the next step's form. Handing the body to the caller is
    /// what lets one screen follow another without knowing which pages exist.
    ///
    /// `pressed` is the name of the button, which is how these pages tell one
    /// action from another: Login and Logout post the same fields and differ
    /// only in which name arrives with them.
    pub async fn submit_form(
        &self,
        form: &crate::forms::Form,
        values: &std::collections::BTreeMap<String, String>,
        pressed: &crate::forms::Submit,
    ) -> Result<String> {
        let mut body = String::new();
        // Hidden first, then what was filled in, then the button. The player
        // reads the last of a repeated name, and nothing here repeats, but the
        // order is the page's own and worth keeping.
        let pairs = form
            .hidden
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .chain(values.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            // A nameless button contributes nothing to the body; it is the only
            // thing its form does, so pressing it is the whole message.
            .chain(
                (!pressed.name.is_empty())
                    .then_some((pressed.name.as_str(), pressed.label.as_str())),
            );

        for (key, value) in pairs {
            if !body.is_empty() {
                body.push('&');
            }
            body.push_str(&percent_encoding::utf8_percent_encode(key, FORM_FIELD).to_string());
            body.push('=');
            body.push_str(&percent_encoding::utf8_percent_encode(value, FORM_FIELD).to_string());
        }

        let url = self.web_url(&form.action);
        let request = if form.post {
            self.http.post(url).header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
        } else {
            self.http.get(format!(
                "{}{}{body}",
                self.web_url(&form.action),
                if form.action.contains('?') { "&" } else { "?" }
            ))
        };

        let request = if form.post {
            request.body(body)
        } else {
            request
        };

        request
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| Error::Http {
                device: self.id,
                source,
            })?
            .text()
            .await
            .map_err(|source| Error::Http {
                device: self.id,
                source,
            })
    }

    /// Every music service the player offers to sign into.
    ///
    /// A list worth drawing. What each row leads to is not: signing in asks for
    /// a password and sometimes a captcha, so [`reports::Service::href`] is a
    /// page to open rather than a form to rebuild.
    pub async fn services(&self) -> Result<Vec<crate::reports::Service>> {
        let body = self
            .get_text(
                "/redirectToCp?href=%2Fservices%3Fnoheader%3D1",
                &[],
                REQUEST_TIMEOUT,
            )
            .await?;
        Ok(crate::reports::services(&body))
    }

    /// The network shares the player is indexing, and where a change to them
    /// goes.
    pub async fn shares(&self) -> Result<(Option<String>, Vec<crate::reports::Share>)> {
        let body = self.get_web("/sharecfg?noheader=1").await?;
        Ok(crate::reports::shares(&body))
    }

    /// Unmount shares, by the field names the page gave for them.
    ///
    /// A form post rather than an API call, because that is all the player
    /// offers: the checkbox's name is the UNC path and its presence is the
    /// whole of the request.
    pub async fn remove_shares(&self, action: &str, fields: &[String]) -> Result<()> {
        // Encoded by hand: reqwest is built here with default features off,
        // and turning one on for a single form post is a heavier change than
        // the two lines it saves.
        let mut body = String::from("remove=Remove+selected+shares");
        for field in fields {
            body.push('&');
            body.push_str(&percent_encoding::utf8_percent_encode(field, FORM_FIELD).to_string());
            body.push_str("=on");
        }

        self.http
            .post(self.web_url(action))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .timeout(REQUEST_TIMEOUT)
            .body(body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| Error::Http {
                device: self.id,
                source,
            })
            .map(drop)
    }

    /// One track's technical details, as label and value.
    pub async fn technical_info(&self, uri: &str) -> Result<Vec<(String, String)>> {
        let body = self.get_text(uri, &[], REQUEST_TIMEOUT).await?;
        Ok(crate::reports::technical_info(&body))
    }

    /// The player's diagnostics, as label and value.
    pub async fn diagnostics(&self) -> Result<Vec<(String, String)>> {
        let body = self
            .get_text("/redirectToCp?href=/diagnostics", &[], REQUEST_TIMEOUT)
            .await?;
        Ok(crate::reports::diagnostics(&body))
    }

    /// Send a path the player itself supplied, and discard the answer.
    ///
    /// This is how a `player-link` action is carried out: the screen document
    /// gives a complete URL — `/Play?url=Capture%3Ahw%3A…` — and the client's
    /// whole job is to fetch it. Deliberately generic, because the point of the
    /// server-driven screens is that the client does not know what the player
    /// will ask for next.
    pub async fn follow(&self, path: &str) -> Result<()> {
        self.get_text(path, &[], REQUEST_TIMEOUT).await.map(drop)
    }

    /// The whole play queue.
    ///
    /// One document however long the queue is. Worth re-reading whenever
    /// [`Status::pid`] changes, which is the player saying the queue was
    /// replaced.
    pub async fn queue(&self) -> Result<Queue> {
        self.get_xml("/Playlist", &[], REQUEST_TIMEOUT).await
    }

    /// A window onto the queue, by position, both ends included.
    ///
    /// For a queue long enough that pulling all of it to redraw a screenful
    /// would be wasteful. `length` on the result is still the whole queue, not
    /// the size of the window.
    pub async fn queue_range(&self, start: u32, end: u32) -> Result<Queue> {
        self.get_xml(
            "/Playlist",
            &[("start", &start.to_string()), ("end", &end.to_string())],
            REQUEST_TIMEOUT,
        )
        .await
    }

    /// A long-poll that yields a new [`Status`] whenever this player changes.
    pub fn watch(&self) -> StatusWatch {
        StatusWatch {
            client: self.clone(),
            etag: None,
            poll: DEFAULT_POLL,
        }
    }

    pub async fn play(&self) -> Result<()> {
        self.command("/Play", &[]).await
    }

    pub async fn pause(&self) -> Result<()> {
        self.command("/Pause", &[]).await
    }

    /// Play or pause, whichever the player is not doing, in one round trip.
    pub async fn toggle(&self) -> Result<()> {
        self.command("/Pause", &[("toggle", "1")]).await
    }

    pub async fn stop(&self) -> Result<()> {
        self.command("/Stop", &[]).await
    }

    pub async fn skip(&self) -> Result<()> {
        self.command("/Skip", &[]).await
    }

    /// Previous track. On most services this restarts the current track first,
    /// exactly as the hardware remote does.
    pub async fn back(&self) -> Result<()> {
        self.command("/Back", &[]).await
    }

    pub async fn seek(&self, secs: u32) -> Result<()> {
        self.command("/Play", &[("seek", &secs.to_string())]).await
    }

    /// Jump to a position in the play queue.
    pub async fn play_queue_index(&self, index: u32) -> Result<()> {
        self.command("/Play", &[("id", &index.to_string())]).await
    }

    /// Volume as the 0-100 scale the app and the hardware both show. The player
    /// also accepts `db`, which is what it stores; this is the linear taper it
    /// maps that to.
    pub async fn set_volume(&self, level: i32) -> Result<()> {
        self.command("/Volume", &[("level", &level.clamp(0, 100).to_string())])
            .await
    }

    pub async fn set_mute(&self, on: bool) -> Result<()> {
        self.command("/Volume", &[("mute", if on { "1" } else { "0" })])
            .await
    }

    pub async fn set_shuffle(&self, on: bool) -> Result<()> {
        self.command("/Shuffle", &[("state", if on { "1" } else { "0" })])
            .await
    }

    pub async fn set_repeat(&self, mode: Repeat) -> Result<()> {
        self.command("/Repeat", &[("state", &(mode as u8).to_string())])
            .await
    }

    /// Advance the sleep timer one step.
    ///
    /// There is no way to set a particular duration: the endpoint cycles
    /// through the player's own ladder — 15, 30, 45, 60, 90 minutes and then
    /// off — which is what the hardware remote's Sleep button does too. The new
    /// value comes back in `/Status`.
    pub async fn cycle_sleep(&self) -> Result<()> {
        self.command("/Sleep", &[]).await
    }

    pub async fn clear_queue(&self) -> Result<()> {
        self.command("/Clear", &[]).await
    }

    /// Drop only the tail the player added by itself.
    ///
    /// Autofill appends tracks the player picked when the queue would otherwise
    /// have run out. This removes those and leaves what was put there on
    /// purpose.
    pub async fn clear_autofill(&self) -> Result<()> {
        self.command("/Clear", &[("nextlist", "1")]).await
    }

    /// Take the track at `from` out of the queue and put it back at `to`.
    ///
    /// The parameter names read backwards — the destination is `new` and the
    /// source is `old` — and the player gives no hint which is which, so the
    /// order was settled on hardware: with a thirteen-track queue,
    /// `Move?new=10&old=12` moved the last track up to position ten and left
    /// everything else in order.
    ///
    /// Positions are absolute queue indices, the same ones `/Play?id=` and
    /// `/Delete?id=` use, and the same one `/Status` reports as `song`.
    pub async fn move_queue_item(&self, from: u32, to: u32) -> Result<()> {
        self.command(
            "/Move",
            &[("new", &to.to_string()), ("old", &from.to_string())],
        )
        .await
    }

    /// Take one track out of the queue.
    ///
    /// Everything after it shifts down, so a caller removing several has to
    /// work from the end or re-read in between.
    pub async fn delete_queue_item(&self, index: u32) -> Result<()> {
        self.command("/Delete", &[("id", &index.to_string())]).await
    }

    /// Save the queue as a playlist under `name`.
    ///
    /// The older of the two ways the player offers. The other is a round trip
    /// through `/AddToPlaylistOptions?saveQueue=1`, which returns a document
    /// naming the services that will accept a playlist and the ones that will
    /// not; this one skips the choosing and saves to the player's own list.
    pub async fn save_queue(&self, name: &str) -> Result<()> {
        self.command("/Save", &[("name", name)]).await
    }

    /// Load one of the player's stored presets, the numbers the hardware
    /// buttons and the app's preset list both use.
    pub async fn load_preset(&self, id: u32) -> Result<()> {
        self.command("/Preset", &[("id", &id.to_string())]).await
    }

    /// Group `slave` under this player. Called on the master.
    pub async fn add_slave(&self, slave: DeviceId) -> Result<()> {
        self.command(
            "/AddSlave",
            &[
                ("slave", &slave.host.to_string()),
                ("port", &slave.port.to_string()),
            ],
        )
        .await
    }

    /// Break `slave` out of this player's group. Called on the master.
    pub async fn remove_slave(&self, slave: DeviceId) -> Result<()> {
        self.command(
            "/RemoveSlave",
            &[
                ("slave", &slave.host.to_string()),
                ("port", &slave.port.to_string()),
            ],
        )
        .await
    }
}

/// A JSON string literal, quotes and all.
fn json_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl From<reqwest::Error> for Error {
    fn from(source: reqwest::Error) -> Self {
        // Only reachable from client construction, which has no device yet.
        Error::Http {
            device: DeviceId::at(std::net::Ipv4Addr::UNSPECIFIED),
            source,
        }
    }
}

/// The head of a response, for an error message.
///
/// Enough to recognise the document and see which element went wrong, short
/// enough to sit on one log line. Cut on a character boundary: a player's
/// track titles are UTF-8 and slicing bytes would panic on the one document
/// anyone actually needs to read.
fn snippet(body: &str) -> String {
    const LIMIT: usize = 240;
    if body.len() <= LIMIT {
        return body.trim().to_owned();
    }
    let end = (0..=LIMIT)
        .rev()
        .find(|i| body.is_char_boundary(*i))
        .unwrap_or(0);
    format!("{}…", body[..end].trim())
}

/// A long-poll on one player's `/Status`.
///
/// Each call to [`StatusWatch::next`] blocks until something about the player
/// changes, or until the poll timeout expires and the player answers with the
/// document unchanged. Either way the answer is current, so a caller can treat
/// every return as "here is the state now" and never needs a separate poll.
///
/// Errors are returned rather than retried. A controller should back off and
/// call `next` again: the etag is kept, so a reconnection resumes rather than
/// restarting, and [`StatusWatch::forget_etag`] forces a full read when the
/// caller wants one.
pub struct StatusWatch {
    client: Client,
    etag: Option<String>,
    poll: Duration,
}

impl StatusWatch {
    /// Override how long each poll is held open.
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    pub fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    /// Drop the etag, so the next call returns immediately with current state
    /// instead of waiting for a change.
    pub fn forget_etag(&mut self) {
        self.etag = None;
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn next(&mut self) -> Result<Status> {
        let seconds = self.poll.as_secs().to_string();
        let mut query: Vec<(&str, &str)> = vec![("timeout", &seconds)];
        if let Some(etag) = &self.etag {
            query.push(("etag", etag));
        }

        let status: Status = self
            .client
            .get_xml("/Status", &query, self.poll + POLL_SLACK)
            .await?;
        self.etag = Some(status.etag.clone());
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        // See the note on `Client::new`: another crate in this workspace wants
        // https for cover art, and feature unification means reqwest here
        // demands a provider too. Installing it repeatedly is harmless — the
        // second call reports that one is already in place.
        let _ = rustls::crypto::ring::default_provider().install_default();
        Client::new("192.0.2.155:11000".parse().unwrap()).unwrap()
    }

    #[test]
    fn resolves_artwork_from_both_sources() {
        let c = client();
        assert_eq!(
            c.image_url("/images/capture/ic_tvNP.png"),
            "http://192.0.2.155:11000/images/capture/ic_tvNP.png"
        );
        // Service CDNs hand back absolute URLs, which must be left alone.
        assert_eq!(
            c.image_url("https://cdn-profiles.tunein.com/Logo.png"),
            "https://cdn-profiles.tunein.com/Logo.png"
        );
        assert_eq!(
            c.image_url("images/x.png"),
            "http://192.0.2.155:11000/images/x.png"
        );
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("plain"), r#""plain""#);
        assert_eq!(json_string(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(json_string("back\\slash"), r#""back\\slash""#);
        assert_eq!(json_string("a\nb"), r#""a\nb""#);
        // A room name is free text and can contain anything.
        assert_eq!(json_string("Ol\u{e1} \u{1f3b5}"), "\"Ol\u{e1} \u{1f3b5}\"");
    }

    #[test]
    fn repeat_off_is_two() {
        assert_eq!(Repeat::Off as u8, 2);
        assert_eq!(Repeat::from_status(0), Repeat::All);
        assert_eq!(Repeat::from_status(2), Repeat::Off);
        assert_eq!(Repeat::from_status(99), Repeat::Off);
    }
}
