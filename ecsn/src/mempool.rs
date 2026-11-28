//! The mempool: unconfirmed transactions relayed with their Utreexo proofs (BIP 183's
//! `utreexotx`), each checked when it comes against the accumulator of the validated tip: its
//! proof, the outputs it spends (in the accumulator, or of transactions held), its amounts, the
//! maturity of the coinbase outputs it spends, its lock times, and its scripts when it is given a
//! script interpreter ([`Mempool::check_scripts`]).
//!
//! A transaction checked stays valid until a block confirms it or spends one of the outputs it
//! spends, which the block's inputs tell, so its proof is not kept. A block that does not follow
//! the tip they were checked against (a reorganisation) empties the mempool. The indexes use short
//! keyed ids in B-trees: small allocations only, none growing into a large table. Beyond its cap
//! (the transactions' serialized bytes) the lowest fee rates leave first, with what spends from
//! them.
//!
//! A transaction spending an unconfirmed output of a transaction not held (an orphan) waits aside,
//! unchecked, until that one comes, and at most until the next block: [`Reject::Orphan`] names
//! the transactions it waits for.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use bitcoin::hashes::{siphash24, Hash};
use bitcoin::{absolute, transaction, Block, BlockHash, OutPoint, Transaction, TxOut, Txid};
use floresta_chain::proof_util::process_proof;
use floresta_chain::pruned_utreexo::consensus::{Consensus, LockTimeCutoff};
use floresta_chain::{BlockchainError, BlockchainInterface, ChainState, ChainStore, UtxoData};
use rustreexo::node_hash::BitcoinNodeHash;
use rustreexo::proof::Proof;

use crate::p2p::UtreexoTx;
use crate::scripts::{self, ScriptRules, VerifyScripts};

/// Why a transaction was not taken.
#[derive(Debug)]
pub enum Reject {
    /// Already held.
    Known,
    /// Its inputs, flags, targets and leaf data do not match, or it spends an output a
    /// transaction held does not have.
    Malformed,
    /// Its leaf data or the transaction against the outputs it spends: amounts, maturity,
    /// relative lock times, sizes.
    Invalid(BlockchainError),
    /// Its proof does not prove the outputs it spends in the accumulator.
    InvalidProof,
    /// It spends unconfirmed outputs of these transactions, not held: it waits for them aside.
    Orphan(Vec<Txid>),
    /// Its absolute lock time is not reached for the next block.
    NotFinal,
    /// The script of this input fails.
    Script(usize),
    /// It spends an output a transaction held spends, without paying more than it (and what
    /// spends from it) in all and per vbyte.
    Conflict,
    /// Its fee rate is the lowest while the mempool is full.
    LowFeeRate,
}

impl From<BlockchainError> for Reject {
    fn from(e: BlockchainError) -> Self {
        Self::Invalid(e)
    }
}

/// A transaction held.
#[derive(Debug)]
pub struct Entry {
    pub tx: Transaction,
    pub txid: Txid,
    /// Satoshis.
    pub fee: u64,
    pub vsize: u64,
    /// Its serialized size.
    pub size: usize,
    /// Satoshis per 1,000 vbytes.
    pub rate: u64,
}

/// What the mempool holds, for a display.
#[derive(Clone, Copy, Debug, Default)]
pub struct MempoolStats {
    pub transactions: usize,
    /// Their serialized bytes.
    pub bytes: usize,
    /// Satoshis.
    pub fees: u64,
    /// The lowest fee rate held, satoshis per 1,000 vbytes.
    pub min_rate: u64,
    /// Orphans waiting for a transaction.
    pub orphans: usize,
}

/// The most orphans kept, and their most bytes (transactions and proofs): the oldest leave.
const ORPHANS: usize = 100;
const ORPHAN_BYTES: usize = 500_000;

pub struct Mempool {
    /// The keys of the short ids.
    k0: u64,
    k1: u64,
    /// The most serialized bytes held.
    cap: usize,
    txs: BTreeMap<u64, Entry>,
    /// The transaction spending an output, by their short ids.
    spent: BTreeMap<u64, u64>,
    /// By fee rate, the lowest first.
    by_rate: BTreeSet<(u64, u64)>,
    bytes: usize,
    fees: u64,
    /// The block the transactions were checked on top of.
    tip: Option<BlockHash>,
    /// Orphans by arrival, with their txid and size.
    orphans: BTreeMap<u64, (Txid, UtreexoTx, usize)>,
    orphan_bytes: usize,
    arrivals: u64,
    /// The script interpreter and when each rule applies, if scripts are checked.
    scripts: Option<(VerifyScripts, ScriptRules)>,
}

