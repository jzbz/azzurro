//! The LSDP wire format — Lenbrook Service Discovery Protocol.
//!
//! LSDP is how BluOS players announce themselves. It is undocumented; this
//! implementation was written against the format the official controller's own
//! decoder implements, and checked against packets from real hardware (see the
//! test at the bottom, which is a capture from a Bluesound Powernode).
//!
//! A packet is a six-byte header followed by one or more messages:
//!
//! ```text
//! 0        header length, and also the offset the first message starts at (6)
//! 1..=4    the ASCII magic word "LSDP"
//! 5        protocol version (1)
//! ```
//!
//! Every message begins with its own total length and a single ASCII byte
//! naming its type, so an unknown type can always be stepped over. Only `A`,
//! announce, carries anything a controller needs:
//!
//! ```text
//! len  type='A'
//! u8 node-id length, then that many bytes  (a MAC address, in practice)
//! u8 address length,  then that many bytes (four bytes of IPv4, in practice)
//! u8 announce-record count, then that many records:
//!     u8 class major, u8 class minor
//!     u8 TXT count, then that many key/value pairs, each a
//!        u8 length followed by that many bytes of UTF-8
//! ```
//!
//! The controller sends `Q`, query, to ask everyone to announce now. Players
//! also announce unprompted, so a long-lived listener picks up arrivals
//! without polling.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use crate::error::{Error, Result};

/// The UDP port LSDP lives on, for both the query and the announcements.
pub const PORT: u16 = 11430;

/// A query for every device class.
///
/// One message, type `Q`, holding a single class filter of `FF FF` — the
/// wildcard. The official controller sends exactly these bytes, and a player
/// answers with the same announce it would have broadcast on its own.
pub const QUERY: [u8; 11] = [
    0x06, b'L', b'S', b'D', b'P', 0x01, // header
    0x05, b'Q', 0x01, 0xff, 0xff, // one query message, class FF:FF
];

/// One device announcing itself, and everything it said in one packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    /// Opaque device identity, stable across address changes. Six bytes of MAC
    /// on every player seen so far, but the length is on the wire, so treat it
    /// as bytes.
    pub node_id: Vec<u8>,
    pub address: Ipv4Addr,
    pub records: Vec<Record>,
}

/// One advertised service on a device. A player announces several: the control
/// API is one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// `(major, minor)`. Major is 0 on everything observed.
    pub class: (u8, u8),
    /// Sorted so that equality and debug output are stable.
    pub txt: BTreeMap<String, String>,
}

impl Record {
    /// Whether this record advertises a controllable player.
    ///
    /// The minor-class set is taken from the official controller, which accepts
    /// 1, 3, 6 and 8 and ignores everything else. A Powernode, for instance,
    /// also announces class 0:4 on port 11431, which is not the control API.
    pub fn is_player(&self) -> bool {
        self.class.0 == 0 && matches!(self.class.1, 1 | 3 | 6 | 8)
    }

    /// The port from the TXT records, defaulting the way the official
    /// controller defaults it when a player omits one.
    pub fn port(&self) -> u16 {
        self.txt
            .get("port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(crate::DEFAULT_PORT)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.txt.get(key).map(String::as_str)
    }
}

impl Announce {
    /// The first record that describes a controllable player, if any.
    pub fn player(&self) -> Option<&Record> {
        self.records.iter().find(|r| r.is_player())
    }
}

/// A bounds-checked reader. Every length in LSDP comes off the wire, so every
/// read has to be able to fail rather than panic on a truncated packet.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or(Error::Lsdp("truncated: wanted one more byte"))?;
        self.pos += 1;
        Ok(b)
    }

    /// A length-prefixed run of bytes, the shape every variable field takes.
    fn prefixed(&mut self) -> Result<&'a [u8]> {
        let n = self.u8()? as usize;
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or(Error::Lsdp("truncated: length ran past the end"))?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn prefixed_str(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.prefixed()?).into_owned())
    }
}

