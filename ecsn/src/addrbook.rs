//! The peers the node knows of: the configured one, those the DNS seeds give and those the peers
//! announce (`addr`, `addrv2`), with what each serves and how the connections to it went.
//!
//! Times are the caller's monotonic seconds.
//!
//! The book can be saved and loaded again across restarts ([`AddrBook::encode`],
//! [`AddrBook::load`]): its most useful entries within a size, what each serves and how the
//! connections to it went, not when to retry it.

use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

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
    /// A bridge that lacked the blocks of the best header chain for long.
    Behind,
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
    /// Its v2 handshake failed: v1 only.
    pub v1_only: bool,
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
/// How long a bridge that fell behind the best chain is not tried again: the others first.
const AFTER_BEHIND: u64 = 1800;

/// A full node serving witnesses: what an ordinary peer must serve.
const FULL: u64 = services::NETWORK | services::WITNESS;

/// The service bits defined so far: NETWORK, GETUTXO, BLOOM, WITNESS, XTHIN, COMPACT_FILTERS,
/// NETWORK_LIMITED, P2P_V2, and BIP 183's two.
const KNOWN_SERVICES: u64 = 0b11_1100_0101_1111;

/// What an advertisement of services is taken to mean before the handshake: one that claims bits
/// no node defines (all 64, on mainnet, for addresses of ordinary nodes) is not believed for the
/// rare bits that make a bridge.
fn advertised(services: u64) -> u64 {
    if services & !KNOWN_SERVICES != 0 {
        services & !(services::UTREEXO | services::UTREEXO_ARCHIVE)
    } else {
        services
    }
}

pub struct AddrBook {
    entries: Vec<Entry>,
    capacity: usize,
    /// Whether IPv6 addresses are kept (the node can reach them).
    ipv6: bool,
    rng: u64,
    /// Changes to what a saved book holds, counted.
    changes: u32,
}

/// The first byte of a saved book: its format.
const SAVED_BOOK: u8 = 1;

impl AddrBook {
    /// An empty book. `seed` drives the random picks.
    pub fn new(ipv6: bool, seed: u64) -> Self {
        Self {
            entries: Vec::new(),
            capacity: CAPACITY,
            ipv6,
            rng: seed,
            changes: 0,
        }
    }

    /// Changes to what [`AddrBook::encode`] gives, counted since the book was made: to save it
    /// again only when it changed.
    pub fn changes(&self) -> u32 {
        self.changes
    }

    /// The most useful entries in at most `max` bytes, for [`AddrBook::load`] after a restart:
    /// the configured peer, then those reached (bridges first, the most handshakes first), then
    /// the others (the fewest failures first).
    pub fn encode(&self, max: usize) -> Vec<u8> {
        let mut order: Vec<&Entry> = self.entries.iter().collect();
        let rank = |e: &Entry| {
            (
                e.source != Source::Configured,
                e.handshakes == 0,
                e.services & services::UTREEXO_ARCHIVE == 0,
                u32::MAX - e.handshakes,
                e.failures,
            )
        };
        order.sort_by_key(|e| rank(e));
        let mut out = alloc::vec![SAVED_BOOK];
        for e in order {
            let (kind, ip): (u8, Vec<u8>) = match e.addr.ip() {
                IpAddr::V4(ip) => (4, ip.octets().to_vec()),
                IpAddr::V6(ip) => (6, ip.octets().to_vec()),
            };
            let size = 1 + ip.len() + 2 + 8 + 1 + 2 + 1 + 1;
            if out.len() + size > max {
                break;
            }
            out.push(kind);
            out.extend_from_slice(&ip);
            out.extend_from_slice(&e.addr.port().to_le_bytes());
            out.extend_from_slice(&e.services.to_le_bytes());
            out.push(match e.source {
                Source::Configured => 0,
                Source::Seed => 1,
                Source::Peer => 2,
            });
            out.extend_from_slice(&(e.handshakes.min(u16::MAX as u32) as u16).to_le_bytes());
            out.push(e.failures.min(u8::MAX as u32) as u8);
            out.push(e.v1_only as u8);
        }
        out
    }

