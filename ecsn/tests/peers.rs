//! The bridge's connection beside ordinary peers' (in the same process, answering from the
//! regtest fixture): announcements from the peers, a block the bridge does not have yet, invalid
//! headers, addresses, and a slot that connects by itself.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bitcoin::block::Header;
use bitcoin::{CompactTarget, Network};
use common::{fixture, Fixture};
use ecsn::node::{Clock, Config, Dialer, Error, Node, Outcome, Role, Source};
use ecsn::p2p::{
    services, Address, Connection, Inventory, Message, NetAddress, UtreexoProof, Version,
    PROTOCOL_VERSION,
};
use ecsn::store::MemChainStore;
use embedded_io_adapters::tokio_1::FromTokio;
use floresta_chain::BlockchainInterface;
use tokio::io::DuplexStream;

fn node() -> Node<MemChainStore> {
    let config = Config {
        network: Network::Regtest,
        user_agent: String::new(),
        assume_utreexo: None,
        blocks_in_flight: 8,
        ipv6: false,
        v2: true,
        v2_bridge: true,
    };
    Node::new(MemChainStore::new(), config, 0).unwrap()
}

fn version(user_agent: &str, services: u64, start_height: i32) -> Message {
    Message::Version(Version {
        version: PROTOCOL_VERSION,
        services,
        timestamp: 0,
        receiver: NetAddress::default(),
        sender: NetAddress::default(),
        nonce: 1,
        user_agent: user_agent.into(),
        start_height,
        relay: false,
    })
}

const BRIDGE: u64 =
    services::NETWORK | services::WITNESS | services::UTREEXO | services::UTREEXO_ARCHIVE;
const FULL: u64 = services::NETWORK | services::WITNESS;

fn headers(fixture: &Fixture, count: usize) -> Message {
    Message::Headers(fixture.blocks[..count].iter().map(|b| b.header).collect())
}

fn blocks(fixture: &Fixture, list: &[Inventory]) -> Vec<Message> {
    list.iter()
        .filter_map(|inv| inv.block())
        .map(|hash| {
            let block = fixture.blocks.iter().find(|b| b.block_hash() == hash);
            Message::Block(block.unwrap().clone())
        })
        .collect()
}

fn proof(fixture: &Fixture, get: &ecsn::p2p::GetUtreexoProof) -> Message {
    Message::UtreexoProof(UtreexoProof {
        block_hash: get.block_hash,
        proof_hashes: fixture.proof.hashes.clone(),
        targets: fixture.proof.targets.clone(),
        leaf_data: fixture.leaf_data.clone(),
    })
}

/// A bridge that has blocks up to 110 until it is asked for headers a second time, then up to
/// 111, which it never announces; it hangs up once it sent block 111's proof.
async fn bridge_of_110(io: DuplexStream, fixture: &Fixture) {
    serve_110(
        Connection::new(FromTokio::new(io), Network::Regtest),
        fixture,
    )
    .await
}

/// [`bridge_of_110`] in the v2 transport.
async fn bridge_of_110_v2(io: DuplexStream, fixture: &Fixture) {
    let mut conn = Connection::new(FromTokio::new(io), Network::Regtest);
    if conn
        .start_v2(bip324::Role::Responder, [7; 64])
        .await
        .is_ok()
    {
        serve_110(conn, fixture).await
    }
}

async fn serve_110(mut conn: Connection<FromTokio<DuplexStream>>, fixture: &Fixture) {
    let mut asked_headers = 0;
    while let Ok(message) = conn.recv().await {
        let answers = match message {
            Message::Version(_) => vec![version("/bridge/", BRIDGE, 110), Message::Verack],
            Message::GetHeaders { .. } => {
                asked_headers += 1;
                vec![headers(fixture, if asked_headers == 1 { 110 } else { 111 })]
            }
            Message::GetData(list) => blocks(fixture, &list),
            Message::GetUtreexoProof(get) => {
                let _ = conn.send(&proof(fixture, &get)).await;
                return;
            }
            _ => Vec::new(),
        };
        for answer in &answers {
            if conn.send(answer).await.is_err() {
                return;
            }
        }
    }
}

