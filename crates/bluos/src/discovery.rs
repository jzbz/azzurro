//! Finding players on the network.
//!
//! LSDP is a broadcast protocol, so discovery is: bind the port, shout, and
//! listen. Players also announce unprompted, which is why [`Discovery::recv`]
//! is worth leaving running rather than sweeping once at startup — a player
//! that was powered off when the app opened will announce itself when it wakes.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::error::Result;
use crate::lsdp::{self, Announce};

/// When to repeat the query within a sweep, in seconds from its start.
///
/// Front-loaded and then thinning out: the same shape the official controller
/// uses. A player that misses the first packet — asleep, or on a switch that
/// was still learning the broadcast group — gets six more chances without the
/// controller flooding the segment.
pub const QUERY_SCHEDULE: [u64; 7] = [0, 1, 2, 3, 5, 7, 10];

/// Long enough to cover the whole schedule with room for the last replies.
pub const DEFAULT_SWEEP: Duration = Duration::from_secs(12);

/// The receive buffer, big enough that no datagram can be cut short by it: a
/// UDP payload cannot exceed 65507 bytes.
///
/// It used to be 2048, which was "larger than any announcement observed" and
/// therefore an assumption about other people's firmware. A truncated read is
/// a lost announcement everywhere, and on Windows it is worse than that: an
/// oversized datagram fails the read with `WSAEMSGSIZE` instead of being
/// quietly cut, and that error used to end discovery for the session.
const RECEIVE_BUFFER: usize = 65535;

/// How many failed reads in a row mean the socket itself is gone.
///
/// A single failure is a property of one datagram, not of the socket —
/// `WSAEMSGSIZE` for an oversized one, `WSAECONNRESET` when a host answers a
/// broadcast with ICMP port-unreachable — so reads carry on. A socket whose
/// interface has gone away fails every time instead, and there the caller
/// wants to hear about it and bind a new one rather than spin.
const FAILURES_MEANING_THE_SOCKET: u32 = 16;

pub struct Discovery {
    /// Replaceable, because a socket can stop working in ways only a fresh one
    /// recovers from, and everything that holds a `Discovery` — the listening
    /// loop, and Rescan — should keep talking to the one that works. The guard
    /// is held only long enough to clone the handle out; nothing awaits under
    /// it.
    socket: Mutex<Arc<UdpSocket>>,
    /// Reads that have failed, for the tests alone.
    ///
    /// The error arm of [`Discovery::recv`] is reached by asking the kernel
    /// for an error, which not every machine will produce: a container with
    /// ICMP suppressed, or a platform that reports the failure on the send
    /// instead, leaves a test that only checks the announcement arrives
    /// passing while exercising none of it. Counting the failures is what lets
    /// a test say the arm ran rather than assume it.
    #[cfg(test)]
    failed_reads: std::sync::atomic::AtomicU32,
}

impl Discovery {
    /// Bind the LSDP port.
    pub fn bind() -> Result<Self> {
        Ok(Self {
            socket: Mutex::new(Arc::new(open()?)),
            #[cfg(test)]
            failed_reads: std::sync::atomic::AtomicU32::new(0),
        })
    }

    /// Bind a fresh socket in place of the current one.
    ///
    /// For a caller whose reads have started failing on every attempt: the
    /// interface the socket was bound through may have gone, and nothing but a
    /// new socket brings discovery back. The old one is dropped once the reads
    /// still holding it have returned, and the port options set by [`open`]
    /// are what let the two overlap.
    pub fn rebind(&self) -> Result<()> {
        let fresh = open()?;
        *self.socket.lock().unwrap() = Arc::new(fresh);
        Ok(())
    }

    /// The socket to use for one operation.
    ///
    /// Cloned out rather than borrowed so that a [`Discovery::rebind`] during a
    /// long read has something to replace: the read finishes against the socket
    /// it started on.
    fn socket(&self) -> Arc<UdpSocket> {
        self.socket.lock().unwrap().clone()
    }

