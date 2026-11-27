//! The peers the node knows of: the configured one, those the DNS seeds give and those the peers
//! announce (`addr`, `addrv2`), with what each serves and how the connections to it went.
//!
//! Times are the caller's monotonic seconds.

use alloc::vec::Vec;
use core::net::{IpAddr, SocketAddr};

use bitcoin::Network;

use crate::p2p::services;

/// Where an address came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Set by the user: the bridge tried first, never forgotten nor banned.
    Configured,
    /// A DNS seed.
    Seed,
    /// A peer's `addr` or `addrv2`.
    Peer,
}

/// What a connection is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A Utreexo bridge: headers, then the blocks with their proofs.
    Bridge,
    /// Another peer: headers, block announcements and addresses.
    Peer,
}

/// How a connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not reached, or no handshake.
    Unreached,
    /// It ran, then ended: the peer left, or the link broke.
    Ended,
    /// It does not serve what its role needs.
    WrongRole,
    /// It sent invalid data.
    Misbehaved,
}

/// An address and what the node knows of it.
#[derive(Clone, Debug)]
pub struct Entry {
    pub addr: SocketAddr,
    /// What it serves: from its handshake once done, until then from where it came from.
    pub services: u64,
    pub source: Source,
    /// Handshakes done with it.
    pub handshakes: u32,
    /// Tries in a row that failed.
    pub failures: u32,
    /// Not tried again before this time.
    retry_at: u64,
    /// A connection to it is open, or being opened.
    in_use: bool,
}

/// The most addresses kept.
pub const CAPACITY: usize = 2000;

/// Tries that failed after which an address never reached is forgotten.
const FORGET_AFTER: u32 = 3;
/// How long a peer that sent invalid data is not tried again.
const BAN: u64 = 24 * 3600;
/// How long after a connection that ran the same peer is tried again.
const AFTER_ENDED: u64 = 30;
/// The same for the configured peer.
const AFTER_ENDED_CONFIGURED: u64 = 5;

/// A full node serving witnesses: what an ordinary peer must serve.
const FULL: u64 = services::NETWORK | services::WITNESS;

pub struct AddrBook {
    entries: Vec<Entry>,
    capacity: usize,
    /// Whether IPv6 addresses are kept (the node can reach them).
    ipv6: bool,
    rng: u64,
}

impl AddrBook {
    /// An empty book. `seed` drives the random picks.
    pub fn new(ipv6: bool, seed: u64) -> Self {
        Self {
            entries: Vec::new(),
            capacity: CAPACITY,
            ipv6,
            rng: seed,
        }
    }

    /// Adds an address, or merges the services of one already known that has not done its
    /// handshake yet. Addresses that are not publicly routable are refused unless configured.
    /// Returns whether it was new.
    pub fn add(&mut self, addr: SocketAddr, services: u64, source: Source) -> bool {
        if addr.port() == 0 || (addr.is_ipv6() && !self.ipv6) {
            return false;
        }
        if source != Source::Configured && !routable(addr.ip()) {
            return false;
        }
        if let Some(e) = self.entries.iter_mut().find(|e| e.addr == addr) {
            if source == Source::Configured {
                e.source = Source::Configured;
            }
            if e.handshakes == 0 {
                e.services |= services;
            }
            return false;
        }
        if self.entries.len() >= self.capacity && !self.evict() {
            return false;
        }
        self.entries.push(Entry {
            addr,
            services,
            source,
            handshakes: 0,
            failures: 0,
            retry_at: 0,
            in_use: false,
        });
        true
    }

    /// Drops an address to make room: one never reached if any, else another, at random; never
    /// the configured one nor one in use. Returns whether it did.
    fn evict(&mut self) -> bool {
        let spare = |e: &Entry| e.source != Source::Configured && !e.in_use;
        let unreached: Vec<usize> = (0..self.entries.len())
            .filter(|&i| spare(&self.entries[i]) && self.entries[i].handshakes == 0)
            .collect();
        let pool = if unreached.is_empty() {
            (0..self.entries.len())
                .filter(|&i| spare(&self.entries[i]))
                .collect()
        } else {
            unreached
        };
        match self.choose(&pool) {
            Some(i) => {
                self.entries.swap_remove(i);
                true
            }
            None => false,
        }
    }

