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

/// What a packet decoded to.
#[derive(Debug, Default)]
pub struct Decoded {
    /// Every announcement it carried.
    pub announces: Vec<Announce>,
    /// The first thing in it that did not decode, if anything did not.
    ///
    /// Handed back rather than logged here, because this function cannot say
    /// anything useful about it: it never sees the address the packet came
    /// from, and a complaint about a packet with no host in it is not
    /// actionable. That is the same rule the errors in this crate follow, and
    /// [`Discovery::recv`](crate::discovery::Discovery::recv) is where both
    /// end up.
    pub skipped: Option<Error>,
}

/// Decode a packet into the announcements it carries.
///
/// Messages of a type this crate does not handle — including the `Q` a
/// controller hears when its own broadcast loops back — are skipped using the
/// on-the-wire length, so an unknown type costs nothing and an empty result is
/// normal rather than an error.
///
/// A message that is malformed costs no more than one that is unknown. Only
/// the header can fail the whole packet: past it, every message says its own
/// length, so one that does not decode is stepped over and one that says a
/// length nobody can follow ends the walk with whatever was read before it.
/// Padding on the end of a packet arrives as exactly that — a zero length,
/// which is not a message — and it should not cost the announcement in front
/// of it. What went wrong comes back in [`Decoded::skipped`] rather than
/// replacing what was read.
pub fn parse(packet: &[u8]) -> Result<Decoded> {
    if packet.len() < 6 {
        return Err(Error::Lsdp("shorter than a header"));
    }
    if &packet[1..5] != b"LSDP" {
        return Err(Error::Lsdp("missing the LSDP magic word"));
    }

    // Byte 0 is the header length and doubles as the offset of the first
    // message, which is how the official decoder finds it.
    let mut pos = packet[0] as usize;
    let mut out = Decoded::default();
    // Only the first is kept: one line naming what went wrong is a
    // diagnostic, and a line per malformed message in a hostile packet is a
    // flood with the same content.
    let note = |out: &mut Decoded, e| {
        if out.skipped.is_none() {
            out.skipped = Some(e);
        }
    };

    while pos + 1 < packet.len() {
        let len = packet[pos] as usize;
        if len < 2 {
            note(&mut out, Error::Lsdp("message length below its own header"));
            break;
        }
        let Some(end) = pos.checked_add(len).filter(|e| *e <= packet.len()) else {
            note(&mut out, Error::Lsdp("message length ran past the end"));
            break;
        };

        // Bound the body to this message before reading it, so a lying inner
        // length cannot walk into the next message. That bound is also what
        // makes skipping a bad one safe: `end` came from the framing, not from
        // anything the body claimed.
        if packet[pos + 1] == b'A' {
            match parse_announce(&packet[pos + 2..end]) {
                Ok(announce) => out.announces.push(announce),
                Err(e) => note(&mut out, e),
            }
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
        let decoded = parse(&bytes(POWERNODE)).unwrap();
        assert!(decoded.skipped.is_none(), "a real packet decodes whole");
        let announces = decoded.announces;
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
        let decoded = parse(&QUERY).unwrap();
        assert!(decoded.announces.is_empty());
        // Carrying no announcement is not the same as being undecodable: an
        // unknown message type is stepped over, and nothing is reported.
        assert!(decoded.skipped.is_none());
    }

    #[test]
    fn one_bad_message_does_not_cost_the_good_ones() {
        let full = bytes(POWERNODE);
        let (header, message) = full.split_at(full[0] as usize);
        assert_eq!(
            message.len(),
            message[0] as usize,
            "the fixture is one message"
        );

        // Padding on the end of a packet reads as a zero-length message, which
        // is not a message at all. It should not cost the announcement in
        // front of it.
        let mut padded = full.clone();
        padded.extend_from_slice(&[0, 0]);
        let decoded = parse(&padded).unwrap();
        assert_eq!(decoded.announces.len(), 1, "padding is not a message");
        assert!(decoded.skipped.is_some(), "but it is still worth saying so");

        // An announcement that does not decode. Its own length still says
        // where it ends, so the one beside it is still findable.
        let bad = [0x06, b'A', 0xff, 0xff, 0xff, 0xff];

        let mut after = header.to_vec();
        after.extend_from_slice(message);
        after.extend_from_slice(&bad);
        let decoded = parse(&after).unwrap();
        assert_eq!(decoded.announces.len(), 1, "a bad one after a good one");
        assert!(decoded.skipped.is_some());

        let mut before = header.to_vec();
        before.extend_from_slice(&bad);
        before.extend_from_slice(message);
        let decoded = parse(&before).unwrap();
        assert_eq!(decoded.announces.len(), 1, "and before it");
        assert!(decoded.skipped.is_some());
    }

    #[test]
    fn a_cut_packet_yields_nothing_rather_than_half_an_announcement() {
        let full = bytes(POWERNODE);
        for n in 6..full.len() {
            let decoded = parse(&full[..n]).expect("a whole header is still a header");
            // The one message is cut, so there is nothing to be had — and the
            // thing that must never happen is an announcement assembled out of
            // half of one.
            assert!(decoded.announces.is_empty(), "cut at {n}");
        }

        // A cut that leaves a message header behind says the packet was cut.
        // One that leaves less than a header has no message to complain about
        // — the walk needs a length and a type byte before there is anything
        // there at all, which is also why a single byte of padding is silent.
        assert!(parse(&full[..8]).unwrap().skipped.is_some());
        assert!(parse(&full[..7]).unwrap().skipped.is_none());

        // Short of a header, or carrying somebody else's, is still an error:
        // there is no framing to walk and nothing to report about.
        assert!(parse(b"").is_err());
        assert!(parse(b"\x06XXXX\x01").is_err());
    }
}