    /// The broadcast addresses queries go to, one per usable interface.
    ///
    /// Read afresh every time rather than kept from the bind. A laptop that
    /// closes its lid on one network and opens it on another keeps this socket
    /// and everything above it, so a list from the old subnet would be
    /// broadcast to for the rest of the session — including on Rescan, which is
    /// exactly the button somebody presses after changing network.
    pub fn targets(&self) -> Vec<Ipv4Addr> {
        broadcast_targets()
    }

    /// Ask every player on every interface to announce itself.
    ///
    /// A send failure on one interface is logged and skipped rather than
    /// failing the query: a machine with a virtual or downed interface should
    /// still discover players on the one that works. Note that a firewall
    /// silently dropping broadcast traffic does not surface as an error here —
    /// it looks exactly like a network with no players on it.
    ///
    /// Infallible, and that is the point rather than an accident of the body.
    /// This used to hand back a `Result` that only ever held `Ok`, and
    /// [`Discovery::sweep_with`] took it with `?` — so the day somebody made a
    /// send failure worth reporting, one downed interface would have abandoned
    /// the remaining broadcasts and every player that had not answered yet.
    /// The return type says there is nothing there to abandon a sweep over.
    pub async fn query(&self) {
        let socket = self.socket();
        for target in self.targets() {
            let to = SocketAddr::from(SocketAddrV4::new(target, lsdp::PORT));
            if let Err(e) = socket.send_to(&lsdp::QUERY, to).await {
                tracing::debug!(%target, "LSDP query failed: {e}");
            }
        }
    }

    /// Wait for the next packet that carries announcements, and return them.
    ///
    /// Packets that carry none are skipped rather than returned empty. That
    /// covers the controller's own query looping back on the socket it was
    /// sent from, which happens on every broadcast.
    ///
    /// So is a read that fails: on a broadcast socket an error belongs to one
    /// datagram far more often than to the socket, and returning it used to end
    /// listening for the session. Only a run of them — see
    /// [`FAILURES_MEANING_THE_SOCKET`] — comes back as an error, which is the
    /// caller's cue to [`Discovery::rebind`] rather than to give up.
    pub async fn recv(&self) -> Result<Vec<Announce>> {
        let socket = self.socket();
        // On the heap: this future is held across the `select!` in
        // `sweep_with` and for as long as a listening loop is waiting, and a
        // buffer this size is not something to be moving about with it.
        let mut buf = vec![0u8; RECEIVE_BUFFER];
        let mut failures = 0;
        loop {
            let (n, from) = match socket.recv_from(&mut buf).await {
                Ok(read) => {
                    failures = 0;
                    read
                }
                Err(e) => {
                    failures += 1;
                    #[cfg(test)]
                    self.failed_reads
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if failures >= FAILURES_MEANING_THE_SOCKET {
                        return Err(e.into());
                    }
                    tracing::debug!("LSDP receive failed: {e}");
                    continue;
                }
            };
            match lsdp::parse(&buf[..n]) {
                Ok(decoded) => {
                    // Said here rather than in the decoder, which does not know
                    // which host it is reading: with several players answering
                    // at once, the address is the whole of what makes this
                    // worth printing.
                    if let Some(e) = decoded.skipped {
                        tracing::debug!(%from, "LSDP packet only partly decodable: {e}");
                    }
                    if !decoded.announces.is_empty() {
                        return Ok(decoded.announces);
                    }
                }
                Err(e) => tracing::debug!(%from, "undecodable LSDP packet: {e}"),
            }
        }
    }

    /// Run the query schedule for `window` and return everything that answered,
    /// one entry per device.
    ///
    /// This is the one-shot form, for a command line or a cold start. A running
    /// app should call [`Discovery::query`] once and then keep [`Discovery::recv`]
    /// in a loop, so that arrivals and address changes land without another sweep.
    pub async fn sweep(&self, window: Duration) -> Result<Vec<Announce>> {
        let mut found = Vec::new();
        self.sweep_with(window, |announce| found.push(announce.clone()))
            .await?;
        Ok(found)
    }

