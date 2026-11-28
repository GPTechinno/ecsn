//! The mempool against the regtest chain recorded from utreexod: block 111's transactions, each
//! with the part of the block's proof that proves the outputs it spends, checked against the
//! accumulator after block 110.

mod common;

use bitcoin::hashes::Hash;
use bitcoin::{absolute, Amount, Network, Sequence, Txid};
use common::{fixture, Fixture};
use ecsn::mempool::{proof_positions, Mempool, Reject};
use ecsn::node::{Config, Node};
use ecsn::p2p::{
    inv, pack_positions, services, unpack_positions, Connection, Inventory, Message, NetAddress,
    UtreexoProof, UtreexoTx, Version, PROTOCOL_VERSION,
};
use ecsn::store::MemChainStore;
use embedded_io_adapters::tokio_1::FromTokio;
use floresta_chain::proof_util::process_proof;
use floresta_chain::pruned_utreexo::UpdatableChainstate;
use floresta_chain::{BlockchainError, BlockchainInterface};
use rustreexo::node_hash::BitcoinNodeHash;
use rustreexo::proof::Proof;
use tokio::io::DuplexStream;

/// A node with every header, and every block validated up to 110.
fn node_at_110(fixture: &Fixture) -> Node<MemChainStore> {
    let config = Config {
        network: Network::Regtest,
        user_agent: String::new(),
        assume_utreexo: None,
        blocks_in_flight: 1,
        ipv6: false,
        v2: true,
        v2_bridge: true,
        free_memory: None,
        mempool: 0,
        verify_scripts: None,
    };
    let node = Node::new(MemChainStore::new(), config, 0).unwrap();
    for block in &fixture.blocks {
        node.chain().accept_header(block.header).unwrap();
    }
    for (i, block) in fixture.blocks[..110].iter().enumerate() {
        node.connect_block(i as u32 + 1, block, &[], Proof::default())
            .unwrap();
    }
    node
}

/// Block 111's transactions as a bridge relays them before the block: transaction `i` with the
/// proof of the outputs it spends that are confirmed, the others (of the block's earlier
/// transactions) flagged unconfirmed.
struct Relay {
    fixture: Fixture,
    node: Node<MemChainStore>,
    /// The leaf hashes of the block's spent outputs, in the order of its inputs.
    del_hashes: Vec<BitcoinNodeHash>,
}

impl Relay {
    fn new() -> Self {
        let fixture = fixture();
        let node = node_at_110(&fixture);
        let chain = node.chain();
        let (del_hashes, _) = process_proof::<_, _, BlockchainError>(
            &fixture.leaf_data,
            &fixture.blocks[110].txdata,
            111,
            |h| chain.get_block_hash(h),
            |h| chain.get_mtp_by_height(h),
        )
        .unwrap();
        let del_hashes = del_hashes
            .iter()
            .map(|h| BitcoinNodeHash::Some(h.to_byte_array()))
            .collect();
        Self {
            fixture,
            node,
            del_hashes,
        }
    }

    fn block_txids(&self) -> Vec<Txid> {
        self.fixture.blocks[110]
            .txdata
            .iter()
            .map(|tx| tx.compute_txid())
            .collect()
    }

    fn utx(&self, i: usize) -> UtreexoTx {
        let txids = self.block_txids();
        let block = &self.fixture.blocks[110];
        // the index of each confirmed input among the block's
        let mut k = 0;
        let mut mine = Vec::new();
        for (j, tx) in block.txdata.iter().enumerate().skip(1) {
            for input in &tx.input {
                if txids.contains(&input.previous_output.txid) {
                    continue;
                }
                if j == i {
                    mine.push(k);
                }
                k += 1;
            }
        }
        let tx = block.txdata[i].clone();
        let unconfirmed = tx
            .input
            .iter()
            .map(|input| txids.contains(&input.previous_output.txid))
            .collect();
        let targets: Vec<u64> = mine
            .iter()
            .map(|k| self.fixture.proof.targets[*k])
            .collect();
        let proof = self
            .fixture
            .proof
            .get_proof_subset(&self.del_hashes, &targets, self.node.chain().acc().leaves)
            .unwrap();
        UtreexoTx {
            tx,
            unconfirmed,
            targets,
            proof_hashes: proof.hashes,
            leaf_data: mine
                .iter()
                .map(|k| self.fixture.leaf_data[*k].clone())
                .collect(),
        }
    }