    /// Adds the entries of a book [`AddrBook::encode`] gave, those not known already; a saved
    /// configured peer comes back as one a peer gave (the configured peer is the caller's to
    /// add). Returns how many it added; none from bytes it cannot read.
    pub fn load(&mut self, bytes: &[u8]) -> usize {
        if bytes.first() != Some(&SAVED_BOOK) {
            return 0;
        }
        let mut r = &bytes[1..];
        let mut take = |n: usize| -> Option<&[u8]> {
            let (head, tail) = (r.get(..n)?, r.get(n..)?);
            r = tail;
            Some(head)
        };
        let mut added = 0;
        while let Some(kind) = take(1) {
            let ip = match kind[0] {
                4 => match take(4) {
                    Some(b) => IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3])),
                    None => break,
                },
                6 => match take(16).and_then(|b| <[u8; 16]>::try_from(b).ok()) {
                    Some(b) => IpAddr::V6(Ipv6Addr::from(b)),
                    None => break,
                },
                _ => break,
            };
            let Some(rest) = take(15) else {
                break;
            };
            let port = u16::from_le_bytes([rest[0], rest[1]]);
            let mut services = [0; 8];
            services.copy_from_slice(&rest[2..10]);
            let source = match rest[10] {
                1 => Source::Seed,
                _ => Source::Peer,
            };
            let handshakes = u16::from_le_bytes([rest[11], rest[12]]) as u32;
            let failures = rest[13] as u32;
            let v1_only = rest[14] != 0;
            let addr = SocketAddr::new(ip, port);
            if self.add(addr, u64::from_le_bytes(services), source) {
                if let Some(e) = self.entries.iter_mut().find(|e| e.addr == addr) {
                    e.handshakes = handshakes;
                    e.failures = failures;
                    e.v1_only = v1_only;
                }
                added += 1;
            }
        }
        added
    }

    /// Adds an address, or merges the services of one already known that has not done its
    /// handshake yet. Addresses that are not publicly routable are refused unless configured.
    /// Returns whether it was new.
    pub fn add(&mut self, addr: SocketAddr, services: u64, source: Source) -> bool {
        if addr.port() == 0 || (addr.is_ipv6() && !self.ipv6) {
            return false;
        }
        let services = advertised(services);
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
            v1_only: false,
            retry_at: 0,
            in_use: false,
        });
        self.changes = self.changes.wrapping_add(1);
        true
    }

    /// Whether to try the v2 transport with `addr`: it says it speaks it, or it is the configured
    /// peer (its services unknown before its handshake), and it has not failed at it.
    pub fn wants_v2(&self, addr: SocketAddr) -> bool {
        self.entries.iter().any(|e| {
            e.addr == addr
                && (e.services & services::P2P_V2 != 0 || e.source == Source::Configured)
                && !e.v1_only
        })
    }

    /// The v2 handshake with `addr` failed: v1 from now on.
    pub fn v1_only(&mut self, addr: SocketAddr) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.addr == addr) {
            e.v1_only = true;
            self.changes = self.changes.wrapping_add(1);
        }
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
            self.changes = self.changes.wrapping_add(1);
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
                self.changes = self.changes.wrapping_add(1);
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
                self.changes = self.changes.wrapping_add(1);
            }
            Outcome::Behind => e.retry_at = now + AFTER_BEHIND,
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
    fn an_advertisement_of_every_service_makes_no_bridge() {
        let mut book = AddrBook::new(false, 1);
        assert!(book.add(addr("35.228.230.233:8333"), u64::MAX, Source::Peer));
        assert_eq!(book.count(Role::Bridge), 0);
        // still an ordinary peer to try
        assert_eq!(book.count(Role::Peer), 1);
        // a bridge's own advertisement is believed
        let bridge = FULL | services::P2P_V2 | services::UTREEXO | services::UTREEXO_ARCHIVE;
        assert!(book.add(addr("1.231.31.3:8333"), bridge | 0x444, Source::Peer));
        assert_eq!(book.count(Role::Bridge), 1);
    }

    #[test]
    fn a_saved_book_comes_back_its_best_entries_first() {
        let mut book = AddrBook::new(true, 1);
        let configured = addr("5.6.7.8:8333");
        let bridge = addr("1.231.31.3:8333");
        let reached = addr("[2a01:4f8::1]:8333");
        let failing = addr("8.8.4.4:8333");
        assert!(book.add(configured, 0, Source::Configured));
        assert!(book.add(bridge, 0, Source::Seed));
        assert!(book.add(reached, FULL, Source::Peer));
        assert!(book.add(failing, FULL, Source::Peer));
        book.handshake(bridge, FULL | services::UTREEXO_ARCHIVE);
        book.handshake(reached, FULL);
        book.v1_only(reached);
        book.release(failing, Outcome::Unreached, 0);
        let changes = book.changes();
        assert!(changes > 0);

        let saved = book.encode(1000);
        let mut again = AddrBook::new(true, 2);
        assert_eq!(again.load(&saved), 4);
        let entry = |a| {
            again
                .entries()
                .iter()
                .find(|e| e.addr == a)
                .unwrap()
                .clone()
        };
        // the configured peer comes back as an ordinary one: the caller adds it
        assert_eq!(entry(configured).source, Source::Peer);
        assert_eq!(entry(bridge).source, Source::Seed);
        assert_eq!(entry(bridge).handshakes, 1);
        assert!(entry(bridge).services & services::UTREEXO_ARCHIVE != 0);
        assert!(entry(reached).v1_only);
        assert_eq!(entry(failing).failures, 1);
        // again: nothing new
        assert_eq!(again.load(&saved), 0);

        // within a size: the configured peer and the bridge first
        let small = book.encode(1 + 2 * 20);
        let mut few = AddrBook::new(true, 3);
        assert_eq!(few.load(&small), 2);
        assert!(few.entries().iter().any(|e| e.addr == configured));
        assert!(few.entries().iter().any(|e| e.addr == bridge));
        // not a book
        assert_eq!(few.load(&[0xff, 1, 2]), 0);
        assert_eq!(few.load(&small[..small.len() - 3]), 0);
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
    fn v2_is_tried_with_those_that_speak_it_until_it_fails() {
        let mut book = AddrBook::new(false, 1);
        let a = addr("1.2.3.4:8333");
        let b = addr("5.6.7.8:8333");
        book.add(a, FULL | services::P2P_V2, Source::Peer);
        book.add(b, FULL, Source::Peer);
        assert!(book.wants_v2(a));
        assert!(!book.wants_v2(b));
        book.v1_only(a);
        assert!(!book.wants_v2(a));
        // its handshake says v2: v1 still, it failed
        book.handshake(a, FULL | services::P2P_V2);
        assert!(!book.wants_v2(a));
        // the configured peer: v2 first
        let c = addr("1.231.31.3:8333");
        book.add(c, 0, Source::Configured);
        assert!(book.wants_v2(c));
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