impl Mempool {
    /// An empty mempool holding up to `cap` serialized bytes; `seed` keys its short ids.
    pub fn new(cap: usize, seed: u64) -> Self {
        Self {
            k0: seed,
            k1: seed.rotate_left(32) ^ 0x6d65_6d70_6f6f_6c00,
            cap,
            txs: BTreeMap::new(),
            spent: BTreeMap::new(),
            by_rate: BTreeSet::new(),
            bytes: 0,
            fees: 0,
            tip: None,
            orphans: BTreeMap::new(),
            orphan_bytes: 0,
            arrivals: 0,
            scripts: None,
        }
    }

    /// Checks the scripts of the transactions it takes with `verify`, under the flags of the
    /// next block.
    pub fn check_scripts(&mut self, verify: VerifyScripts, rules: ScriptRules) {
        self.scripts = Some((verify, rules));
    }

    fn short(&self, bytes: &[u8]) -> u64 {
        siphash24::Hash::hash_with_keys(self.k0, self.k1, bytes).as_u64()
    }

    fn short_txid(&self, txid: &Txid) -> u64 {
        self.short(txid.as_byte_array())
    }

    fn short_outpoint(&self, outpoint: &OutPoint) -> u64 {
        let mut bytes = [0; 36];
        bytes[..32].copy_from_slice(outpoint.txid.as_byte_array());
        bytes[32..].copy_from_slice(&outpoint.vout.to_le_bytes());
        self.short(&bytes)
    }

    pub fn contains(&self, txid: &Txid) -> bool {
        self.txs.contains_key(&self.short_txid(txid))
    }

    pub fn get(&self, txid: &Txid) -> Option<&Entry> {
        self.txs.get(&self.short_txid(txid))
    }

    pub fn stats(&self) -> MempoolStats {
        MempoolStats {
            transactions: self.txs.len(),
            bytes: self.bytes,
            fees: self.fees,
            min_rate: self.by_rate.first().map_or(0, |(rate, _)| *rate),
            orphans: self.orphans.len(),
        }
    }

    /// Checks `utx` against the accumulator of `chain`'s validated tip and the transactions
    /// held, and holds it, then the orphans that waited for it. Returns the txids of those held:
    /// its own, then the orphans'.
    pub fn admit<S: ChainStore>(
        &mut self,
        utx: UtreexoTx,
        chain: &ChainState<S>,
    ) -> Result<Vec<Txid>, Reject> {
        let mut taken = vec![self.admit_one(utx, chain)?];
        let mut i = 0;
        while i < taken.len() {
            let parent = taken[i];
            i += 1;
            let waiting: Vec<u64> = self
                .orphans
                .iter()
                .filter(|(_, (_, utx, _))| {
                    utx.tx
                        .input
                        .iter()
                        .zip(&utx.unconfirmed)
                        .any(|(input, u)| *u && input.previous_output.txid == parent)
                })
                .map(|(arrival, _)| *arrival)
                .collect();
            for arrival in waiting {
                let Some((_, utx, size)) = self.orphans.remove(&arrival) else {
                    continue;
                };
                self.orphan_bytes -= size;
                // one still waiting for another transaction is kept aside again
                if let Ok(txid) = self.admit_one(utx, chain) {
                    taken.push(txid);
                }
            }
        }
        Ok(taken)
    }