    /// Admits `utx`, returning its txid.
    fn admit(&self, mempool: &mut Mempool, utx: UtreexoTx) -> Result<Txid, Reject> {
        mempool.admit(utx, self.node.chain()).map(|taken| taken[0])
    }
}

#[test]
fn transactions_spending_confirmed_outputs_are_taken() {
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 1);
    for i in 1..=4 {
        let utx = relay.utx(i);
        assert!(utx.unconfirmed.iter().all(|u| !u));
        let txid = relay.admit(&mut mempool, utx).unwrap();
        assert_eq!(txid, relay.block_txids()[i]);
        let entry = mempool.get(&txid).unwrap();
        assert!(entry.fee > 0 && entry.vsize > 0);
    }
    let stats = mempool.stats();
    assert_eq!(stats.transactions, 4);
    assert!(stats.bytes > 0 && stats.fees > 0 && stats.min_rate > 0);
    // again: held already
    assert!(matches!(
        relay.admit(&mut mempool, relay.utx(1)),
        Err(Reject::Known)
    ));
}

#[test]
fn the_proof_is_only_the_hashes_at_the_positions_asked_for() {
    let relay = Relay::new();
    let leaves = relay.node.chain().acc().leaves;
    for i in 1..=4 {
        let utx = relay.utx(i);
        assert_eq!(
            proof_positions(&utx.targets, leaves).len(),
            utx.proof_hashes.len()
        );
    }
}

#[test]
fn an_orphan_waits_for_its_parent() {
    let relay = Relay::new();
    let txids = relay.block_txids();
    let mut mempool = Mempool::new(1 << 20, 2);
    // transaction 5 spends an output of transaction 4
    let child = relay.utx(5);
    assert_eq!(child.unconfirmed, [true]);
    assert!(child.targets.is_empty() && child.proof_hashes.is_empty());
    match relay.admit(&mut mempool, child.clone()) {
        Err(Reject::Orphan(parents)) => assert_eq!(parents, [txids[4]]),
        other => panic!("{other:?}"),
    }
    // kept aside once
    assert!(matches!(
        relay.admit(&mut mempool, child),
        Err(Reject::Orphan(_))
    ));
    assert_eq!(mempool.stats().orphans, 1);
    assert_eq!(mempool.stats().transactions, 0);
    // its parent comes: both are held
    let taken = mempool.admit(relay.utx(4), relay.node.chain()).unwrap();
    assert_eq!(taken, [txids[4], txids[5]]);
    let stats = mempool.stats();
    assert_eq!((stats.transactions, stats.orphans), (2, 0));
}

#[test]
fn a_block_drops_the_orphans() {
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 12);
    assert!(relay.admit(&mut mempool, relay.utx(5)).is_err());
    assert_eq!(mempool.stats().orphans, 1);
    relay
        .node
        .connect_block(
            111,
            &relay.fixture.blocks[110],
            &relay.fixture.leaf_data,
            relay.fixture.proof.clone(),
        )
        .unwrap();
    mempool.block_connected(&relay.fixture.blocks[110]);
    assert_eq!(mempool.stats().orphans, 0);
}

#[test]
fn scripts_are_checked_when_asked_for() {
    use ecsn::scripts::ScriptRules;
    use floresta_chain::ChainParams;
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 13);
    mempool.check_scripts(
        ecsn_consensus::verify_scripts,
        ScriptRules::new(&ChainParams::from(Network::Regtest)),
    );
    // a signature changed: same outputs spent, same proof
    let mut utx = relay.utx(1);
    let input = &mut utx.tx.input[0];
    if input.witness.is_empty() {
        let mut script = input.script_sig.to_bytes();
        script[5] ^= 1;
        input.script_sig = script.into();
    } else {
        let mut items: Vec<Vec<u8>> = input.witness.iter().map(<[u8]>::to_vec).collect();
        items[0][5] ^= 1;
        input.witness = bitcoin::Witness::from_slice(&items);
    }
    assert!(matches!(
        relay.admit(&mut mempool, utx),
        Err(Reject::Script(0))
    ));
    for i in 1..=5 {
        relay.admit(&mut mempool, relay.utx(i)).unwrap();
    }
    assert_eq!(mempool.stats().transactions, 5);
}

