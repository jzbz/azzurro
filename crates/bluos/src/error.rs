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

    #[error("request to {device} failed")]
    Http {
        device: crate::DeviceId,
        #[source]
        source: reqwest::Error,
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

    #[error("{device} sent more than {limit} bytes in reply to one request")]
    Oversized {
        device: crate::DeviceId,
        limit: usize,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