    fn admit_one<S: ChainStore>(
        &mut self,
        utx: UtreexoTx,
        chain: &ChainState<S>,
    ) -> Result<Txid, Reject> {
        if utx.unconfirmed.len() != utx.tx.input.len() {
            return Err(Reject::Malformed);
        }
        let txid = utx.tx.compute_txid();
        let id = self.short_txid(&txid);
        if self.txs.contains_key(&id) {
            return Err(Reject::Known);
        }
        let tip = chain.get_validation_index()?;
        let height = tip + 1;
        let mtp = chain.get_mtp_by_height(tip)?;
        let tip_hash = chain.get_block_hash(tip)?;
        if self.tip != Some(tip_hash) {
            // checked on top of another block, which a block connected did not follow
            self.clear();
            self.tip = Some(tip_hash);
        }

        // the transactions not held whose outputs it spends: it waits for them
        let mut parents = Vec::new();
        for (input, _) in utx
            .tx
            .input
            .iter()
            .zip(&utx.unconfirmed)
            .filter(|(_, u)| **u)
        {
            let parent = input.previous_output.txid;
            if !self.txs.contains_key(&self.short_txid(&parent)) && !parents.contains(&parent) {
                parents.push(parent);
            }
        }
        if !parents.is_empty() {
            self.keep_orphan(txid, utx);
            return Err(Reject::Orphan(parents));
        }
        let UtreexoTx {
            tx,
            unconfirmed,
            targets,
            proof_hashes,
            leaf_data,
        } = utx;

        // the confirmed inputs: the outputs they spend from their leaf data, and the proof
        let confirmed: Vec<_> = tx
            .input
            .iter()
            .zip(&unconfirmed)
            .filter(|(_, u)| !**u)
            .map(|(input, _)| input.clone())
            .collect();
        if confirmed.len() != leaf_data.len() || confirmed.len() != targets.len() {
            return Err(Reject::Malformed);
        }
        // the proof's processing skips the first transaction, a block's coinbase
        let placeholder = Transaction {
            version: transaction::Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: Vec::new(),
            output: Vec::new(),
        };
        let spending = Transaction {
            version: tx.version,
            lock_time: tx.lock_time,
            input: confirmed,
            output: tx.output.clone(),
        };
        let (del_hashes, mut utxos) = process_proof::<_, _, BlockchainError>(
            &leaf_data,
            &[placeholder, spending],
            height,
            |h| chain.get_block_hash(h),
            |h| chain.get_mtp_by_height(h),
        )?;
        if !del_hashes.is_empty() {
            let del_hashes: Vec<_> = del_hashes
                .iter()
                .map(|h| BitcoinNodeHash::Some(h.to_byte_array()))
                .collect();
            let proof = Proof::new(targets, proof_hashes);
            if !matches!(chain.acc().verify(&proof, &del_hashes), Ok(true)) {
                return Err(Reject::InvalidProof);
            }
        }

        // the unconfirmed inputs: outputs of transactions held
        for (input, _) in tx.input.iter().zip(&unconfirmed).filter(|(_, u)| **u) {
            let prevout = input.previous_output;
            let parent = self
                .txs
                .get(&self.short_txid(&prevout.txid))
                .ok_or(Reject::Malformed)?;
            let txout = parent
                .tx
                .output
                .get(prevout.vout as usize)
                .ok_or(Reject::Malformed)?;
            utxos.insert(
                prevout,
                UtxoData {
                    txout: txout.clone(),
                    is_coinbase: false,
                    creation_height: height,
                    creation_time: mtp,
                },
            );
        }

        // the outputs it spends in the order of its inputs, for its scripts
        let spent: Option<Vec<TxOut>> = self.scripts.as_ref().and_then(|_| {
            tx.input
                .iter()
                .map(|input| utxos.get(&input.previous_output).map(|u| u.txout.clone()))
                .collect()
        });

        // the transaction against them, for the next block
        let (in_value, out_value) = Consensus::verify_transaction(
            &tx,
            &mut utxos,
            height,
            LockTimeCutoff::Mtp(mtp),
            false,
            0,
        )?;
        if !is_final(&tx, height, mtp) {
            return Err(Reject::NotFinal);
        }
        // the scripts last, the costliest
        if let (Some((verify, rules)), Some(spent)) = (&self.scripts, &spent) {
            scripts::check(*verify, &tx, spent, rules.flags(height, None))
                .map_err(Reject::Script)?;
        }
        let fee = (in_value - out_value).to_sat();
        self.hold(tx, txid, id, fee)
    }