/// An ordinary peer at 111: announces block 111's header once the node asked it for addresses,
/// answers `getheaders` with all of them.
async fn peer_at_111(io: DuplexStream, fixture: &Fixture) {
    let mut conn = Connection::new(FromTokio::new(io), Network::Regtest);
    while let Ok(message) = conn.recv().await {
        let answers = match message {
            Message::Version(_) => vec![version("/ordinary/", FULL, 111), Message::Verack],
            Message::GetAddr => vec![Message::Headers(vec![fixture.blocks[110].header])],
            Message::GetHeaders { .. } => vec![headers(fixture, 111)],
            _ => Vec::new(),
        };
        for answer in &answers {
            if conn.send(answer).await.is_err() {
                return;
            }
        }
    }
}

/// Runs `work` until `done` holds, within 10 s.
async fn until(work: impl std::future::Future, done: impl Fn() -> bool) {
    let wait = async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let both = async {
        tokio::select! {
            _ = work => {}
            _ = wait => {}
        }
    };
    tokio::time::timeout(Duration::from_secs(10), both)
        .await
        .expect("not done in 10 s");
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

#[test]
fn a_block_an_ordinary_peer_announces_is_fetched_from_the_bridge() {
    let fixture = fixture();
    let node = node();
    runtime().block_on(async {
        let (bridge_ours, bridge_theirs) = tokio::io::duplex(1 << 16);
        let (peer_ours, peer_theirs) = tokio::io::duplex(1 << 16);
        let bridge = async {
            let mut conn = Connection::new(FromTokio::new(bridge_ours), Network::Regtest);
            let _ = node.run_bridge(&mut conn, 0, None, 0).await;
        };
        let peer = async {
            let mut conn = Connection::new(FromTokio::new(peer_ours), Network::Regtest);
            let _ = node.run_peer(&mut conn, 1, None, 0).await;
        };
        let all = async {
            tokio::join!(
                bridge,
                bridge_of_110(bridge_theirs, &fixture),
                peer,
                peer_at_111(peer_theirs, &fixture)
            )
        };
        until(all, || node.chain().get_validation_index().unwrap() == 111).await;
    });
    let acc = node.chain().acc();
    assert_eq!(acc.leaves, fixture.leaves);
    let status = node.status();
    let ordinary = status.peers.iter().find(|p| p.role == Role::Peer).unwrap();
    assert_eq!(ordinary.slot, 1);
    assert_eq!(ordinary.info.as_ref().unwrap().user_agent, "/ordinary/");
    assert_eq!(status.connections, 2);
}

/// A bridge that says it is at 111 but does not have block 111 when first asked for it: it
/// answers `notfound`, then announces it, then serves it, and hangs up after its proof.
async fn bridge_late_with_111(io: DuplexStream, fixture: &Fixture) {
    let mut conn = Connection::new(FromTokio::new(io), Network::Regtest);
    let last = fixture.blocks[110].block_hash();
    let mut missing = true;
    while let Ok(message) = conn.recv().await {
        let answers = match message {
            Message::Version(_) => vec![version("/bridge/", BRIDGE, 111), Message::Verack],
            Message::GetHeaders { .. } => vec![headers(fixture, 111)],
            Message::GetData(list) if missing && list.iter().any(|i| i.block() == Some(last)) => {
                missing = false;
                let (late, rest): (Vec<_>, Vec<_>) =
                    list.into_iter().partition(|i| i.block() == Some(last));
                let mut answers = blocks(fixture, &rest);
                answers.push(Message::NotFound(late));
                answers.push(Message::Headers(vec![fixture.blocks[110].header]));
                answers
            }
            Message::GetData(list) => blocks(fixture, &list),
            Message::GetUtreexoProof(get) => {
                let _ = conn.send(&proof(fixture, &get)).await;
                return;
            }
            _ => Vec::new(),
        };
        for answer in &answers {
            if conn.send(answer).await.is_err() {
                return;
            }
        }
    }
}

#[test]
fn a_block_the_bridge_does_not_have_yet_is_waited_for() {
    let fixture = fixture();
    let node = node();
    let result = runtime().block_on(async {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let bridge = async {
            let mut conn = Connection::new(FromTokio::new(ours), Network::Regtest);
            node.run_bridge(&mut conn, 0, None, 0).await
        };
        let both = async { tokio::join!(bridge, bridge_late_with_111(theirs, &fixture)).0 };
        tokio::time::timeout(Duration::from_secs(10), both)
            .await
            .expect("not done in 10 s")
    });
    assert_eq!(node.chain().get_validation_index().unwrap(), 111);
    // the connection ends when the bridge hangs up, not at its `notfound`
    assert!(matches!(
        result,
        Err(Error::P2p(
            ecsn::p2p::Error::Closed | ecsn::p2p::Error::Io(_)
        ))
    ));
}

/// An ordinary peer that gives three addresses (one private), then a header of the wrong
/// difficulty.
async fn peer_with_a_bad_header(io: DuplexStream, fixture: &Fixture) {
    let mut conn = Connection::new(FromTokio::new(io), Network::Regtest);
    let mut bad: Header = fixture.blocks[0].header;
    bad.bits = CompactTarget::from_consensus(0x2000_ffff);
    let address = |s: &str| Address {
        time: 0,
        services: FULL,
        socket: s.parse().unwrap(),
    };
    while let Ok(message) = conn.recv().await {
        let answers = match message {
            Message::Version(_) => vec![version("/bad/", FULL, 1), Message::Verack],
            Message::GetAddr => vec![
                Message::AddrV2(vec![
                    address("1.2.3.4:18444"),
                    address("5.6.7.8:18444"),
                    address("192.168.1.2:18444"),
                ]),
                Message::Headers(vec![bad]),
            ],
            _ => Vec::new(),
        };
        for answer in &answers {
            if conn.send(answer).await.is_err() {
                return;
            }
        }
    }
}

#[test]
fn an_invalid_header_ends_the_connection_and_its_addresses_are_kept() {
    let fixture = fixture();
    let node = node();
    let result = runtime().block_on(async {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let peer = async {
            let mut conn = Connection::new(FromTokio::new(ours), Network::Regtest);
            node.run_peer(&mut conn, 1, None, 0).await
        };
        let both = async { tokio::join!(peer, peer_with_a_bad_header(theirs, &fixture)).0 };
        tokio::time::timeout(Duration::from_secs(10), both)
            .await
            .expect("not done in 10 s")
    });
    let e = result.unwrap_err();
    assert!(matches!(e, Error::InvalidHeader), "{e:?}");
    assert_eq!(e.outcome(), Outcome::Misbehaved);
    assert_eq!(node.chain().get_best_block().unwrap().0, 0);
    // the private address is not kept
    assert_eq!(node.status().known, 2);
}

/// Hands out one stream, once.
struct OneStream(Option<DuplexStream>);

impl Dialer for OneStream {
    type Io<'a> = FromTokio<DuplexStream>;

    async fn connect(&mut self, _: SocketAddr) -> Option<Self::Io<'_>> {
        self.0.take().map(FromTokio::new)
    }

    fn random(&mut self) -> [u8; 64] {
        [3; 64]
    }
}

