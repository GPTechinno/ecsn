//! The sync against a peer in the same process that answers from the regtest fixture: several
//! blocks asked for at once, validated in order whatever order they come in.

mod common;

use bitcoin::Network;
use common::{fixture, mine_headers, Fixture, Shared};
use ecsn::node::{Config, Node, Phase};
use ecsn::p2p::{
    services, Connection, Message, NetAddress, UtreexoProof, Version, MAX_HEADERS, PROTOCOL_VERSION,
};
use ecsn::store::{FlashChainStore, MemChainStore};
use embedded_io_adapters::tokio_1::FromTokio;
use floresta_chain::BlockchainInterface;
use tokio::io::DuplexStream;

/// How the peer answers: the order of the blocks of a `getdata`, and
/// whether a header announcement comes before each `headers` answer.
#[derive(Clone, Copy)]
enum Order {
    AsAsked,
    Reversed,
    AnnouncedFirst,
}

/// Answers the node from the fixture until it hangs up. Returns the length of each `getdata`.
async fn peer(io: DuplexStream, fixture: &Fixture, order: Order) -> Vec<usize> {
    let mut conn = Connection::new(FromTokio::new(io), Network::Regtest);
    let mut asked = Vec::new();
    while let Ok(message) = conn.recv().await {
        let answers = match message {
            Message::Version(_) => vec![
                Message::Version(Version {
                    version: PROTOCOL_VERSION,
                    services: services::WITNESS | services::UTREEXO | services::UTREEXO_ARCHIVE,
                    timestamp: 0,
                    receiver: NetAddress::default(),
                    sender: NetAddress::default(),
                    nonce: 1,
                    user_agent: "/fixture/".into(),
                    start_height: 111,
                    relay: false,
                }),
                Message::Verack,
            ],
            Message::GetHeaders { .. } => {
                let all = Message::Headers(fixture.blocks.iter().map(|b| b.header).collect());
                match order {
                    // the newest header, announced (`sendheaders`) before the answer
                    Order::AnnouncedFirst => {
                        vec![Message::Headers(vec![fixture.blocks[110].header]), all]
                    }
                    _ => vec![all],
                }
            }
            Message::GetData(list) => {
                asked.push(list.len());
                let mut blocks: Vec<_> = list
                    .iter()
                    .filter_map(|inv| inv.block())
                    .map(|hash| {
                        let block = fixture.blocks.iter().find(|b| b.block_hash() == hash);
                        Message::Block(block.unwrap().clone())
                    })
                    .collect();
                if let Order::Reversed = order {
                    blocks.reverse();
                }
                blocks
            }
            Message::GetUtreexoProof(get) => {
                // block 111 is the only one with more than its coinbase
                assert_eq!(get.block_hash, fixture.blocks[110].block_hash());
                vec![Message::UtreexoProof(UtreexoProof {
                    block_hash: get.block_hash,
                    proof_hashes: fixture.proof.hashes.clone(),
                    targets: fixture.proof.targets.clone(),
                    leaf_data: fixture.leaf_data.clone(),
                })]
            }
            _ => Vec::new(),
        };
        for answer in &answers {
            if conn.send(answer).await.is_err() {
                return asked;
            }
        }
    }
    asked
}

/// Syncs a node with `blocks_in_flight` from the fixture's peer, checks the accumulator it
/// ends with, and returns the length of each `getdata` it sent.
fn sync(blocks_in_flight: usize, order: Order) -> Vec<usize> {
    let fixture = fixture();
    let config = Config {
        network: Network::Regtest,
        user_agent: String::new(),
        assume_utreexo: None,
        blocks_in_flight,
        ipv6: false,
        v2: true,
        v2_bridge: true,
    };
    let node = Node::new(MemChainStore::new(), config, 0).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let asked = runtime.block_on(async {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let ours = async {
            let mut conn = Connection::new(FromTokio::new(ours), Network::Regtest);
            let version = node.handshake(&mut conn, 0).await.unwrap();
            assert_eq!(
                (version.user_agent.as_str(), version.start_height),
                ("/fixture/", 111)
            );
            assert_eq!(node.sync(&mut conn).await.unwrap(), 111);
        };
        tokio::join!(ours, peer(theirs, &fixture, order)).1
    });
    let acc = node.chain().acc();
    assert_eq!(acc.leaves, fixture.leaves);
    let roots: Vec<String> = acc.roots.iter().map(ToString::to_string).collect();
    assert_eq!(roots, fixture.roots);
    assert_eq!(asked.iter().sum::<usize>(), 111);

    let status = node.status();
    assert_eq!(status.phase, Phase::Following);
    // a connection handed to the node, its handshake done by hand: no slot shows it
    assert!(status.peers.is_empty());
    assert_eq!(status.connections, 1);
    assert_eq!(status.validated, 111);
    let last = status.last_block.unwrap();
    assert_eq!(last.height, 111);
    assert_eq!(last.hash, fixture.blocks[110].block_hash());
    assert_eq!(last.transactions, fixture.blocks[110].txdata.len());
    assert_eq!(last.inputs, fixture.leaf_data.len());
    asked
}

