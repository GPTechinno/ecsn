//! The sync against a peer in the same process that answers from the regtest fixture: several
//! blocks asked for at once, validated in order whatever order they come in.

mod common;

use bitcoin::Network;
use common::{fixture, Fixture};
use ecsn::node::{Config, Node};
use ecsn::p2p::{
    services, Connection, Message, NetAddress, UtreexoProof, Version, PROTOCOL_VERSION,
};
use ecsn::store::MemChainStore;
use embedded_io_adapters::tokio_1::FromTokio;
use tokio::io::DuplexStream;

/// The order in which the peer sends the blocks of a `getdata`.
#[derive(Clone, Copy)]
enum Order {
    AsAsked,
    Reversed,
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
            Message::GetHeaders { .. } => vec![Message::Headers(
                fixture.blocks.iter().map(|b| b.header).collect(),
            )],
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
    };
    let node = Node::new(MemChainStore::new(), config, 0).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let asked = runtime.block_on(async {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let ours = async {
            let mut conn = Connection::new(FromTokio::new(ours), Network::Regtest);
            node.handshake(&mut conn, 0).await.unwrap();
            assert_eq!(node.sync(&mut conn).await.unwrap(), 111);
        };
        tokio::join!(ours, peer(theirs, &fixture, order)).1
    });
    let acc = node.chain().acc();
    assert_eq!(acc.leaves, fixture.leaves);
    let roots: Vec<String> = acc.roots.iter().map(ToString::to_string).collect();
    assert_eq!(roots, fixture.roots);
    assert_eq!(asked.iter().sum::<usize>(), 111);
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