    /// The next address to connect to for `role` at `now`, in use until [`AddrBook::release`]:
    /// the configured peer first for a bridge, else one at random, half the time among those
    /// already reached.
    pub fn pick(&mut self, role: Role, now: u64) -> Option<SocketAddr> {
        let fits = |e: &Entry| !e.in_use && e.retry_at <= now && suits(e, role);
        let configured = self
            .entries
            .iter()
            .position(|e| e.source == Source::Configured && fits(e));
        let i = match configured {
            Some(i) => i,
            None => {
                let all: Vec<usize> = (0..self.entries.len())
                    .filter(|&i| fits(&self.entries[i]))
                    .collect();
                let reached: Vec<usize> = all
                    .iter()
                    .copied()
                    .filter(|&i| self.entries[i].handshakes > 0)
                    .collect();
                let pool = if !reached.is_empty() && self.next() & 1 == 0 {
                    reached
                } else {
                    all
                };
                self.choose(&pool)?
            }
        };
        let e = &mut self.entries[i];
        e.in_use = true;
        Some(e.addr)
    }

    /// The handshake with `addr` is done: it serves `services`.
    pub fn handshake(&mut self, addr: SocketAddr, services: u64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.addr == addr) {
            e.handshakes += 1;
            e.failures = 0;
            e.services = services;
        }
    }

    /// The connection to `addr` ended with `outcome` at `now`.
    pub fn release(&mut self, addr: SocketAddr, outcome: Outcome, now: u64) {
        let Some(i) = self.entries.iter().position(|e| e.addr == addr) else {
            return;
        };
        let e = &mut self.entries[i];
        e.in_use = false;
        let configured = e.source == Source::Configured;
        match outcome {
            Outcome::Unreached => {
                e.failures += 1;
                if !configured && e.handshakes == 0 && e.failures >= FORGET_AFTER {
                    self.entries.swap_remove(i);
                    return;
                }
                e.retry_at = now + backoff(e);
            }
            Outcome::Ended => {
                e.retry_at = now
                    + if configured {
                        AFTER_ENDED_CONFIGURED
                    } else {
                        AFTER_ENDED
                    };
            }
            // its services, from its handshake, keep it from that role from now on
            Outcome::WrongRole => e.retry_at = now,
            Outcome::Misbehaved => {
                e.failures += 1;
                e.retry_at = now + if configured { backoff(e) } else { BAN };
            }
        }
    }

    /// The addresses known.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The addresses that suit `role`, in use or waiting included.
    pub fn count(&self, role: Role) -> usize {
        self.entries.iter().filter(|e| suits(e, role)).count()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// SplitMix64.
    fn next(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn choose(&mut self, pool: &[usize]) -> Option<usize> {
        if pool.is_empty() {
            return None;
        }
        Some(pool[(self.next() % pool.len() as u64) as usize])
    }
}

/// Whether `e` can serve `role`. Bridges, and the configured peer, are left to the bridge's
/// connection.
fn suits(e: &Entry, role: Role) -> bool {
    let bridge = e.source == Source::Configured || e.services & services::UTREEXO_ARCHIVE != 0;
    match role {
        Role::Bridge => bridge,
        Role::Peer => !bridge && e.services & FULL == FULL,
    }
}

/// How long to wait after a failure: doubling from 5 s up to a minute for the configured peer,
/// from a minute up to 4 hours for the others.
fn backoff(e: &Entry) -> u64 {
    let doublings = e.failures.saturating_sub(1).min(16);
    if e.source == Source::Configured {
        (5u64 << doublings).min(60)
    } else {
        (60u64 << doublings).min(4 * 3600)
    }
}

/// Whether `ip` is a public address: not private, loopback, link-local, shared (CGNAT),
/// multicast, documentation or reserved.
pub fn routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_multicast()
                || ip.is_documentation()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && o[1] & 0xc0 == 64))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || s[0] & 0xfe00 == 0xfc00
                || s[0] & 0xffc0 == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || ip.to_ipv4_mapped().is_some())
        }
    }
}