    /// Holds `tx`, checked, which pays `fee`: in place of the transactions it conflicts with if
    /// it pays more, then under the cap.
    fn hold(&mut self, tx: Transaction, txid: Txid, id: u64, fee: u64) -> Result<Txid, Reject> {
        let vsize = tx.vsize() as u64;
        let size = tx.total_size();
        let rate = fee * 1000 / vsize.max(1);

        // conflicts: replaced if it pays more than them and what spends from them, in all and
        // per vbyte
        let mut gone = BTreeSet::new();
        for input in &tx.input {
            if let Some(other) = self.spent.get(&self.short_outpoint(&input.previous_output)) {
                self.descendants(*other, &mut gone);
            }
        }
        if !gone.is_empty() {
            let gone_fees: u64 = gone.iter().map(|g| self.txs[g].fee).sum();
            let gone_rate = gone.iter().map(|g| self.txs[g].rate).max().unwrap_or(0);
            if fee <= gone_fees || rate <= gone_rate {
                return Err(Reject::Conflict);
            }
            for g in gone {
                self.remove(g);
            }
        }

        for input in &tx.input {
            let spent = self.short_outpoint(&input.previous_output);
            self.spent.insert(spent, id);
        }
        self.by_rate.insert((rate, id));
        self.bytes += size;
        self.fees += fee;
        self.txs.insert(
            id,
            Entry {
                tx,
                txid,
                fee,
                vsize,
                size,
                rate,
            },
        );

        // room under the cap: the lowest fee rates leave
        while self.bytes > self.cap {
            let Some(&(_, lowest)) = self.by_rate.first() else {
                break;
            };
            self.remove_with_descendants(lowest);
        }
        if !self.txs.contains_key(&id) {
            return Err(Reject::LowFeeRate);
        }
        Ok(txid)
    }

    /// Holds `tx` as if it had passed admission, paying `fee`: for measurements of the mempool's
    /// memory (`ecsn-replay`), which have the transactions without what they spend.
    #[cfg(feature = "unchecked")]
    #[doc(hidden)]
    pub fn hold_unchecked(&mut self, tx: Transaction, fee: u64) -> Result<Txid, Reject> {
        let txid = tx.compute_txid();
        let id = self.short_txid(&txid);
        if self.txs.contains_key(&id) {
            return Err(Reject::Known);
        }
        self.hold(tx, txid, id, fee)
    }

    /// Keeps `utx` aside until the transactions it spends from come, if it is not already.
    fn keep_orphan(&mut self, txid: Txid, utx: UtreexoTx) {
        if self.orphans.values().any(|(t, _, _)| *t == txid) {
            return;
        }
        let size = utx.tx.total_size() + 32 * utx.proof_hashes.len() + 24 * utx.leaf_data.len();
        if size > ORPHAN_BYTES {
            return;
        }
        self.arrivals += 1;
        self.orphans.insert(self.arrivals, (txid, utx, size));
        self.orphan_bytes += size;
        while self.orphans.len() > ORPHANS || self.orphan_bytes > ORPHAN_BYTES {
            let Some((_, (_, _, size))) = self.orphans.pop_first() else {
                break;
            };
            self.orphan_bytes -= size;
        }
    }

    /// A block connected: on the tip the transactions were checked against, its transactions
    /// leave, and those spending an output one of them spends, with what spends from them; on
    /// another block, every transaction leaves. The orphans leave, their proofs no longer the
    /// accumulator's.
    pub fn block_connected(&mut self, block: &Block) {
        self.orphans.clear();
        self.orphan_bytes = 0;
        // a mempool that has not seen a tip yet has checked nothing against one
        let follows = self
            .tip
            .is_none_or(|tip| tip == block.header.prev_blockhash);
        self.tip = Some(block.block_hash());
        if self.txs.is_empty() {
            return;
        }
        if !follows {
            self.clear();
            return;
        }
        for tx in &block.txdata {
            let id = self.short_txid(&tx.compute_txid());
            if self.txs.contains_key(&id) {
                // what spends from it now spends a confirmed output
                self.remove(id);
                continue;
            }
            for input in &tx.input {
                if let Some(other) = self.spent.get(&self.short_outpoint(&input.previous_output)) {
                    self.remove_with_descendants(*other);
                }
            }
        }
    }

    /// Drops every transaction, and the orphans.
    pub fn clear(&mut self) {
        self.orphans.clear();
        self.orphan_bytes = 0;
        self.txs.clear();
        self.spent.clear();
        self.by_rate.clear();
        self.bytes = 0;
        self.fees = 0;
    }