#[test]
fn a_changed_proof_hash_is_refused() {
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 3);
    let mut utx = relay.utx(1);
    let BitcoinNodeHash::Some(mut hash) = utx.proof_hashes[0] else {
        panic!("an empty proof hash")
    };
    hash[0] ^= 1;
    utx.proof_hashes[0] = BitcoinNodeHash::Some(hash);
    assert!(matches!(
        relay.admit(&mut mempool, utx),
        Err(Reject::InvalidProof)
    ));
    assert_eq!(mempool.stats().transactions, 0);
}

#[test]
fn a_changed_amount_is_refused() {
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 4);
    let mut utx = relay.utx(1);
    utx.leaf_data[0].amount += 1;
    assert!(matches!(
        relay.admit(&mut mempool, utx),
        Err(Reject::InvalidProof)
    ));
}

#[test]
fn missing_leaf_data_is_refused() {
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 5);
    let mut utx = relay.utx(1);
    utx.leaf_data.pop();
    assert!(matches!(
        relay.admit(&mut mempool, utx),
        Err(Reject::Malformed)
    ));
}

#[test]
fn spending_more_than_its_inputs_is_refused() {
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 6);
    let mut utx = relay.utx(3);
    utx.tx.output[0].value += Amount::from_btc(100.0).unwrap();
    assert!(matches!(
        relay.admit(&mut mempool, utx),
        Err(Reject::Invalid(_))
    ));
}

#[test]
fn a_lock_time_ahead_is_refused() {
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 7);
    let mut utx = relay.utx(3);
    utx.tx.lock_time = absolute::LockTime::from_height(200).unwrap();
    utx.tx.input[0].sequence = Sequence::ENABLE_LOCKTIME_NO_RBF;
    assert!(matches!(
        relay.admit(&mut mempool, utx.clone()),
        Err(Reject::NotFinal)
    ));
    // reached for the next block, 111
    utx.tx.lock_time = absolute::LockTime::from_height(110).unwrap();
    relay.admit(&mut mempool, utx).unwrap();
}

#[test]
fn a_conflict_replaces_only_paying_more() {
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 8);
    let first = relay.admit(&mut mempool, relay.utx(1)).unwrap();
    // transaction 7 spends an output of transaction 1
    let child = relay.admit(&mut mempool, relay.utx(7)).unwrap();
    let fees = mempool.stats().fees;

    // the same inputs paying less
    let mut cheaper = relay.utx(1);
    cheaper.tx.output[1].value += Amount::from_sat(1);
    assert!(matches!(
        relay.admit(&mut mempool, cheaper),
        Err(Reject::Conflict)
    ));
    // paying more than both together, and at a higher rate
    let mut better = relay.utx(1);
    better.tx.output[1].value -= Amount::from_sat(fees);
    let replacement = relay.admit(&mut mempool, better).unwrap();
    assert!(!mempool.contains(&first));
    assert!(!mempool.contains(&child));
    assert!(mempool.contains(&replacement));
    assert_eq!(mempool.stats().transactions, 1);
}

