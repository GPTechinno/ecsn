//! Validation of a regtest chain recorded from utreexod (a bridge node): blocks 1 to 110 of
//! their coinbase alone, then block 111, whose transactions spend six coinbase outputs of
//! earlier blocks with their Utreexo proof, and the accumulator utreexod has after it.

mod common;

use bitcoin::Network;
use common::{fixture, Fixture};
use ecsn::node::{Config, Node};
use ecsn::store::MemChainStore;
use floresta_chain::pruned_utreexo::UpdatableChainstate;
use floresta_chain::{BlockchainInterface, CompactLeafData};
use rustreexo::node_hash::BitcoinNodeHash;
use rustreexo::proof::Proof;

/// A node with every header, and every block validated up to 110.
fn node_at_110(fixture: &Fixture) -> Node<MemChainStore> {
    let config = Config {
        network: Network::Regtest,
        user_agent: String::new(),
        assume_utreexo: None,
        blocks_in_flight: 1,
        ipv6: false,
    };
    let node = Node::new(MemChainStore::new(), config, 0).unwrap();
    for block in &fixture.blocks {
        node.chain().accept_header(block.header).unwrap();
    }
    for (i, block) in fixture.blocks[..110].iter().enumerate() {
        node.connect_block(i as u32 + 1, block, &[], Proof::default())
            .unwrap();
    }
    assert_eq!(node.chain().get_validation_index().unwrap(), 110);
    node
}

fn connect_111(
    node: &Node<MemChainStore>,
    fixture: &Fixture,
    leaf_data: &[CompactLeafData],
    proof: Proof,
) -> bool {
    node.connect_block(111, &fixture.blocks[110], leaf_data, proof)
        .is_ok()
}

#[test]
fn the_proven_block_gives_utreexods_accumulator() {
    let fixture = fixture();
    assert_eq!(fixture.blocks.len(), 111);
    assert_eq!(fixture.blocks[110].txdata.len(), 9);
    assert_eq!(fixture.leaf_data.len(), 6);
    let node = node_at_110(&fixture);
    assert!(connect_111(
        &node,
        &fixture,
        &fixture.leaf_data,
        fixture.proof.clone()
    ));
    let acc = node.chain().acc();
    assert_eq!(acc.leaves, fixture.leaves);
    let roots: Vec<String> = acc.roots.iter().map(ToString::to_string).collect();
    assert_eq!(roots, fixture.roots);
}

#[test]
fn a_changed_proof_hash_is_refused() {
    let fixture = fixture();
    let node = node_at_110(&fixture);
    let mut proof = fixture.proof.clone();
    let BitcoinNodeHash::Some(mut hash) = proof.hashes[0] else {
        panic!("an empty proof hash")
    };
    hash[0] ^= 1;
    proof.hashes[0] = BitcoinNodeHash::Some(hash);
    assert!(!connect_111(&node, &fixture, &fixture.leaf_data, proof));
    // nothing changed: the right proof still goes through
    assert!(connect_111(
        &node,
        &fixture,
        &fixture.leaf_data,
        fixture.proof.clone()
    ));
}

#[test]
fn a_changed_amount_is_refused() {
    let fixture = fixture();
    let node = node_at_110(&fixture);
    let mut leaf_data = fixture.leaf_data.clone();
    leaf_data[0].amount += 1;
    assert!(!connect_111(
        &node,
        &fixture,
        &leaf_data,
        fixture.proof.clone()
    ));
}

#[test]
fn a_changed_creation_height_is_refused() {
    let fixture = fixture();
    let node = node_at_110(&fixture);
    let mut leaf_data = fixture.leaf_data.clone();
    leaf_data[0].header_code += 2;
    assert!(!connect_111(
        &node,
        &fixture,
        &leaf_data,
        fixture.proof.clone()
    ));
}

#[test]
fn swapped_targets_are_refused() {
    let fixture = fixture();
    let node = node_at_110(&fixture);
    let mut proof = fixture.proof.clone();
    proof.targets.swap(0, 1);
    assert!(!connect_111(&node, &fixture, &fixture.leaf_data, proof));
}

#[test]
fn a_missing_leaf_is_refused() {
    let fixture = fixture();
    let node = node_at_110(&fixture);
    let leaf_data = &fixture.leaf_data[..5];
    assert!(!connect_111(
        &node,
        &fixture,
        leaf_data,
        fixture.proof.clone()
    ));
}

#[test]
fn the_validated_tip_again_is_refused() {
    let fixture = fixture();
    let node = node_at_110(&fixture);
    let acc = node.chain().acc();
    assert!(node
        .connect_block(110, &fixture.blocks[109], &[], Proof::default())
        .is_err());
    // its outputs are not added a second time
    assert_eq!(node.chain().acc(), acc);
}

#[test]
fn a_block_at_another_height_is_refused() {
    let fixture = fixture();
    let node = node_at_110(&fixture);
    assert!(node
        .connect_block(
            112,
            &fixture.blocks[110],
            &fixture.leaf_data,
            fixture.proof.clone()
        )
        .is_err());
    assert!(node
        .connect_block(111, &fixture.blocks[109], &[], Proof::default())
        .is_err());
}