    /// The same sweep, handing each player over as it answers.
    ///
    /// The schedule below spreads its broadcasts across the window because a
    /// single one is dropped often enough to matter and a sleeping player takes
    /// a moment to reply. That is a reason to keep listening for twelve
    /// seconds; it is not a reason to sit on an answer that arrived in the
    /// first tenth of one, which is what collecting into a vector and returning
    /// it at the end did — nothing appeared in the window until the whole
    /// schedule had run.
    ///
    /// Each node is handed over once. A player answering three broadcasts is
    /// still one player — see [`worth_handing_over`] for the one exception.
    pub async fn sweep_with(
        &self,
        window: Duration,
        mut found_one: impl FnMut(&Announce),
    ) -> Result<()> {
        let start = Instant::now();
        let mut pending: VecDeque<Duration> = QUERY_SCHEDULE
            .iter()
            .map(|s| Duration::from_secs(*s) + jitter())
            .collect();
        let mut seen: BTreeMap<Vec<u8>, bool> = BTreeMap::new();

        loop {
            let elapsed = start.elapsed();
            let Some(remaining) = window.checked_sub(elapsed) else {
                break;
            };

            // Fire any query whose slot has arrived before going back to sleep.
            if let Some(at) = pending.front().copied()
                && at <= elapsed
            {
                pending.pop_front();
                self.query().await;
                continue;
            }

            let wake = pending
                .front()
                .map(|at| *at - elapsed)
                .unwrap_or(remaining)
                .min(remaining);

            tokio::select! {
                _ = tokio::time::sleep(wake) => {}
                result = self.recv() => {
                    // A sweep is bounded by its window, so a socket that has
                    // stopped working ends it here with whatever answered
                    // before it did, rather than spending the rest of the
                    // window failing. The caller of a sweep is a cold start:
                    // it is the listening loop afterwards that rebinds.
                    let announces = match result {
                        Ok(announces) => announces,
                        Err(e) => {
                            tracing::warn!("LSDP socket stopped receiving mid-sweep: {e}");
                            break;
                        }
                    };
                    for announce in announces {
                        if worth_handing_over(&mut seen, &announce) {
                            found_one(&announce);
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

/// Whether this announcement is one the caller has not already been given.
///
/// `seen` is keyed by node id and holds whether what was handed over for it was
/// *usable* — carried a control API, the only kind of announcement a caller can
/// do anything with. That second piece is the point: LSDP is an unauthenticated
/// broadcast, so the first announcement bearing a node id is not necessarily
/// that node's own, and a forged or partial one used to claim the id and
/// suppress the player's real answer for the rest of the sweep. Only a usable
/// announcement settles an id now. An unusable one is still handed over once,
/// since deciding for the caller what is worth seeing is not this function's
/// job, but a usable one after it is handed over too.
fn worth_handing_over(seen: &mut BTreeMap<Vec<u8>, bool>, announce: &Announce) -> bool {
    let usable = announce.player().is_some();
    match seen.entry(announce.node_id.clone()) {
        Entry::Vacant(slot) => {
            slot.insert(usable);
            true
        }
        Entry::Occupied(mut slot) => {
            let settled = *slot.get();
            if usable && !settled {
                slot.insert(true);
                true
            } else {
                false
            }
        }
    }
}

/// Bind a socket on the LSDP port, configured to share it.
///
/// The port is shared: `SO_REUSEADDR`, plus `SO_REUSEPORT` wherever there is
/// one, so that this can run alongside the official controller instead of one
/// of them failing to start. That is what the official app does too, and it
/// makes debugging against a known-good client possible. It is also what lets
/// [`Discovery::rebind`] open the replacement while the old socket is still
/// open.
///
/// `SO_REUSEPORT` was gated to Linux, which was too narrow. On the BSDs — macOS
/// among them — `SO_REUSEADDR` alone does not let two sockets wildcard-bind one
/// UDP port; only a multicast address gets that exemption, and this binds a
/// unicast wildcard. Without it the second controller to start gets
/// `EADDRINUSE`, and since the option has to be set on *both* sockets for
/// either to share, whichever of the two came up first would lock the other
/// out. Windows has no such option at all and does not need one: its
/// `SO_REUSEADDR` already permits the duplicate bind.
///
/// The gate mirrors socket2's own for `set_reuse_port` rather than naming
/// platforms, so this compiles exactly where the method is defined.
fn open() -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(all(
        unix,
        not(any(
            target_os = "solaris",
            target_os = "illumos",
            target_os = "cygwin",
            target_os = "nuttx",
            target_os = "wasi"
        ))
    ))]
    socket.set_reuse_port(true)?;
    socket.set_broadcast(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, lsdp::PORT).into())?;
    socket.set_nonblocking(true)?;

    Ok(UdpSocket::from_std(socket.into())?)
}

/// Whether `address` is on a subnet this machine is attached to.
///
/// Used before re-probing a remembered player: a list of addresses from the
/// home network is worth nothing on a hotel wifi, and quietly trying them all
/// wastes a connection attempt each. The official controller filters its own
/// stored list the same way.
pub fn is_local(address: IpAddr) -> bool {
    let IpAddr::V4(address) = address else {
        return false;
    };
    let address = u32::from(address);

    if_addrs::get_if_addrs()
        .into_iter()
        .flatten()
        .filter(|iface| !iface.is_loopback())
        .any(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) => {
                let mask = u32::from(v4.netmask);
                // A /32 — a point-to-point or VPN interface — matches only
                // itself, which is the right answer rather than a special case.
                (u32::from(v4.ip) & mask) == (address & mask)
            }
            if_addrs::IfAddr::V6(_) => false,
        })
}

/// The broadcast address of every usable IPv4 interface.
///
/// Derived as `ip | !netmask` rather than read from the interface's own
/// broadcast field, because a point-to-point interface may not have one and
/// this is what the official controller computes. Loopback and non-IPv4
/// interfaces are dropped; if that leaves nothing, fall back to the limited
/// broadcast address, which at least reaches a directly attached segment.
fn broadcast_targets() -> Vec<Ipv4Addr> {
    targets_from(
        if_addrs::get_if_addrs()
            .into_iter()
            .flatten()
            .filter(|iface| !iface.is_loopback())
            .filter_map(|iface| match iface.addr {
                if_addrs::IfAddr::V4(v4) => Some((v4.ip, v4.netmask)),
                if_addrs::IfAddr::V6(_) => None,
            }),
    )
}

/// The same derivation, over addresses from anywhere, so that it can be checked
/// against interfaces this machine does not have.
fn targets_from(addresses: impl Iterator<Item = (Ipv4Addr, Ipv4Addr)>) -> Vec<Ipv4Addr> {
    let mut targets: Vec<Ipv4Addr> = addresses
        .map(|(ip, netmask)| Ipv4Addr::from(u32::from(ip) | !u32::from(netmask)))
        .collect();

    targets.sort_unstable();
    targets.dedup();

    if targets.is_empty() {
        targets.push(Ipv4Addr::BROADCAST);
    }
    targets
}

/// Up to 250ms of spread, so that several controllers starting at once — or one
/// restarting in a loop — do not land their queries on the same millisecond.
///
/// Taken from the clock rather than a PRNG to keep a random-number generator
/// out of the dependency graph for something this undemanding.
fn jitter() -> Duration {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    Duration::from_millis((nanos % 250) as u64)
}

#[cfg(test)]
impl Discovery {
    /// A discovery over a socket the caller chose, so that the receive loop can
    /// be exercised on loopback. [`Discovery::bind`] takes the LSDP port on
    /// every interface and broadcasts to the network, which is not something a
    /// test should do.
    fn over(socket: UdpSocket) -> Self {
        Self {
            socket: Mutex::new(Arc::new(socket)),
            failed_reads: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// The same swap [`Discovery::rebind`] makes, onto a socket the caller
    /// chose. `rebind` binds the LSDP port on every interface, which is not
    /// something a test should do; what is worth testing is that the swap
    /// takes — that reads afterwards come off the new socket while the old one
    /// is still open.
    fn rebind_over(&self, socket: UdpSocket) {
        *self.socket.lock().unwrap() = Arc::new(socket);
    }

    /// How many reads have failed so far. See [`Discovery::failed_reads`].
    fn failures(&self) -> u32 {
        self.failed_reads.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsdp::Record;

    /// The six-byte LSDP header, with byte 0 pointing at the first message.
    const HEADER: [u8; 6] = [0x06, b'L', b'S', b'D', b'P', 0x01];

    /// One announce message: node id `aa:bb:cc:dd`, address 192.0.2.155, one
    /// record of class 0:1 — a player — carrying no TXT pairs.
    const ANNOUNCE: [u8; 16] = [
        0x10, b'A', // length and type
        0x04, 0xaa, 0xbb, 0xcc, 0xdd, // node id
        0x04, 192, 0, 2, 155, // address
        0x01, 0x00, 0x01, 0x00, // one record, class 0:1, no TXT
    ];

    fn announce(node_id: &[u8], classes: &[(u8, u8)]) -> Announce {
        Announce {
            node_id: node_id.to_vec(),
            address: Ipv4Addr::new(192, 0, 2, 155),
            records: classes
                .iter()
                .map(|class| Record {
                    class: *class,
                    txt: BTreeMap::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_player_answering_every_broadcast_is_still_one_player() {
        let mut seen = BTreeMap::new();
        let powernode = announce(b"node", &[(0, 1)]);

        assert!(worth_handing_over(&mut seen, &powernode));
        assert!(!worth_handing_over(&mut seen, &powernode));
        assert!(!worth_handing_over(&mut seen, &powernode));
    }

    #[test]
    fn an_unusable_announcement_does_not_settle_a_node() {
        let mut seen = BTreeMap::new();
        // Anyone on the segment can send this: the player's node id, and no
        // control API to reach it by. It arrives first because the forger is
        // not waiting for a broadcast.
        let forged = announce(b"node", &[(0, 4)]);
        let real = announce(b"node", &[(0, 1)]);

        assert!(worth_handing_over(&mut seen, &forged));
        // The whole point: before this, the id was already claimed and the
        // player's own answer was dropped for the rest of the sweep.
        assert!(worth_handing_over(&mut seen, &real), "the real one gets in");
        // And having got in, it settles the id.
        assert!(!worth_handing_over(&mut seen, &real));
        assert!(!worth_handing_over(&mut seen, &forged));
    }

    #[test]
    fn broadcast_addresses_come_off_the_netmasks() {
        let targets = targets_from(
            [
                ("192.168.1.5", "255.255.255.0"),
                // A second address on the same subnet is the same target.
                ("192.168.1.9", "255.255.255.0"),
                ("10.1.2.3", "255.255.0.0"),
                // A /32 — a VPN or point-to-point link — is its own target.
                ("10.8.0.2", "255.255.255.255"),
            ]
            .into_iter()
            .map(|(ip, mask)| (ip.parse().unwrap(), mask.parse().unwrap())),
        );

        assert_eq!(
            targets,
            ["10.1.255.255", "10.8.0.2", "192.168.1.255"]
                .map(|a| a.parse::<Ipv4Addr>().unwrap())
                .to_vec()
        );
    }

    #[test]
    fn locality_follows_the_interfaces() {
        // Loopback is deliberately not "local": nothing is discovered there,
        // and a player reporting 127.0.0.1 is reporting its own view, not ours.
        assert!(!is_local("127.0.0.1".parse().unwrap()));
        // Documentation space is not on anybody's LAN.
        assert!(!is_local("192.0.2.155".parse().unwrap()));
        // IPv6 is not handled at all, and says so rather than guessing.
        assert!(!is_local("::1".parse().unwrap()));
    }

    #[test]
    fn always_has_somewhere_to_broadcast() {
        assert!(!broadcast_targets().is_empty());
    }

    #[test]
    fn jitter_stays_inside_its_budget() {
        for _ in 0..100 {
            assert!(jitter() < Duration::from_millis(250));
        }
    }

    /// A packet with `padding` bytes of unknown messages in front of one
    /// announcement, so that a buffer smaller than the whole packet keeps the
    /// padding and loses the player.
    fn padded_packet(padding: usize) -> Vec<u8> {
        let mut packet = HEADER.to_vec();
        while packet.len() < padding {
            // One message of an unknown type, which the decoder steps over
            // using the length on the wire.
            packet.push(0xff);
            packet.push(b'Z');
            packet.resize(packet.len() + 0xff - 2, 0);
        }
        packet.extend_from_slice(&ANNOUNCE);
        packet
    }

    /// Loopback only: nothing here binds the LSDP port or broadcasts.
    async fn listening() -> (Discovery, UdpSocket, SocketAddr) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        (Discovery::over(socket), sender, addr)
    }

    #[tokio::test]
    async fn a_datagram_bigger_than_the_old_buffer_is_read_whole() {
        let (discovery, sender, addr) = listening().await;
        let packet = padded_packet(4000);
        sender.send_to(&packet, addr).await.unwrap();

        // With the old 2048-byte buffer the read stopped inside the padding
        // and the announcement behind it was never seen, so this waits for a
        // packet that has already been and gone.
        let announces = tokio::time::timeout(Duration::from_secs(5), discovery.recv())
            .await
            .expect("the announcement is in the datagram")
            .unwrap();

        assert_eq!(announces.len(), 1);
        assert_eq!(announces[0].address, Ipv4Addr::new(192, 0, 2, 155));
    }

    /// Whether this machine tells a connected UDP socket that its peer is not
    /// there — the behaviour the test below is built on.
    ///
    /// Not every machine does. A container can have ICMP suppressed, and a
    /// platform is free to surface the condition on the `send` instead of on
    /// the next read. Either has nothing for that test to exercise, which is a
    /// reason to skip it rather than to fail: an environment the code cannot
    /// affect should not read as a regression in the code.
    async fn port_unreachable_reaches_the_reader() -> bool {
        let dead = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = dead.local_addr().unwrap();
        drop(dead);

        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        if socket.connect(peer).await.is_err() || socket.send(b"nobody is there").await.is_err() {
            return false;
        }
        // The kernel has to generate the ICMP and queue it against the socket,
        // which is a round trip rather than something already waiting.
        let mut buf = [0u8; 64];
        matches!(
            tokio::time::timeout(Duration::from_millis(500), socket.recv(&mut buf)).await,
            Ok(Err(_))
        )
    }

    #[tokio::test]
    async fn a_failed_read_does_not_end_the_listening() {
        if !port_unreachable_reaches_the_reader().await {
            eprintln!(
                "skipping: this machine does not report a UDP port-unreachable \
                 to the reader, so there is no failed read to recover from"
            );
            return;
        }

        // A connected UDP socket is told when its peer is not there: the send
        // below draws an ICMP port-unreachable, which the next read reports as
        // an error. That is the loopback stand-in for the Windows pair
        // (WSAECONNRESET, WSAEMSGSIZE) that used to end discovery for good.
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = sender.local_addr().unwrap();
        drop(sender);

        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        socket.connect(peer).await.unwrap();
        socket.send(b"nobody is there").await.unwrap();
        let discovery = Discovery::over(socket);

        // The failed read is drawn out before the announcement is sent rather
        // than alongside it. The probe above says this machine reports the
        // port-unreachable to the reader; it says nothing about when, and on
        // macOS the announcement can be read first, leaving the error queued
        // behind it and the read that was meant to fail never failing. This
        // call parks on the socket once it has counted whatever is waiting, so
        // the timeout elapsing is the expected end of it.
        let _ = tokio::time::timeout(Duration::from_millis(500), discovery.recv()).await;

        // A machine that reported one to the probe and none to the reader has
        // nothing for the rest of this to exercise, and that is the platform's
        // business rather than a regression here.
        if discovery.failures() == 0 {
            eprintln!(
                "skipping: the port-unreachable never reached the reader, so \
                 there is no failed read to recover from"
            );
            return;
        }

        // Then the peer comes back and announces, from the one address a
        // connected socket will accept.
        let sender = UdpSocket::bind(peer).await.unwrap();
        sender.send_to(&padded_packet(0), addr).await.unwrap();

        let announces = tokio::time::timeout(Duration::from_secs(5), discovery.recv())
            .await
            .expect("a failed read is not the end of the socket")
            .expect("nor is it handed back as one");
        assert_eq!(announces.len(), 1);
    }

    /// A run of failures is the socket itself, and comes back as an error.
    ///
    /// That is the whole point of counting them: the caller takes the error as
    /// its cue to bind a fresh socket. Without a threshold this would loop for
    /// ever on a socket that can never read again, which is what a laptop
    /// waking on another network leaves behind.
    #[tokio::test]
    async fn a_socket_that_fails_every_read_is_handed_back_as_an_error() {
        // The same ICMP the test above uses, kept coming: each datagram to a
        // port nobody is listening on draws one, and a connected socket
        // reports it to the next read.
        let dead = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = dead.local_addr().unwrap();
        drop(dead);

        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(peer).await.unwrap();
        let discovery = Discovery::over(socket);

        // From the socket `recv` is reading, so every one of these is another
        // error for it to find.
        let poking = discovery.socket();
        let poker = tokio::spawn(async move {
            loop {
                let _ = poking.send(b"nobody is there").await;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });

        let ended = tokio::time::timeout(Duration::from_secs(10), discovery.recv())
            .await
            .expect("a socket that fails every read must not loop for ever");
        poker.abort();

        assert!(
            ended.is_err(),
            "a run of failed reads is the caller's cue to re-bind"
        );
        assert!(
            discovery.failures() >= FAILURES_MEANING_THE_SOCKET,
            "it gave up after {} reads rather than {FAILURES_MEANING_THE_SOCKET}",
            discovery.failures()
        );
    }

    /// Re-binding puts a working socket under everything that holds the
    /// `Discovery`, without any of them being handed a new one.
    ///
    /// The listening loop, Rescan and a sweep all share one `Discovery`, so
    /// recovery has to happen inside it: a read after the swap comes off the
    /// new socket. The old one is still open at that point — a read that was
    /// already waiting finishes against the socket it started on — which is
    /// what the port options in `open` are for.
    #[tokio::test]
    async fn a_rebound_socket_is_the_one_that_is_read_from() {
        let (discovery, sender, old) = listening().await;

        let fresh = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let new = fresh.local_addr().unwrap();
        discovery.rebind_over(fresh);

        // Sent to the address the old socket answers on. Nothing must come of
        // it: that socket is not the one being read any more.
        sender.send_to(&padded_packet(0), old).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), discovery.recv())
                .await
                .is_err(),
            "the old socket is still open, but it is not the one being read"
        );

        sender.send_to(&padded_packet(0), new).await.unwrap();
        let announces = tokio::time::timeout(Duration::from_secs(5), discovery.recv())
            .await
            .expect("the fresh socket is the one that is read from")
            .unwrap();
        assert_eq!(announces.len(), 1);
    }
}