#[test]
fn beyond_the_cap_the_lowest_fee_rate_leaves() {
    let relay = Relay::new();
    let utxs: Vec<_> = (1..=4).map(|i| relay.utx(i)).collect();
    let size: usize = utxs.iter().map(|u| u.tx.total_size()).sum();
    let mut mempool = Mempool::new(size - 1, 9);
    let mut rates = Vec::new();
    for utx in &utxs {
        let txid = utx.tx.compute_txid();
        match relay.admit(&mut mempool, utx.clone()) {
            Ok(_) => rates.push((mempool.get(&txid).unwrap().rate, txid)),
            Err(Reject::LowFeeRate) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!(mempool.stats().transactions, 3);
    assert!(mempool.stats().bytes < size);
    let lowest = rates.iter().min().unwrap();
    let held: Vec<_> = rates.iter().filter(|(_, t)| mempool.contains(t)).collect();
    assert!(held.iter().all(|(rate, _)| *rate >= lowest.0));
}

#[test]
fn the_block_takes_its_transactions_and_their_conflicts_out() {
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 10);
    for i in [1, 2, 4, 5, 7] {
        relay.admit(&mut mempool, relay.utx(i)).unwrap();
    }
    // a transaction the block does not hold, spending what its transaction 3 spends
    let mut other = relay.utx(3);
    other.tx.output[1].value -= Amount::from_sat(1000);
    let other = relay.admit(&mut mempool, other).unwrap();
    assert_eq!(mempool.stats().transactions, 6);

    let block = &relay.fixture.blocks[110];
    relay
        .node
        .connect_block(
            111,
            block,
            &relay.fixture.leaf_data,
            relay.fixture.proof.clone(),
        )
        .unwrap();
    mempool.block_connected(block);
    assert!(!mempool.contains(&other));
    let stats = mempool.stats();
    assert_eq!(stats.transactions, 0);
    assert_eq!((stats.bytes, stats.fees), (0, 0));
}

#[test]
fn a_block_off_the_tip_empties_the_mempool() {
    let relay = Relay::new();
    let mut mempool = Mempool::new(1 << 20, 11);
    relay.admit(&mut mempool, relay.utx(1)).unwrap();
    // block 110 again: not on block 110, the tip the transaction was checked on
    mempool.block_connected(&relay.fixture.blocks[109]);
    assert_eq!(mempool.stats().transactions, 0);
}

/// A bridge that serves blocks up to 110, then relays block 111's transactions as utreexod does
/// (announced with the positions of the leaves they spend, sent with the proof hashes asked
/// for), then block 111, checking the node's mempool on the way. Its announcements: 1; 4; 5, a
/// child of 4, whose answer comes first; 4 again with 2. It also sends 3 unasked. Hangs up at
/// the end.
async fn relaying_bridge(io: DuplexStream, relay: &Relay, node: &Node<MemChainStore>) {
    let fixture = &relay.fixture;
    let leaves = relay.node.chain().acc().leaves;
    let txids = relay.block_txids();
    let mut conn = Connection::new(FromTokio::new(io), Network::Regtest);
    let group = |i: usize| {
        let mut entries = vec![Inventory {
            kind: inv::TX,
            hash: txids[i].to_byte_array(),
        }];
        entries.extend(
            pack_positions(&relay.utx(i).targets)
                .into_iter()
                .map(|hash| Inventory {
                    kind: inv::PROOF_POSITIONS,
                    hash,
                }),
        );
        entries
    };
    let mut headers = 110;
    // the transactions asked for, in order, those answered, and 4 while 5 is not answered
    let mut asked = Vec::new();
    let mut answered = Vec::new();
    let mut held = None;
    while let Ok(message) = conn.recv().await {
        let answers = match message {
            Message::Version(theirs) => {
                // asking for transactions as a Utreexo node
                assert!(theirs.relay);
                assert_ne!(theirs.services & services::UTREEXO, 0);
                vec![
                    Message::Version(Version {
                        version: PROTOCOL_VERSION,
                        services: services::WITNESS | services::UTREEXO | services::UTREEXO_ARCHIVE,
                        timestamp: 0,
                        receiver: NetAddress::default(),
                        sender: NetAddress::default(),
                        nonce: 1,
                        user_agent: "/fixture/".into(),
                        start_height: 110,
                        relay: true,
                    }),
                    Message::Verack,
                ]
            }
            Message::GetHeaders { .. } => vec![Message::Headers(
                fixture.blocks[..headers].iter().map(|b| b.header).collect(),
            )],
            Message::GetData(list) if list[0].tx().is_some() => {
                let mut answers = Vec::new();
                let mut i = 0;
                while i < list.len() {
                    let txid = list[i].tx().unwrap();
                    assert_eq!(list[i], Inventory::utreexo_tx(txid));
                    let end = list[i + 1..]
                        .iter()
                        .position(|e| e.kind != inv::PROOF_POSITIONS)
                        .map_or(list.len(), |n| i + 1 + n);
                    let n = txids.iter().position(|t| *t == txid).unwrap();
                    let utx = relay.utx(n);
                    // the positions of the proof hashes of the leaves it spends, all of them
                    let packed: Vec<_> = list[i + 1..end].iter().map(|e| e.hash).collect();
                    assert_eq!(
                        unpack_positions(&packed),
                        proof_positions(&utx.targets, leaves)
                    );
                    asked.push(n);
                    match n {
                        4 if !asked.contains(&5) => held = Some(utx),
                        5 => {
                            answers.push(Message::UtreexoTx(utx));
                            answers.extend(held.take().map(Message::UtreexoTx));
                            answered.extend([5, 4]);
                            // not asked for
                            answers.push(Message::UtreexoTx(relay.utx(3)));
                        }
                        _ => {
                            answers.push(Message::UtreexoTx(utx));
                            answered.push(n);
                        }
                    }
                    i = end;
                }
                if [1, 2, 4, 5].iter().all(|n| answered.contains(n)) {
                    answers.push(Message::Ping(7));
                }
                answers
            }
            Message::GetData(list) => {
                let blocks: Vec<_> = list
                    .iter()
                    .filter_map(|inv| inv.block())
                    .map(|hash| {
                        let block = fixture.blocks.iter().find(|b| b.block_hash() == hash);
                        Message::Block(block.unwrap().clone())
                    })
                    .collect();
                let mut answers = blocks;
                if list
                    .iter()
                    .any(|e| e.block() == Some(fixture.blocks[109].block_hash()))
                {
                    // synced: the transactions
                    answers.push(Message::Inv(group(1)));
                    answers.push(Message::Inv(group(4)));
                    answers.push(Message::Inv(group(5)));
                    let mut last = group(4);
                    last.extend(group(2));
                    answers.push(Message::Inv(last));
                }
                answers
            }
            Message::GetUtreexoProof(get) => {
                assert_eq!(get.block_hash, fixture.blocks[110].block_hash());
                vec![
                    Message::UtreexoProof(UtreexoProof {
                        block_hash: get.block_hash,
                        proof_hashes: fixture.proof.hashes.clone(),
                        targets: fixture.proof.targets.clone(),
                        leaf_data: fixture.leaf_data.clone(),
                    }),
                    Message::Ping(8),
                ]
            }
            Message::Pong(7) => {
                // each asked for once
                assert_eq!(asked, [1, 4, 5, 2]);
                // 5 waited for 4; 3 refused, not asked for
                let status = node.status();
                assert_eq!((status.taken, status.refused), (4, 1));
                assert_eq!(status.mempool.transactions, 4);
                assert_eq!(status.mempool.orphans, 0);
                for i in [1, 2, 4, 5] {
                    assert!(node.mempool().contains(&txids[i]));
                }
                // block 111, announced
                headers = 111;
                vec![Message::Headers(vec![fixture.blocks[110].header])]
            }
            Message::Pong(8) => {
                // block 111 took its transactions out
                assert_eq!(node.chain().get_validation_index().unwrap(), 111);
                assert_eq!(node.status().mempool.transactions, 0);
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
    panic!("the node hung up");
}

#[test]
fn the_bridge_relays_transactions_once_synced() {
    let relay = Relay::new();
    let config = Config {
        network: Network::Regtest,
        user_agent: String::new(),
        assume_utreexo: None,
        blocks_in_flight: 8,
        ipv6: false,
        v2: true,
        v2_bridge: true,
        free_memory: None,
        mempool: 1 << 20,
        verify_scripts: None,
    };
    let node = Node::new(MemChainStore::new(), config, 0).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let ours = async {
            let mut conn = Connection::new(FromTokio::new(ours), Network::Regtest);
            // ends when the bridge hangs up
            assert!(node.run_bridge(&mut conn, 0, None, 0).await.is_err());
        };
        tokio::join!(ours, relaying_bridge(theirs, &relay, &node));
    });
    assert_eq!(node.status().last_block.unwrap().height, 111);
}