/// Decode a packet into the announcements it carries.
///
/// Messages of a type this crate does not handle — including the `Q` a
/// controller hears when its own broadcast loops back — are skipped using the
/// on-the-wire length, so an unknown type costs nothing and an empty result is
/// normal rather than an error.
pub fn parse(packet: &[u8]) -> Result<Vec<Announce>> {
    if packet.len() < 6 {
        return Err(Error::Lsdp("shorter than a header"));
    }
    if &packet[1..5] != b"LSDP" {
        return Err(Error::Lsdp("missing the LSDP magic word"));
    }

    // Byte 0 is the header length and doubles as the offset of the first
    // message, which is how the official decoder finds it.
    let mut pos = packet[0] as usize;
    let mut out = Vec::new();

    while pos + 1 < packet.len() {
        let len = packet[pos] as usize;
        if len < 2 {
            return Err(Error::Lsdp("message length below its own header"));
        }
        let end = pos
            .checked_add(len)
            .filter(|e| *e <= packet.len())
            .ok_or(Error::Lsdp("message length ran past the end"))?;

        // Bound the body to this message before reading it, so a lying inner
        // length cannot walk into the next message.
        if packet[pos + 1] == b'A' {
            out.push(parse_announce(&packet[pos + 2..end])?);
        }
        pos = end;
    }

    Ok(out)
}

fn parse_announce(body: &[u8]) -> Result<Announce> {
    let mut r = Reader::new(body);

    let node_id = r.prefixed()?.to_vec();

    let address = match r.prefixed()? {
        [a, b, c, d] => Ipv4Addr::new(*a, *b, *c, *d),
        _ => return Err(Error::Lsdp("address field was not four bytes of IPv4")),
    };

    let count = r.u8()?;
    let mut records = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let class = (r.u8()?, r.u8()?);
        let txt_count = r.u8()?;
        let mut txt = BTreeMap::new();
        for _ in 0..txt_count {
            let key = r.prefixed_str()?;
            let value = r.prefixed_str()?;
            txt.insert(key, value);
        }
        records.push(Record { class, txt });
    }

    Ok(Announce {
        node_id,
        address,
        records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real announcement from a Bluesound Powernode N330 running BluOS 4.16.6,
    /// with the MAC and address replaced by documentation values. Two records:
    /// the control API on 11000, and a second service on 11431 that is not a
    /// player and must not be treated as one.
    const POWERNODE: &str = "064c534450016841\
        06aabbccddeeff04c000029b02\
        000105046e616d6509506f7765726e6f646504706f7274053131303030056d6f64656c044e333330\
        0776657273696f6e06342e31362e36027a730130\
        000402046e616d6509506f7765726e6f646504706f7274053131343331";

    fn bytes(hex: &str) -> Vec<u8> {
        let hex: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_a_real_announcement() {
        let announces = parse(&bytes(POWERNODE)).unwrap();
        assert_eq!(announces.len(), 1);
        let a = &announces[0];

        assert_eq!(a.node_id, vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(a.address, Ipv4Addr::new(192, 0, 2, 155));
        assert_eq!(a.records.len(), 2);

        let player = a.player().expect("the 0:1 record is a player");
        assert_eq!(player.class, (0, 1));
        assert_eq!(player.port(), 11000);
        assert_eq!(player.get("name"), Some("Powernode"));
        assert_eq!(player.get("model"), Some("N330"));
        assert_eq!(player.get("version"), Some("4.16.6"));
        assert_eq!(player.get("zs"), Some("0"));

        // The 0:4 record shares a name and would be indistinguishable without
        // the class check.
        assert!(!a.records[1].is_player());
        assert_eq!(a.records[1].port(), 11431);
    }

    #[test]
    fn a_query_carries_no_announcements() {
        // The controller hears its own broadcast on the socket it sent from.
        assert!(parse(&QUERY).unwrap().is_empty());
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let full = bytes(POWERNODE);
        for n in 6..full.len() {
            let _ = parse(&full[..n]);
        }
        assert!(parse(b"").is_err());
        assert!(parse(b"\x06XXXX\x01").is_err());
    }
}