    /// Adds `id` and what spends from it, from it on, to `set`.
    fn descendants(&self, id: u64, set: &mut BTreeSet<u64>) {
        if !set.insert(id) {
            return;
        }
        let Some(entry) = self.txs.get(&id) else {
            return;
        };
        for vout in 0..entry.tx.output.len() as u32 {
            let outpoint = OutPoint {
                txid: entry.txid,
                vout,
            };
            if let Some(child) = self.spent.get(&self.short_outpoint(&outpoint)) {
                self.descendants(*child, set);
            }
        }
    }

    fn remove_with_descendants(&mut self, id: u64) {
        let mut gone = BTreeSet::new();
        self.descendants(id, &mut gone);
        for g in gone {
            self.remove(g);
        }
    }

    fn remove(&mut self, id: u64) {
        let Some(entry) = self.txs.remove(&id) else {
            return;
        };
        for input in &entry.tx.input {
            let spent = self.short_outpoint(&input.previous_output);
            if self.spent.get(&spent) == Some(&id) {
                self.spent.remove(&spent);
            }
        }
        self.by_rate.remove(&(entry.rate, id));
        self.bytes -= entry.size;
        self.fees -= entry.fee;
    }
}

/// Whether `tx`'s absolute lock time lets it into the block at `height`, the previous block's
/// median time past being `mtp` (BIP 113).
fn is_final(tx: &Transaction, height: u32, mtp: u32) -> bool {
    let reached = match tx.lock_time {
        absolute::LockTime::Blocks(h) => h.to_consensus_u32() < height,
        absolute::LockTime::Seconds(t) => t.to_consensus_u32() < mtp,
    };
    tx.lock_time == absolute::LockTime::ZERO || reached || !tx.is_lock_time_enabled()
}

/// The rows a forest of `leaves` leaves has.
fn tree_rows(leaves: u64) -> u8 {
    if leaves <= 1 {
        return 0;
    }
    (64 - (leaves - 1).leading_zeros()) as u8
}

/// The rows of the forest positions are written in on the wire, whatever the accumulator's
/// size, so that they never move as it grows.
const WIRE_ROWS: u8 = 63;

fn detect_row(pos: u64, rows: u8) -> u8 {
    let mut marker: u64 = 1 << rows;
    let mut row = 0;
    while pos & marker != 0 {
        marker >>= 1;
        row += 1;
    }
    row
}

fn row_start(row: u8, rows: u8) -> u64 {
    ((2u128 << rows) - (2u128 << (rows - row))) as u64
}

/// `pos`, in a forest of `from` rows, in one of `to`.
fn translate(pos: u64, from: u8, to: u8) -> u64 {
    let row = detect_row(pos, from);
    if row == 0 {
        return pos;
    }
    pos - row_start(row, from) + row_start(row, to)
}

fn parent(pos: u64, rows: u8) -> u64 {
    (pos >> 1) | (1 << rows)
}

fn is_root(pos: u64, leaves: u64, rows: u8) -> bool {
    let row = detect_row(pos, rows);
    let mask = (2u64 << rows) - 1;
    let before = leaves & (mask << (row + 1));
    let root = ((before >> row) | (mask << (rows + 1 - row))) & mask;
    leaves & (1 << row) != 0 && root == pos
}

/// The positions of the hashes that prove the leaves at `targets` in an accumulator of `leaves`
/// leaves, both in the wire's coordinates: the siblings of the targets and of the nodes computed
/// from them up to the roots, those computed excluded; ascending.
pub fn proof_positions(targets: &[u64], leaves: u64) -> Vec<u64> {
    let rows = tree_rows(leaves);
    let mut computed: Vec<u64> = targets
        .iter()
        .map(|t| translate(*t, WIRE_ROWS, rows))
        .collect();
    computed.sort_unstable();
    computed.dedup();
    let mut known: BTreeSet<u64> = computed.iter().copied().collect();
    let mut proof = BTreeSet::new();
    let mut i = 0;
    while i < computed.len() {
        let pos = computed[i];
        i += 1;
        if is_root(pos, leaves, rows) {
            continue;
        }
        let sibling = pos ^ 1;
        if known.contains(&sibling) {
            proof.remove(&pos);
        } else {
            proof.insert(sibling);
        }
        let up = parent(pos, rows);
        if known.insert(up) {
            computed.push(up);
        }
    }
    let mut positions: Vec<u64> = proof
        .into_iter()
        .map(|p| translate(p, rows, WIRE_ROWS))
        .collect();
    positions.sort_unstable();
    positions
}