/// A DNS seed: a name that resolves to addresses of peers, all serving `services`.
#[derive(Clone, Copy, Debug)]
pub struct DnsSeed {
    pub name: &'static str,
    pub services: u64,
}

const fn seed(name: &'static str, services: u64) -> DnsSeed {
    DnsSeed { name, services }
}

/// A Utreexo bridge serving witnesses.
const BRIDGE: u64 = FULL | services::UTREEXO | services::UTREEXO_ARCHIVE;

const MAINNET: &[DnsSeed] = &[
    seed("manualseed.calvinkim.info", BRIDGE),
    seed("x9.seed.bitcoin.sipa.be", FULL),
    seed("x9.dnsseed.bluematt.me", FULL),
    seed("x9.dnsseed.bitcoin.dashjr-list-of-p2p-nodes.us", FULL),
    seed("x9.seed.bitcoin.jonasschnelli.ch", FULL),
    seed("x9.seed.btc.petertodd.net", FULL),
    seed("x9.seed.bitcoin.sprovoost.nl", FULL),
    seed("x9.dnsseed.emzy.de", FULL),
    seed("x9.seed.bitcoin.wiz.biz", FULL),
    seed("x9.seed.mainnet.achownodes.xyz", FULL),
];
const SIGNET: &[DnsSeed] = &[
    seed("x9.seed.signet.bitcoin.sprovoost.nl", FULL),
    seed("x9.seed.signet.achownodes.xyz", FULL),
];
const TESTNET4: &[DnsSeed] = &[
    seed("x9.seed.testnet4.bitcoin.sprovoost.nl", FULL),
    seed("seed.testnet4.wiz.biz", FULL),
];
const TESTNET: &[DnsSeed] = &[
    seed("x9.testnet-seed.bitcoin.jonasschnelli.ch", FULL),
    seed("x9.seed.tbtc.petertodd.net", FULL),
    seed("x9.seed.testnet.bitcoin.sprovoost.nl", FULL),
    seed("x9.testnet-seed.bluematt.me", FULL),
    seed("x9.seed.testnet.achownodes.xyz", FULL),
];