/// Hands out its streams in turn, then none.
struct Streams(std::collections::VecDeque<DuplexStream>);

impl Dialer for Streams {
    type Io<'a> = FromTokio<DuplexStream>;

    async fn connect(&mut self, _: SocketAddr) -> Option<Self::Io<'_>> {
        self.0.pop_front().map(FromTokio::new)
    }

    fn random(&mut self) -> [u8; 64] {
        [5; 64]
    }
}

struct TokioClock(Instant);

impl Clock for TokioClock {
    fn now(&self) -> u64 {
        self.0.elapsed().as_secs()
    }

    fn unix(&self) -> i64 {
        0
    }

    async fn sleep(&self, seconds: u64) {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }
}

#[test]
fn the_bridge_slot_connects_to_the_configured_peer() {
    let fixture = fixture();
    let node = node();
    let configured: SocketAddr = "10.0.2.2:18444".parse().unwrap();
    assert!(node.add_address(configured, 0, Source::Configured));
    let seen = std::cell::Cell::new(None);
    runtime().block_on(async {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let mut dialer = OneStream(Some(ours));
        let clock = TokioClock(Instant::now());
        let slot = node.run_slot(0, Role::Bridge, &mut dialer, &clock);
        let watch = async {
            loop {
                if let Some(bridge) = node.status().bridge() {
                    if bridge.info.is_some() {
                        seen.set(bridge.addr);
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        // the configured peer is tried in v2 first
        let all = async { tokio::join!(slot, bridge_of_110_v2(theirs, &fixture), watch) };
        until(all, || node.chain().get_validation_index().unwrap() == 110).await;
    });
    assert_eq!(seen.get(), Some(configured));
    let status = node.status();
    assert_eq!((status.known, status.known_bridges), (1, 1));
}

/// The bridge's slot to a bridge that says it speaks v2 (`addr`), served by `first` then
/// `second` on the streams it opens: the slot's status once it validated up to 110, and whether
/// the book still wants v2 with it.
fn slot_to_a_v2_bridge<F1, F2>(
    first: impl FnOnce(DuplexStream) -> F1,
    second: impl FnOnce(DuplexStream) -> F2,
) -> (ecsn::node::PeerStatus, usize)
where
    F1: std::future::Future<Output = ()>,
    F2: std::future::Future<Output = ()>,
{
    let node = node();
    let addr: SocketAddr = "5.6.7.8:18444".parse().unwrap();
    assert!(node.add_address(addr, BRIDGE | services::P2P_V2, Source::Peer));
    let seen = std::cell::RefCell::new(None);
    let streams = std::cell::Cell::new(0);
    runtime().block_on(async {
        let (a, a_far) = tokio::io::duplex(1 << 16);
        let (b, b_far) = tokio::io::duplex(1 << 16);
        let mut dialer = Streams([a, b].into());
        let clock = TokioClock(Instant::now());
        let slot = node.run_slot(0, Role::Bridge, &mut dialer, &clock);
        let watch = async {
            loop {
                if let Some(bridge) = node.status().bridge() {
                    if bridge.info.is_some() {
                        *seen.borrow_mut() = Some(bridge.clone());
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let all = async { tokio::join!(slot, first(a_far), second(b_far), watch) };
        until(all, || node.chain().get_validation_index().unwrap() == 110).await;
        streams.set(2 - dialer.0.len());
    });
    (
        seen.into_inner().expect("the bridge's handshake"),
        streams.get(),
    )
}

#[test]
fn the_bridge_slot_speaks_v2_with_a_bridge_that_does() {
    let fixture = fixture();
    let (bridge, streams) = slot_to_a_v2_bridge(|io| bridge_of_110_v2(io, &fixture), |_| async {});
    assert!(bridge.v2);
    assert_eq!(bridge.info.unwrap().user_agent, "/bridge/");
    assert_eq!(streams, 1);
}

#[test]
fn the_bridge_slot_falls_back_to_v1_when_v2_fails() {
    let fixture = fixture();
    // a v1 bridge that says it speaks v2: it hangs up at our key, then serves us in v1
    let (bridge, streams) = slot_to_a_v2_bridge(
        |io| bridge_of_110(io, &fixture),
        |io| bridge_of_110(io, &fixture),
    );
    assert!(!bridge.v2);
    assert_eq!(streams, 2);
}