#[cfg(test)]
mod tests {
    use rustreexo::node_hash::BitcoinNodeHash;
    use rustreexo::stump::Stump;

    use super::*;

    fn hash(i: u8) -> BitcoinNodeHash {
        BitcoinNodeHash::Some([i; 32])
    }

    /// The positions agree with the proofs rustreexo makes: a proof checked by rustreexo needs
    /// exactly as many hashes as there are positions.
    #[test]
    fn proof_positions_match_rustreexo() {
        use rustreexo::mem_forest::MemForest;
        for leaves in [1u8, 2, 3, 5, 8, 13, 31, 64] {
            let mut forest = MemForest::new();
            let hashes: Vec<_> = (0..leaves).map(hash).collect();
            forest.modify(&hashes, &[]).unwrap();
            for mut target_set in [
                vec![0u8],
                vec![0, leaves - 1],
                (0..leaves).step_by(3).collect(),
            ] {
                target_set.dedup();
                let wanted: Vec<_> = target_set.iter().map(|i| hash(*i)).collect();
                let proof = forest.prove(&wanted).unwrap();
                let positions = proof_positions(&proof.targets, leaves as u64);
                assert_eq!(
                    positions.len(),
                    proof.hashes.len(),
                    "{leaves} leaves, targets {:?}",
                    proof.targets
                );
                let mut stump = Stump::new();
                stump = stump.modify(&hashes, &[], &Proof::default()).unwrap();
                assert!(
                    stump.verify(&proof, &wanted).unwrap(),
                    "{leaves} leaves, {target_set:?}"
                );
            }
        }
    }

    /// The same, hash for hash, in forests that deletions cut up (leaves moved up several rows):
    /// the hashes at the positions, taken from the forest, are rustreexo's proof, in order.
    #[test]
    fn proof_positions_are_rustreexos_after_deletions() {
        use rustreexo::mem_forest::MemForest;
        let hash = |i: u32| {
            let mut bytes = [0u8; 32];
            bytes[..4].copy_from_slice(&i.to_le_bytes());
            bytes[4] = 1;
            BitcoinNodeHash::Some(bytes)
        };
        // a small xorshift for the choices
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..20u32 {
            let mut forest = MemForest::new();
            let mut alive = Vec::new();
            let mut added = 0u32;
            // blocks of additions and deletions
            for _ in 0..(3 + round % 5) {
                let add: Vec<_> = (0..(50 + next() % 400) as u32)
                    .map(|i| hash(added + i))
                    .collect();
                added += add.len() as u32;
                let mut del = Vec::new();
                alive.retain(|h| {
                    if next() % 3 == 0 {
                        del.push(*h);
                        false
                    } else {
                        true
                    }
                });
                forest.modify(&add, &del).unwrap();
                alive.extend(add);
            }
            let leaves = u64::from(added);
            let wanted: Vec<_> = alive
                .iter()
                .filter(|_| next() % (1 + round as u64 % 7 * 5) == 0)
                .take(200)
                .copied()
                .collect();
            if wanted.is_empty() {
                continue;
            }
            let proof = forest.prove(&wanted).unwrap();
            let positions = proof_positions(&proof.targets, leaves);
            let rows = tree_rows(leaves);
            let hashes: Vec<_> = positions
                .iter()
                .map(|p| {
                    forest
                        .grab_node(translate(*p, WIRE_ROWS, rows))
                        .unwrap()
                        .0
                        .get_data()
                })
                .collect();
            assert_eq!(hashes, proof.hashes, "round {round}");
            assert!(forest
                .verify(&Proof::new(proof.targets.clone(), hashes), &wanted)
                .unwrap());
        }
    }

    #[test]
    fn a_lock_time_past_is_final() {
        let mut tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::from_height(100).unwrap(),
            input: vec![bitcoin::TxIn {
                sequence: bitcoin::Sequence::ENABLE_LOCKTIME_NO_RBF,
                ..Default::default()
            }],
            output: Vec::new(),
        };
        assert!(!is_final(&tx, 100, 0));
        assert!(is_final(&tx, 101, 0));
        // every input final: the lock time does not apply
        tx.input[0].sequence = bitcoin::Sequence::MAX;
        assert!(is_final(&tx, 100, 0));
    }
}
