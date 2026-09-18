/// Everything this crate can fail at.
///
/// Transport and parse failures carry the device they came from, because a
/// controller talks to every player on the network at once and an error with
/// no address in it is not actionable.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network I/O failed")]
    Io(#[from] std::io::Error),

    #[error("no usable IPv4 interface to broadcast discovery on")]
    NoInterface,

    #[error("malformed LSDP packet: {0}")]
    Lsdp(&'static str),

    /// A request that got no usable answer.
    ///
    /// The cause is in the message and not behind `source()`. Nothing that
    /// shows these walks a chain — a toast, the poll's log line — so a cause
    /// kept only there reached nobody, and every network failure read "request
    /// to 10.0.0.5:11000 failed" and stopped. Keeping it in both would print it
    /// twice wherever something does walk the chain, as the CLI's does.
    ///
    /// The path goes in too, which reqwest's own message used to carry: with
    /// several routes that some firmware lacks, "it answered 404 Not Found"
    /// says nothing until it says which request it was about.
    #[error("request to {} failed: {}", target(.device, .cause), reason(.cause, .body))]
    Http {
        device: crate::DeviceId,
        cause: reqwest::Error,
        /// The start of what the player said with a 4xx or 5xx, which is
        /// where it explains itself — `must be application/json`, say. Empty
        /// for everything else.
        body: String,
    },

    #[error("{device} returned XML this crate could not read: {source} — {body}")]
    Xml {
        device: crate::DeviceId,
        #[source]
        source: quick_xml::DeError,
        /// The start of what actually came back. Without it a parse failure
        /// names no culprit, and the documents that fail are the transient
        /// ones — gone by the time anyone goes looking.
        body: String,
    },

    #[error("{0} is not a host:port a player can live at")]
    BadDeviceId(String),

    #[error("could not read a screen document: {0}")]
    Screen(String),

    /// Something this crate declined to send, because it could already see
    /// the request was wrong or pointless: a second upgrade on a player running
    /// one, a preset with nothing to play. Nothing was read and nothing was
    /// sent, so it is neither a transport failure nor a parse failure.
    #[error("{0}")]
    Refused(String),

    #[error("{device} asked for {url}, which is not on that player")]
    OffPlayer {
        device: crate::DeviceId,
        url: String,
    },

    #[error("{device} sent more than {limit} bytes in reply to one request")]
    Oversized {
        device: crate::DeviceId,
        limit: usize,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where a failed request went: the address it was sent to and its path.
///
/// The address comes from the request rather than from `device` where reqwest
/// kept it, because the two differ — the web pages are on port 80, and a
/// settings page may be read wherever a redirect landed. The query is left
/// off. A form sent as a GET carries what was typed into it there, and an error
/// message ends up in a toast and a log line, where none of that belongs.
fn target(device: &crate::DeviceId, cause: &reqwest::Error) -> String {
    let Some(url) = cause.url() else {
        return device.to_string();
    };
    match (url.host_str(), url.port_or_known_default()) {
        (Some(host), Some(port)) => format!("{host}:{port}{}", url.path()),
        _ => format!("{device}{}", url.path()),
    }
}

/// Why a request failed, in the fewest words that say it.
///
/// reqwest's own message is "error sending request for url (…)", which repeats
/// the address and names no reason; the reason is at the bottom of its chain —
/// `Connection refused`, or a redirect this crate refused. A reply that did
/// arrive is described by its status instead, with whatever the player said
/// alongside it.
fn reason(cause: &reqwest::Error, body: &str) -> String {
    if let Some(status) = cause.status() {
        return if body.is_empty() {
            format!("it answered {status}")
        } else {
            format!("it answered {status}: {body}")
        };
    }
    // A timeout is told apart before the chain is walked, because the bottom
    // of it does not say which one it was. A player that has dropped off the
    // network, which the connect timeout exists for, read only "deadline has
    // elapsed": tokio's words, which say neither that it was a timeout nor
    // that the connection never opened, and that is the whole diagnosis.
    if cause.is_timeout() {
        return if cause.is_connect() {
            "could not connect: timed out".to_owned()
        } else {
            "timed out waiting for a reply".to_owned()
        };
    }
    let mut innermost: &dyn std::error::Error = cause;
    while let Some(next) = innermost.source() {
        innermost = next;
    }
    if cause.is_connect() {
        format!("could not connect: {innermost}")
    } else {
        innermost.to_string()
    }
}
