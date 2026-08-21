use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use crate::error::{Error, Result};

/// How a player is addressed and identified.
///
/// BluOS itself uses `host:port` as a device's identity — `/SyncStatus`
/// reports `id="10.0.0.155:11000"` — so this crate does too, rather than
/// inventing a key the device would not recognise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId {
    pub host: IpAddr,
    pub port: u16,
}

impl DeviceId {
    pub fn new(host: impl Into<IpAddr>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// A player at the standard control port.
    pub fn at(host: impl Into<IpAddr>) -> Self {
        Self::new(host, crate::DEFAULT_PORT)
    }

    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }

    /// The base of every control URL for this player.
    pub fn base_url(&self) -> String {
        match self.host {
            IpAddr::V4(v4) => format!("http://{v4}:{}", self.port),
            IpAddr::V6(v6) => format!("http://[{v6}]:{}", self.port),
        }
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host {
            IpAddr::V4(v4) => write!(f, "{v4}:{}", self.port),
            IpAddr::V6(v6) => write!(f, "[{v6}]:{}", self.port),
        }
    }
}

impl FromStr for DeviceId {
    type Err = Error;

    /// Accepts `host` or `host:port`, so a bare address on a command line
    /// means the standard control port.
    fn from_str(s: &str) -> Result<Self> {
        if let Ok(addr) = s.parse::<SocketAddr>() {
            return Ok(Self::new(addr.ip(), addr.port()));
        }
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(Self::at(ip));
        }
        Err(Error::BadDeviceId(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_forms_and_round_trips() {
        let with_port: DeviceId = "10.0.0.155:11000".parse().unwrap();
        let bare: DeviceId = "10.0.0.155".parse().unwrap();
        assert_eq!(with_port, bare);
        assert_eq!(with_port.to_string(), "10.0.0.155:11000");
        assert_eq!(with_port.base_url(), "http://10.0.0.155:11000");
        assert!("not-an-address".parse::<DeviceId>().is_err());
    }

    #[test]
    fn brackets_ipv6_in_urls() {
        let v6: DeviceId = "[::1]:11000".parse().unwrap();
        assert_eq!(v6.base_url(), "http://[::1]:11000");
    }
}