/// The DNS seeds of `network`: Bitcoin Core's, asked for full nodes with witnesses (`x9.`, which
/// they filter on), and on mainnet a seed of Utreexo bridges.
pub fn dns_seeds(network: Network) -> &'static [DnsSeed] {
    match network {
        Network::Bitcoin => MAINNET,
        Network::Signet => SIGNET,
        Network::Testnet4 => TESTNET4,
        Network::Testnet => TESTNET,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_and_reserved_addresses_are_refused_unless_configured() {
        let mut book = AddrBook::new(false, 1);
        for a in [
            "10.0.0.1:8333",
            "192.168.1.10:8333",
            "127.0.0.1:8333",
            "100.64.0.1:8333",
            "169.254.1.1:8333",
            "0.1.2.3:8333",
            "240.0.0.1:8333",
            "1.2.3.4:0",
        ] {
            assert!(!book.add(addr(a), FULL, Source::Peer), "{a}");
        }
        assert!(book.add(addr("10.0.2.2:18444"), 0, Source::Configured));
        assert!(book.add(addr("1.2.3.4:8333"), FULL, Source::Peer));
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn ipv6_is_kept_only_when_reachable() {
        let mut book = AddrBook::new(false, 1);
        assert!(!book.add(addr("[2a01:4f8::1]:8333"), FULL, Source::Peer));
        let mut book = AddrBook::new(true, 1);
        assert!(book.add(addr("[2a01:4f8::1]:8333"), FULL, Source::Peer));
        assert!(!book.add(addr("[2001:db8::1]:8333"), FULL, Source::Peer));
    }

    #[test]
    fn the_configured_bridge_comes_first_and_bridges_are_left_to_the_bridge() {
        let mut book = AddrBook::new(false, 7);
        book.add(addr("5.6.7.8:8333"), BRIDGE, Source::Peer);
        book.add(addr("1.2.3.4:8333"), FULL, Source::Seed);
        book.add(addr("1.231.31.3:8333"), 0, Source::Configured);
        assert_eq!(book.pick(Role::Bridge, 0), Some(addr("1.231.31.3:8333")));
        assert_eq!(book.pick(Role::Bridge, 0), Some(addr("5.6.7.8:8333")));
        assert_eq!(book.pick(Role::Bridge, 0), None);
        assert_eq!(book.pick(Role::Peer, 0), Some(addr("1.2.3.4:8333")));
        assert_eq!(book.pick(Role::Peer, 0), None);
        assert_eq!((book.count(Role::Bridge), book.count(Role::Peer)), (2, 1));
    }

    #[test]
    fn a_peer_unreached_three_times_is_forgotten() {
        let mut book = AddrBook::new(false, 1);
        let a = addr("1.2.3.4:8333");
        book.add(a, FULL, Source::Seed);
        let mut now = 0;
        for _ in 0..2 {
            assert_eq!(book.pick(Role::Peer, now), Some(a));
            book.release(a, Outcome::Unreached, now);
            // not before its backoff
            assert_eq!(book.pick(Role::Peer, now + 1), None);
            now += 4 * 3600;
        }
        assert_eq!(book.pick(Role::Peer, now), Some(a));
        book.release(a, Outcome::Unreached, now);
        assert!(book.is_empty());
    }

    #[test]
    fn the_configured_peer_is_retried_within_a_minute_and_never_forgotten() {
        let mut book = AddrBook::new(false, 1);
        let a = addr("1.231.31.3:8333");
        book.add(a, 0, Source::Configured);
        let mut now = 0;
        for wait in [5, 10, 20, 40, 60, 60] {
            assert_eq!(book.pick(Role::Bridge, now), Some(a));
            book.release(a, Outcome::Unreached, now);
            assert_eq!(book.pick(Role::Bridge, now + wait - 1), None);
            now += wait;
        }
        assert_eq!(book.pick(Role::Bridge, now), Some(a));
        book.release(a, Outcome::Misbehaved, now);
        assert_eq!(book.pick(Role::Bridge, now + 60), Some(a));
    }

    #[test]
    fn a_peer_that_misbehaved_waits_a_day() {
        let mut book = AddrBook::new(false, 1);
        let a = addr("1.2.3.4:8333");
        book.add(a, FULL, Source::Peer);
        assert_eq!(book.pick(Role::Peer, 0), Some(a));
        book.handshake(a, FULL);
        book.release(a, Outcome::Misbehaved, 0);
        assert_eq!(book.pick(Role::Peer, BAN - 1), None);
        assert_eq!(book.pick(Role::Peer, BAN), Some(a));
    }

    #[test]
    fn its_handshake_decides_what_a_peer_is_for() {
        let mut book = AddrBook::new(false, 1);
        let a = addr("5.6.7.8:8333");
        // a seed's guess
        book.add(a, BRIDGE, Source::Seed);
        assert_eq!(book.pick(Role::Bridge, 0), Some(a));
        book.handshake(a, FULL);
        book.release(a, Outcome::WrongRole, 0);
        assert_eq!(book.pick(Role::Bridge, 0), None);
        // an `addr` saying otherwise changes nothing once it did its handshake
        book.add(a, BRIDGE, Source::Peer);
        assert_eq!(book.pick(Role::Peer, 0), Some(a));
    }

    #[test]
    fn a_full_book_makes_room_but_keeps_the_configured_peer() {
        let mut book = AddrBook::new(false, 3);
        book.capacity = 3;
        book.add(addr("1.231.31.3:8333"), 0, Source::Configured);
        book.add(addr("1.1.1.1:8333"), FULL, Source::Peer);
        book.add(addr("1.1.1.2:8333"), FULL, Source::Peer);
        for i in 3..20 {
            assert!(book.add(addr(&format!("1.1.1.{i}:8333")), FULL, Source::Peer));
            assert_eq!(book.len(), 3);
        }
        assert!(book
            .entries()
            .iter()
            .any(|e| e.addr == addr("1.231.31.3:8333")));
    }
}
