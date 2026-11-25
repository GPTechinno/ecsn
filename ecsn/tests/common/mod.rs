//! The regtest chain recorded from utreexod (a bridge node): blocks 1 to 110 of their coinbase
//! alone, then block 111, whose transactions spend six coinbase outputs of earlier blocks with
//! their Utreexo proof, and the accumulator utreexod has after it.

// each test crate uses part of it
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use bitcoin::block::{Header, Version};
use bitcoin::consensus::encode::{deserialize_hex, Decodable, VarInt};
use bitcoin::hashes::Hash;
use bitcoin::{Block, TxMerkleNode};
use ecsn::flash::{Flash, RamFlash, RamFlashError};
use floresta_chain::{CompactLeafData, ScriptPubKeyKind};
use rustreexo::node_hash::BitcoinNodeHash;
use rustreexo::proof::Proof;

pub struct Fixture {
    pub blocks: Vec<Block>,
    pub leaf_data: Vec<CompactLeafData>,
    pub proof: Proof,
    pub leaves: u64,
    pub roots: Vec<String>,
}

/// BIP 183's serialized Utreexo data: targets, proof hashes, then the spent outputs.
pub fn udata(hex: &str) -> (Vec<CompactLeafData>, Proof) {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let r = &mut &bytes[..];
    let n = VarInt::consensus_decode(r).unwrap().0;
    let targets = (0..n)
        .map(|_| VarInt::consensus_decode(r).unwrap().0)
        .collect();
    let n = VarInt::consensus_decode(r).unwrap().0;
    let hashes = (0..n)
        .map(|_| BitcoinNodeHash::Some(<[u8; 32]>::consensus_decode(r).unwrap()))
        .collect();
    let n = VarInt::consensus_decode(r).unwrap().0;
    let leaf_data = (0..n)
        .map(|_| CompactLeafData {
            header_code: u32::consensus_decode(r).unwrap(),
            amount: u64::consensus_decode(r).unwrap(),
            spk_ty: ScriptPubKeyKind::consensus_decode(r).unwrap(),
        })
        .collect();
    assert!(r.is_empty());
    (leaf_data, Proof { targets, hashes })
}

pub fn fixture() -> Fixture {
    let mut blocks = Vec::new();
    let mut proof = None;
    let mut acc = None;
    for line in include_str!("../data/regtest-111.txt").lines() {
        match line.split_once(' ') {
            Some(("block", hex)) => blocks.push(deserialize_hex(hex).unwrap()),
            Some(("udata", hex)) => proof = Some(udata(hex)),
            Some(("acc", rest)) => {
                let mut words = rest.split_whitespace();
                let leaves = words.next().unwrap().parse().unwrap();
                acc = Some((leaves, words.map(String::from).collect()));
            }
            _ => {}
        }
    }
    let (leaf_data, proof) = proof.unwrap();
    let (leaves, roots) = acc.unwrap();
    Fixture {
        blocks,
        leaf_data,
        proof,
        leaves,
        roots,
    }
}

/// A flash the test keeps a handle on while a node owns the store.
#[derive(Clone)]
pub struct Shared(Rc<RefCell<RamFlash>>);

impl Shared {
    pub fn new(capacity: u32) -> Self {
        Self(Rc::new(RefCell::new(RamFlash::new(capacity))))
    }

    /// The flash after a reset now: a copy of what is programmed.
    pub fn reset(&self) -> Self {
        let bytes = self.0.borrow().bytes().to_vec();
        Self(Rc::new(RefCell::new(RamFlash::from_bytes(bytes))))
    }
}

impl Flash for Shared {
    type Error = RamFlashError;
    const ERASE_SIZE: u32 = RamFlash::ERASE_SIZE;
    const WRITE_SIZE: u32 = RamFlash::WRITE_SIZE;

    fn capacity(&self) -> u32 {
        self.0.borrow().capacity()
    }

    fn read(&self, offset: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.0.borrow().read(offset, buf)
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), Self::Error> {
        self.0.borrow_mut().write(offset, data)
    }

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.0.borrow_mut().erase(from, to)
    }
}

/// `n` headers mined on `tip`, a second apart (regtest's proof of work takes a few tries).
pub fn mine_headers(tip: &Header, n: usize) -> Vec<Header> {
    let mut headers: Vec<Header> = Vec::with_capacity(n);
    let mut prev = *tip;
    for _ in 0..n {
        let mut header = Header {
            version: Version::from_consensus(0x2000_0000),
            prev_blockhash: prev.block_hash(),
            merkle_root: TxMerkleNode::all_zeros(),
            time: prev.time + 1,
            bits: prev.bits,
            nonce: 0,
        };
        while header.validate_pow(header.target()).is_err() {
            header.nonce += 1;
        }
        headers.push(header);
        prev = header;
    }
    headers
}
