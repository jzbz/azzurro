//! Finding players on the network.
//!
//! LSDP is a broadcast protocol, so discovery is: bind the port, shout, and
//! listen. Players also announce unprompted, which is why [`Discovery::recv`]
//! is worth leaving running rather than sweeping once at startup — a player
//! that was powered off when the app opened will announce itself when it wakes.

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
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

pub struct Discovery {
    socket: UdpSocket,
    targets: Vec<Ipv4Addr>,
}

impl Discovery {
    /// Bind the LSDP port and work out where to broadcast.
    ///
    /// The port is shared: `SO_REUSEADDR`, plus `SO_REUSEPORT` wherever there
    /// is one, so that this can run alongside the official controller instead
    /// of one of them failing to start. That is what the official app does
    /// too, and it makes debugging against a known-good client possible.
    ///
    /// `SO_REUSEPORT` was gated to Linux, which was too narrow. On the BSDs —
    /// macOS among them — `SO_REUSEADDR` alone does not let two sockets
    /// wildcard-bind one UDP port; only a multicast address gets that
    /// exemption, and this binds a unicast wildcard. Without it the second
    /// controller to start gets `EADDRINUSE`, and since the option has to be
    /// set on *both* sockets for either to share, whichever of the two came up
    /// first would lock the other out. Windows has no such option at all and
    /// does not need one: its `SO_REUSEADDR` already permits the duplicate
    /// bind.
    ///
    /// The gate mirrors socket2's own for `set_reuse_port` rather than naming
    /// platforms, so this compiles exactly where the method is defined.
    pub fn bind() -> Result<Self> {
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

        Ok(Self {
            socket: UdpSocket::from_std(socket.into())?,
            targets: broadcast_targets(),
        })
    }

    /// The broadcast addresses queries go to, one per usable interface.
    pub fn targets(&self) -> &[Ipv4Addr] {
        &self.targets
    }

    /// Ask every player on every interface to announce itself.
    ///
    /// A send failure on one interface is logged and skipped rather than
    /// failing the query: a machine with a virtual or downed interface should
    /// still discover players on the one that works. Note that a firewall
    /// silently dropping broadcast traffic does not surface as an error here —
    /// it looks exactly like a network with no players on it.
    pub async fn query(&self) -> Result<()> {
        for target in &self.targets {
            let to = SocketAddr::from(SocketAddrV4::new(*target, lsdp::PORT));
            if let Err(e) = self.socket.send_to(&lsdp::QUERY, to).await {
                tracing::debug!(%target, "LSDP query failed: {e}");
            }
        }
        Ok(())
    }

    /// Wait for the next packet that carries announcements, and return them.
    ///
    /// Packets that carry none are skipped rather than returned empty. That
    /// covers the controller's own query looping back on the socket it was
    /// sent from, which happens on every broadcast.
    pub async fn recv(&self) -> Result<Vec<Announce>> {
        // Larger than any announcement observed; a player listing many
        // services is still far short of this.
        let mut buf = [0u8; 2048];
        loop {
            let (n, from) = self.socket.recv_from(&mut buf).await?;
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
    /// still one player.
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
        let mut seen: BTreeMap<Vec<u8>, ()> = BTreeMap::new();

        loop {
            let elapsed = start.elapsed();
            let Some(remaining) = window.checked_sub(elapsed) else {
                break;
            };

            // Fire any query whose slot has arrived before going back to sleep.
            if let Some(at) = pending.front().copied() {
                if at <= elapsed {
                    pending.pop_front();
                    self.query().await?;
                    continue;
                }
            }

            let wake = pending
                .front()
                .map(|at| *at - elapsed)
                .unwrap_or(remaining)
                .min(remaining);

            tokio::select! {
                _ = tokio::time::sleep(wake) => {}
                result = self.recv() => {
                    for announce in result? {
                        if seen.insert(announce.node_id.clone(), ()).is_none() {
                            found_one(&announce);
                        }
                    }
                }
            }
        }

        Ok(())
    }
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
    let mut targets: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .into_iter()
        .flatten()
        .filter(|iface| !iface.is_loopback())
        .filter_map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) => {
                let ip = u32::from(v4.ip);
                let mask = u32::from(v4.netmask);
                Some(Ipv4Addr::from(ip | !mask))
            }
            if_addrs::IfAddr::V6(_) => None,
        })
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
mod tests {
    use super::*;

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
}