#[test]
fn eight_blocks_are_asked_for_at_once() {
    let asked = sync(8, Order::AsAsked);
    assert_eq!(asked[0], 8);
}

#[test]
fn blocks_sent_in_reverse_are_validated_in_order() {
    let asked = sync(8, Order::Reversed);
    assert_eq!(asked[0], 8);
}

#[test]
fn one_block_at_a_time() {
    assert_eq!(sync(1, Order::AsAsked), vec![1; 111]);
}

#[test]
fn a_header_announcement_before_the_answer_is_skipped() {
    let asked = sync(8, Order::AnnouncedFirst);
    assert_eq!(asked[0], 8);
}

/// A peer whose first `headers` answer stops at block 110, which announces block 111 while
/// serving the first blocks, serves it, and hangs up once it sent its proof.
async fn peer_with_a_late_block(io: DuplexStream, fixture: &Fixture) {
    let mut conn = Connection::new(FromTokio::new(io), Network::Regtest);
    let mut headers_answered = false;
    let mut announced = false;
    while let Ok(message) = conn.recv().await {
        let answers = match message {
            Message::Version(_) => vec![
                Message::Version(Version {
                    version: PROTOCOL_VERSION,
                    services: services::WITNESS | services::UTREEXO | services::UTREEXO_ARCHIVE,
                    timestamp: 0,
                    receiver: NetAddress::default(),
                    sender: NetAddress::default(),
                    nonce: 1,
                    user_agent: "/fixture/".into(),
                    start_height: 110,
                    relay: false,
                }),
                Message::Verack,
            ],
            Message::GetHeaders { .. } => {
                let count = if headers_answered { 111 } else { 110 };
                headers_answered = true;
                vec![Message::Headers(
                    fixture.blocks[..count].iter().map(|b| b.header).collect(),
                )]
            }
            Message::GetData(list) => {
                let mut answers: Vec<_> = list
                    .iter()
                    .filter_map(|inv| inv.block())
                    .map(|hash| {
                        let block = fixture.blocks.iter().find(|b| b.block_hash() == hash);
                        Message::Block(block.unwrap().clone())
                    })
                    .collect();
                if !announced {
                    announced = true;
                    answers.push(Message::Headers(vec![fixture.blocks[110].header]));
                }
                answers
            }
            Message::GetUtreexoProof(get) => {
                let proof = Message::UtreexoProof(UtreexoProof {
                    block_hash: get.block_hash,
                    proof_hashes: fixture.proof.hashes.clone(),
                    targets: fixture.proof.targets.clone(),
                    leaf_data: fixture.leaf_data.clone(),
                });
                let _ = conn.send(&proof).await;
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
fn a_block_announced_during_a_sync_is_followed() {
    let fixture = fixture();
    let config = Config {
        network: Network::Regtest,
        user_agent: String::new(),
        assume_utreexo: None,
        blocks_in_flight: 8,
        ipv6: false,
        v2: true,
        v2_bridge: true,
    };
    let node = Node::new(MemChainStore::new(), config, 0).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let ours = async {
            let mut conn = Connection::new(FromTokio::new(ours), Network::Regtest);
            // the peer hangs up at the end
            assert!(node.run_bridge(&mut conn, 0, None, 0).await.is_err());
        };
        let both = async { tokio::join!(ours, peer_with_a_late_block(theirs, &fixture)) };
        tokio::time::timeout(std::time::Duration::from_secs(10), both)
            .await
            .expect("the node waited for an announcement it already had");
    });
    assert_eq!(node.chain().get_validation_index().unwrap(), 111);
    // the connection ended
    let status = node.status();
    assert_eq!(status.phase, Phase::Idle);
    // the slot shows the bridge until whoever runs the slots closes it
    let bridge = status.bridge().unwrap().info.as_ref().unwrap();
    assert_eq!(
        (bridge.user_agent.as_str(), bridge.height),
        ("/fixture/", 110)
    );
    assert_eq!(status.connections, 1);
    assert_eq!(status.last_block.map(|b| b.height), Some(111));
}

/// Sends a full batch of headers, then hangs up at the next `getheaders`.
async fn peer_of_one_batch(io: DuplexStream, headers: Vec<bitcoin::block::Header>) {
    let mut conn = Connection::new(FromTokio::new(io), Network::Regtest);
    let mut batches = 0;
    while let Ok(message) = conn.recv().await {
        let answers = match message {
            Message::Version(_) => vec![
                Message::Version(Version {
                    version: PROTOCOL_VERSION,
                    services: services::WITNESS | services::UTREEXO | services::UTREEXO_ARCHIVE,
                    timestamp: 0,
                    receiver: NetAddress::default(),
                    sender: NetAddress::default(),
                    nonce: 1,
                    user_agent: "/fixture/".into(),
                    start_height: 4000,
                    relay: false,
                }),
                Message::Verack,
            ],
            Message::GetHeaders { .. } if batches == 0 => {
                batches += 1;
                vec![Message::Headers(headers.clone())]
            }
            Message::GetHeaders { .. } => return,
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
fn a_reset_during_the_header_sync_keeps_the_headers_synced() {
    let fixture = fixture();
    let mut headers: Vec<_> = fixture.blocks.iter().map(|b| b.header).collect();
    headers.extend(mine_headers(&headers[110], MAX_HEADERS - headers.len()));
    let flash = Shared::new(4 << 20);
    let store = FlashChainStore::format(flash.clone(), Network::Regtest).unwrap();
    let config = || Config {
        network: Network::Regtest,
        user_agent: String::new(),
        assume_utreexo: None,
        blocks_in_flight: 1,
        ipv6: false,
        v2: true,
        v2_bridge: true,
    };
    let node = Node::new(store, config(), 0).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let ours = async {
            let mut conn = Connection::new(FromTokio::new(ours), Network::Regtest);
            node.handshake(&mut conn, 0).await.unwrap();
            assert!(node.sync(&mut conn).await.is_err(), "the peer hung up");
        };
        tokio::join!(ours, peer_of_one_batch(theirs, headers.clone()));
    });

    // a reset now: the batch is the best chain, nothing to reindex
    let (store, formatted) =
        FlashChainStore::mount(flash.reset(), Network::Regtest, false).unwrap();
    assert!(!formatted);
    let node = Node::new(store, config(), 0).unwrap();
    let (height, hash) = node.chain().get_best_block().unwrap();
    assert_eq!(height, MAX_HEADERS as u32);
    assert_eq!(hash, headers[MAX_HEADERS - 1].block_hash());
}

/// Answers `getheaders` from `headers` (the chain after genesis), from the first locator hash
/// it has, and records each locator.
async fn peer_of_headers(
    io: DuplexStream,
    headers: &[bitcoin::block::Header],
    locators: &mut Vec<Vec<bitcoin::BlockHash>>,
) {
    let mut conn = Connection::new(FromTokio::new(io), Network::Regtest);
    while let Ok(message) = conn.recv().await {
        let answers = match message {
            Message::Version(_) => vec![
                Message::Version(Version {
                    version: PROTOCOL_VERSION,
                    services: services::WITNESS | services::UTREEXO | services::UTREEXO_ARCHIVE,
                    timestamp: 0,
                    receiver: NetAddress::default(),
                    sender: NetAddress::default(),
                    nonce: 1,
                    user_agent: "/fixture/".into(),
                    start_height: headers.len() as i32,
                    relay: false,
                }),
                Message::Verack,
            ],
            Message::GetHeaders { locator, .. } => {
                let start = locator
                    .iter()
                    .find_map(|hash| headers.iter().position(|h| h.block_hash() == *hash))
                    .map_or(0, |i| i + 1);
                locators.push(locator);
                let end = (start + MAX_HEADERS).min(headers.len());
                vec![Message::Headers(headers[start..end].to_vec())]
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
fn the_next_batch_of_headers_is_asked_for_before_checking_one() {
    let fixture = fixture();
    let mut headers: Vec<_> = fixture.blocks.iter().map(|b| b.header).collect();
    headers.extend(mine_headers(
        &headers[110],
        MAX_HEADERS + 100 - headers.len(),
    ));
    let config = Config {
        network: Network::Regtest,
        user_agent: String::new(),
        assume_utreexo: None,
        blocks_in_flight: 1,
        ipv6: false,
        v2: true,
        v2_bridge: true,
    };
    let node = Node::new(MemChainStore::new(), config, 0).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut locators = Vec::new();
    runtime.block_on(async {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let ours = async {
            let mut conn = Connection::new(FromTokio::new(ours), Network::Regtest);
            node.handshake(&mut conn, 0).await.unwrap();
            let height = node.sync_headers(&mut conn).await.unwrap();
            assert_eq!(height, MAX_HEADERS as u32 + 100);
        };
        tokio::join!(ours, peer_of_headers(theirs, &headers, &mut locators));
    });
    // the first from the chain's locator, the second from the last header of the first batch
    assert_eq!(locators.len(), 2);
    assert_eq!(locators[1], vec![headers[MAX_HEADERS - 1].block_hash()]);
}
