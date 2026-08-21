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
use crate::status::{Status, SyncStatus};

/// Ordinary requests are answered from memory, so this is generous already;
/// the long-poll below sets its own.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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

    pub async fn clear_queue(&self) -> Result<()> {
        self.command("/Clear", &[]).await
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

impl From<reqwest::Error> for Error {
    fn from(source: reqwest::Error) -> Self {
        // Only reachable from client construction, which has no device yet.
        Error::Http {
            device: DeviceId::at(std::net::Ipv4Addr::UNSPECIFIED),
            source,
        }
    }
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
    fn repeat_off_is_two() {
        assert_eq!(Repeat::Off as u8, 2);
        assert_eq!(Repeat::from_status(0), Repeat::All);
        assert_eq!(Repeat::from_status(2), Repeat::Off);
        assert_eq!(Repeat::from_status(99), Repeat::Off);
    }
}
