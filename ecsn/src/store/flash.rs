//! A [`ChainStore`] on NOR flash.
//!
//! NOR flash programs only clear bits, and only an erase of a whole block sets them back, so
//! everything here is written once, or appended:
//!
//! - **Header records**, 88 bytes each, appended: the header, then its height and the height's
//!   complement, written last as the record's commit mark. A record is never rewritten.
//! - **Hash table**: slots of 4 bytes, each written once, from a block hash to its record (21 bits)
//!   with 11 more bits of the hash to skip most wrong candidates. Open addressing, linear probing,
//!   at most two thirds full.
//! - **Height index**: entries of 4 bytes, each written once, from a height to the record of the
//!   best chain there. A height whose entry is already written (after a reorg) gets an override
//!   in the journal instead.
//! - **Journal**: two banks used in turn, entries appended: the best chain, index overrides,
//!   headers marked invalid or valid again. When the active bank is full, the other one is erased
//!   and starts with a snapshot of the current state.
//! - **Accumulators**: a ring of erase blocks, entries appended; the oldest block is erased when the
//!   ring wraps, so only the recent accumulators are kept (those a reorg can need).
//!
//! The state of each header (valid, in a fork...) is not written: `floresta-chain` rewrites it for
//! every header of the chain when it assumes an accumulator. It is kept in RAM, and after a reset
//! it follows from the index and the persisted best chain: a header on the index up to the
//! validated block is valid, a header on the index above it, or at a height without an entry, is
//! a header only, a header off the index is in a fork.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use bitcoin::block::Header as BlockHeader;
use bitcoin::consensus::encode::{deserialize, serialize};
use bitcoin::hashes::Hash;
use bitcoin::{BlockHash, Network};
use floresta_chain::{
    BestChain, ChainStore, ChainStoreWarning, ChainstoreError, DiskBlockHeader,
    MAX_ACCUMULATOR_SIZE,
};

use crate::flash::Flash;

const MAGIC: [u8; 8] = *b"ECSNSTOR";
const VERSION: u32 = 1;
const SUPERBLOCK_SIZE: usize = 64;

/// A header record: the header, its height, the height's complement.
pub const RECORD_SIZE: u32 = 88;
/// The height of a header stored without one.
const NO_HEIGHT: u32 = 0x7FFF_FFFF;
/// An erased slot or index entry.
const EMPTY: u32 = u32::MAX;
const RECORD_BITS: u32 = 21;
const RECORD_MASK: u32 = (1 << RECORD_BITS) - 1;
/// Record numbers fit in 21 bits, and a slot of record `RECORD_MASK` and tag `0x7FF` would read
/// as erased.
pub const MAX_RECORDS: u32 = RECORD_MASK;

const JOURNAL_MAGIC: u32 = 0x4C4E_524A; // "JRNL"
const RING_MAGIC: u32 = 0x544F_4F52; // "ROOT"
const BLOCK_HEADER_SIZE: u32 = 16;

const ENTRY_BEST: u8 = 1;
const ENTRY_INDEX: u8 = 2;
const ENTRY_INVALID: u8 = 3;
const ENTRY_VALID: u8 = 4;

// header states, in RAM
const UNKNOWN: u8 = 0;
const FULLY_VALID: u8 = 1;
const ASSUMED_VALID: u8 = 2;
const ORPHAN: u8 = 3;
const HEADERS_ONLY: u8 = 4;
const IN_FORK: u8 = 5;
const INVALID: u8 = 6;
/// A record cut by a power loss before its commit mark.
const DEAD: u8 = 7;

/// Records whose hash is cached.
const HASH_CACHE: usize = 1024;

/// Where each part of the store lies, in bytes from the start of the flash. Every part starts on
/// an erase block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub erase_size: u32,
    pub capacity: u32,
    /// Two banks of `journal_bank` bytes.
    pub journal_at: u32,
    pub journal_bank: u32,
    /// The accumulator ring, `roots_size` bytes.
    pub roots_at: u32,
    pub roots_size: u32,
    /// `slots` (a power of two) of 4 bytes.
    pub slots_at: u32,
    pub slots: u32,
    /// `records` entries of 4 bytes.
    pub index_at: u32,
    /// `records` of [`RECORD_SIZE`] bytes.
    pub records_at: u32,
    pub records: u32,
}

impl Layout {
    /// The layout for a flash of `capacity` bytes erased by blocks of `erase_size`: a journal
    /// bank of 1/1024 of it, a ring of 1/128, and as many records as fit with a hash table at
    /// most two thirds full. 128 MB give 1,353,193 records.
    pub fn new(capacity: u32, erase_size: u32) -> Option<Self> {
        let e = erase_size;
        if e < 4096 || !e.is_power_of_two() || capacity % e != 0 {
            return None;
        }
        let round_up = |x: u64| x.div_ceil(e as u64) * e as u64;
        let journal_bank = round_up((capacity / 1024).max(e) as u64) as u32;
        let roots_size = round_up((capacity / 128).max(4 * e) as u64) as u32;
        let journal_at = e;
        let roots_at = journal_at + 2 * journal_bank;
        let slots_at = roots_at + roots_size;
        // two erase blocks of margin for the rounding of the index and records
        let rest = (capacity as u64).checked_sub(slots_at as u64 + 2 * e as u64)?;

        // the smallest table for which it is at most two thirds full gives the most records
        let mut slots: u64 = 64;
        let records = loop {
            let table = round_up(slots * 4);
            if table >= rest {
                return None;
            }
            let records = ((rest - table) / (RECORD_SIZE as u64 + 4)).min(MAX_RECORDS as u64);
            if 3 * records <= 2 * slots {
                break records;
            }
            slots *= 2;
        };
        let index_at = slots_at as u64 + round_up(slots * 4);
        let records_at = index_at + round_up(records * 4);
        if records == 0 || records_at + records * RECORD_SIZE as u64 > capacity as u64 {
            return None;
        }
        Some(Self {
            erase_size: e,
            capacity,
            journal_at,
            journal_bank,
            roots_at,
            roots_size,
            slots_at,
            slots: slots as u32,
            index_at: index_at as u32,
            records_at: records_at as u32,
            records: records as u32,
        })
    }

    fn fields(&self) -> [u32; 11] {
        [
            self.erase_size,
            self.capacity,
            self.journal_at,
            self.journal_bank,
            self.roots_at,
            self.roots_size,
            self.slots_at,
            self.slots,
            self.index_at,
            self.records_at,
            self.records,
        ]
    }

    fn superblock(&self, network: Network) -> [u8; SUPERBLOCK_SIZE] {
        let mut sb = [0xFF; SUPERBLOCK_SIZE];
        sb[..8].copy_from_slice(&MAGIC);
        sb[8..12].copy_from_slice(&VERSION.to_le_bytes());
        sb[12..16].copy_from_slice(&network.magic().to_bytes());
        for (i, field) in self.fields().iter().enumerate() {
            sb[16 + 4 * i..20 + 4 * i].copy_from_slice(&field.to_le_bytes());
        }
        let crc = crc32(&[&sb[..60]]);
        sb[60..].copy_from_slice(&crc.to_le_bytes());
        sb
    }

    /// The layout a superblock describes, and the magic of its network.
    fn from_superblock(sb: &[u8; SUPERBLOCK_SIZE]) -> Option<(Self, [u8; 4])> {
        if sb[..8] != MAGIC || u32_at(sb, 8) != VERSION || u32_at(sb, 60) != crc32(&[&sb[..60]]) {
            return None;
        }
        let f = |i: usize| u32_at(sb, 16 + 4 * i);
        let layout = Self {
            erase_size: f(0),
            capacity: f(1),
            journal_at: f(2),
            journal_bank: f(3),
            roots_at: f(4),
            roots_size: f(5),
            slots_at: f(6),
            slots: f(7),
            index_at: f(8),
            records_at: f(9),
            records: f(10),
        };
        Some((layout, [sb[12], sb[13], sb[14], sb[15]]))
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// CRC-32 (IEEE) of the concatenation of `parts`.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for part in parts {
        for &byte in *part {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
            }
        }
    }
    !crc
}

fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

fn flash_error<E: core::fmt::Debug>(e: E) -> ChainstoreError {
    ChainstoreError::Other(format!("flash: {e:?}"))
}

/// Where the journal appends.
struct Journal {
    /// 0 or 1.
    bank: u32,
    seq: u32,
    /// The next entry's offset.
    at: u32,
}

/// The accumulator ring.
struct Ring {
    /// The newest block, if any.
    last: Option<u32>,
    /// Whether entries can still be appended to the newest block (none after a torn one).
    open: bool,
    /// The newest block's sequence number.
    seq: u32,
    /// The next entry's offset.
    at: u32,
    /// The offset of the latest entry of each height kept.
    entries: BTreeMap<u32, u32>,
}

/// The chain store on a NOR flash. See the module documentation for the format.
pub struct FlashChainStore<F: Flash> {
    flash: F,
    layout: Layout,
    /// Records written, including a dead one.
    records: u32,
    /// The state of each record, `UNKNOWN` until a header is saved.
    states: Vec<u8>,
    /// The height of the validated block when the store was mounted.
    validated: u32,
    overrides: BTreeMap<u32, u32>,
    invalid: BTreeSet<u32>,
    best: Option<BestChain>,
    journal: Journal,
    ring: Ring,
    hashes: RefCell<Vec<Option<(u32, BlockHash)>>>,
}

impl<F: Flash> FlashChainStore<F> {
    /// Mounts the store `flash` holds for `network`. Without one, formats the flash if `format`
    /// (erasing what it holds, which takes minutes on a large flash) and returns `true` with the
    /// store, else fails.
    pub fn mount(
        flash: F,
        network: Network,
        format: bool,
    ) -> Result<(Self, bool), ChainstoreError> {
        assert!(F::WRITE_SIZE <= 4 && 4 % F::WRITE_SIZE == 0);
        let mut sb = [0; SUPERBLOCK_SIZE];
        flash.read(0, &mut sb).map_err(flash_error)?;
        let found = Layout::from_superblock(&sb).filter(|(layout, magic)| {
            *magic == network.magic().to_bytes()
                && layout.capacity == flash.capacity()
                && layout.erase_size == F::ERASE_SIZE
        });
        match found {
            Some((layout, _)) => Ok((Self::open(flash, layout)?, false)),
            None if format => Ok((Self::format(flash, network)?, true)),
            None => Err(ChainstoreError::Other(format!(
                "no chain store of {network} on the flash"
            ))),
        }
    }

    /// Erases what the flash holds and writes an empty store.
    pub fn format(mut flash: F, network: Network) -> Result<Self, ChainstoreError> {
        let layout = Layout::new(flash.capacity(), F::ERASE_SIZE)
            .ok_or_else(|| ChainstoreError::Other("flash too small".into()))?;
        let e = layout.erase_size;
        // the superblock first, so that an interrupted format is not taken for a store
        flash.erase(0, e).map_err(flash_error)?;
        // a sector of the board's two chips side by side
        let chunk = e.max(128 * 1024);
        let mut buf = vec![0; chunk as usize];
        let mut at = e;
        while at < layout.capacity {
            let len = chunk.min(layout.capacity - at);
            let buf = &mut buf[..len as usize];
            flash.read(at, buf).map_err(flash_error)?;
            if buf.iter().any(|&b| b != 0xFF) {
                flash.erase(at, at + len).map_err(flash_error)?;
            }
            at += len;
        }
        flash
            .write(layout.journal_at, &block_header(JOURNAL_MAGIC, 1))
            .map_err(flash_error)?;
        flash
            .write(0, &layout.superblock(network))
            .map_err(flash_error)?;
        Self::open(flash, layout)
    }

    fn open(flash: F, layout: Layout) -> Result<Self, ChainstoreError> {
        let mut store = Self {
            flash,
            layout,
            records: 0,
            states: Vec::new(),
            validated: 0,
            overrides: BTreeMap::new(),
            invalid: BTreeSet::new(),
            best: None,
            journal: Journal {
                bank: 0,
                seq: 0,
                at: 0,
            },
            ring: Ring {
                last: None,
                open: false,
                seq: 0,
                at: 0,
                entries: BTreeMap::new(),
            },
            hashes: RefCell::new(vec![None; HASH_CACHE]),
        };
        store.mount_records()?;
        store.mount_journal()?;
        store.mount_ring()?;
        if let Some(best) = &store.best {
            if let Some(r) = store.find(&best.validation_index)? {
                store.validated = store.height_of(r)?.unwrap_or(0);
            }
        }
        Ok(store)
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Header records written.
    pub fn records(&self) -> u32 {
        self.records
    }

    /// Heights whose accumulator is kept.
    pub fn roots_kept(&self) -> usize {
        self.ring.entries.len()
    }

    pub fn into_flash(self) -> F {
        self.flash
    }

    fn read(&self, at: u32, buf: &mut [u8]) -> Result<(), ChainstoreError> {
        self.flash.read(at, buf).map_err(flash_error)
    }

    fn read_u32(&self, at: u32) -> Result<u32, ChainstoreError> {
        let mut buf = [0; 4];
        self.read(at, &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    fn write(&mut self, at: u32, data: &[u8]) -> Result<(), ChainstoreError> {
        self.flash.write(at, data).map_err(flash_error)
    }

    fn erase(&mut self, from: u32, to: u32) -> Result<(), ChainstoreError> {
        self.flash.erase(from, to).map_err(flash_error)
    }

    // records

    fn record_at(&self, r: u32) -> u32 {
        self.layout.records_at + r * RECORD_SIZE
    }

    /// Finds the records written (they are written in order), and makes the last one consistent:
    /// dead if its commit mark is missing, else in the hash table.
    fn mount_records(&mut self) -> Result<(), ChainstoreError> {
        let blank = |store: &Self, r: u32| -> Result<bool, ChainstoreError> {
            let mut buf = [0; RECORD_SIZE as usize];
            store.read(store.record_at(r), &mut buf)?;
            Ok(buf.iter().all(|&b| b == 0xFF))
        };
        let (mut lo, mut hi) = (0, self.layout.records);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if blank(self, mid)? {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        self.records = lo;
        self.states = vec![UNKNOWN; lo as usize];
        if let Some(last) = lo.checked_sub(1) {
            if self.committed_height(last)?.is_none() {
                self.states[last as usize] = DEAD;
            } else {
                let hash = self.record_hash(last)?;
                if self.find(&hash)? != Some(last) {
                    self.insert_slot(&hash, last)?;
                }
            }
        }
        Ok(())
    }

    fn read_header(&self, r: u32) -> Result<BlockHeader, ChainstoreError> {
        let mut buf = [0; 80];
        self.read(self.record_at(r), &mut buf)?;
        deserialize(&buf).map_err(|_| ChainstoreError::CorruptedDatabase)
    }

    /// The record's height field, if its commit mark is whole.
    fn committed_height(&self, r: u32) -> Result<Option<u32>, ChainstoreError> {
        let mut buf = [0; 8];
        self.read(self.record_at(r) + 80, &mut buf)?;
        let height = u32_at(&buf, 0);
        Ok((height ^ u32_at(&buf, 4) == u32::MAX).then_some(height))
    }

    /// The record's height, `None` for a header stored without one.
    fn height_of(&self, r: u32) -> Result<Option<u32>, ChainstoreError> {
        let height = self
            .committed_height(r)?
            .ok_or(ChainstoreError::CorruptedDatabase)?;
        Ok((height != NO_HEIGHT).then_some(height))
    }

    fn record_hash(&self, r: u32) -> Result<BlockHash, ChainstoreError> {
        let slot = r as usize % HASH_CACHE;
        if let Some((cached, hash)) = self.hashes.borrow()[slot] {
            if cached == r {
                return Ok(hash);
            }
        }
        let hash = self.read_header(r)?.block_hash();
        self.hashes.borrow_mut()[slot] = Some((r, hash));
        Ok(hash)
    }

    fn append(
        &mut self,
        header: &BlockHeader,
        height: Option<u32>,
        hash: &BlockHash,
    ) -> Result<u32, ChainstoreError> {
        let r = self.records;
        if r >= self.layout.records {
            return Err(ChainstoreError::FullIndex);
        }
        let at = self.record_at(r);
        self.write(at, &serialize(header))?;
        let height = height.unwrap_or(NO_HEIGHT);
        let mut mark = [0; 8];
        mark[..4].copy_from_slice(&height.to_le_bytes());
        mark[4..].copy_from_slice(&(!height).to_le_bytes());
        self.write(at + 80, &mark)?;
        self.records += 1;
        self.states.push(UNKNOWN);
        self.hashes.borrow_mut()[r as usize % HASH_CACHE] = Some((r, *hash));
        self.insert_slot(hash, r)?;
        Ok(r)
    }

    fn state(&self, r: u32) -> Result<u8, ChainstoreError> {
        let state = self.states[r as usize];
        if state != UNKNOWN {
            return Ok(state);
        }
        if self.invalid.contains(&r) {
            return Ok(INVALID);
        }
        let Some(height) = self.height_of(r)? else {
            return Ok(ORPHAN);
        };
        Ok(match self.index_entry(height)? {
            Some(x) if x == r && height <= self.validated => FULLY_VALID,
            Some(x) if x == r => HEADERS_ONLY,
            None => HEADERS_ONLY,
            Some(_) => IN_FORK,
        })
    }

    fn disk_header(&self, r: u32) -> Result<DiskBlockHeader, ChainstoreError> {
        let header = self.read_header(r)?;
        let height = || self.height_of(r)?.ok_or(ChainstoreError::CorruptedDatabase);
        Ok(match self.state(r)? {
            FULLY_VALID => DiskBlockHeader::FullyValid(header, height()?),
            ASSUMED_VALID => DiskBlockHeader::AssumedValid(header, height()?),
            ORPHAN => DiskBlockHeader::Orphan(header),
            HEADERS_ONLY => DiskBlockHeader::HeadersOnly(header, height()?),
            IN_FORK => DiskBlockHeader::InFork(header, height()?),
            INVALID => DiskBlockHeader::InvalidChain(header),
            _ => return Err(ChainstoreError::CorruptedDatabase),
        })
    }

    // hash table

    fn slot_at(&self, i: u32) -> u32 {
        self.layout.slots_at + 4 * i
    }

    fn bucket_and_tag(hash: &BlockHash) -> (u32, u32) {
        let bytes = hash.as_byte_array();
        let bucket = u32_at(bytes, 0);
        let tag = u16::from_le_bytes([bytes[4], bytes[5]]) as u32 & (u32::MAX >> RECORD_BITS);
        (bucket, tag)
    }

    fn find(&self, hash: &BlockHash) -> Result<Option<u32>, ChainstoreError> {
        let (bucket, tag) = Self::bucket_and_tag(hash);
        let mask = self.layout.slots - 1;
        let mut i = bucket & mask;
        for _ in 0..self.layout.slots {
            let slot = self.read_u32(self.slot_at(i))?;
            if slot == EMPTY {
                return Ok(None);
            }
            let r = slot & RECORD_MASK;
            if slot >> RECORD_BITS == tag
                && r < self.records
                && self.states[r as usize] != DEAD
                && self.record_hash(r)? == *hash
            {
                return Ok(Some(r));
            }
            i = (i + 1) & mask;
        }
        Ok(None)
    }

    fn insert_slot(&mut self, hash: &BlockHash, r: u32) -> Result<(), ChainstoreError> {
        let (bucket, tag) = Self::bucket_and_tag(hash);
        let mask = self.layout.slots - 1;
        let mut i = bucket & mask;
        for _ in 0..self.layout.slots {
            let at = self.slot_at(i);
            if self.read_u32(at)? == EMPTY {
                return self.write(at, &(tag << RECORD_BITS | r).to_le_bytes());
            }
            i = (i + 1) & mask;
        }
        Err(ChainstoreError::FullIndex)
    }

    // height index

    /// The record of the best chain at `height`. An entry pointing to a record of another height
    /// (one cut by a power loss) counts as none.
    fn index_entry(&self, height: u32) -> Result<Option<u32>, ChainstoreError> {
        if height >= self.layout.records {
            return Ok(None);
        }
        let r = match self.overrides.get(&height) {
            Some(&r) => r,
            None => self.read_u32(self.layout.index_at + 4 * height)?,
        };
        if r >= self.records || self.states[r as usize] == DEAD {
            return Ok(None);
        }
        Ok((self.committed_height(r)? == Some(height)).then_some(r))
    }

    // journal

    fn bank_at(&self, bank: u32) -> u32 {
        self.layout.journal_at + bank * self.layout.journal_bank
    }

    fn mount_journal(&mut self) -> Result<(), ChainstoreError> {
        let mut active = None;
        for bank in 0..2 {
            if let Some(seq) = self.read_block_header(self.bank_at(bank), JOURNAL_MAGIC)? {
                if active.is_none_or(|(_, s)| seq > s) {
                    active = Some((bank, seq));
                }
            }
        }
        let (bank, seq) = active.ok_or(ChainstoreError::CorruptedDatabase)?;
        let start = self.bank_at(bank);
        let end = start + self.layout.journal_bank;
        let mut at = start + BLOCK_HEADER_SIZE;
        let mut torn = false;
        while at + 4 <= end {
            let head = self.read_u32(at)?;
            if head == EMPTY {
                break;
            }
            let len = (head & 0xFFFF) as usize;
            let kind = (head >> 16) as u8;
            let size = 4 + pad4(len) as u32 + 4;
            if head >> 24 != 0 || at + size > end {
                torn = true;
                break;
            }
            let mut payload = vec![0; len];
            self.read(at + 4, &mut payload)?;
            let crc = self.read_u32(at + 4 + pad4(len) as u32)?;
            if crc != crc32(&[&head.to_le_bytes(), &payload]) || !self.apply(kind, &payload) {
                torn = true;
                break;
            }
            at += size;
        }
        self.journal = Journal { bank, seq, at };
        if torn {
            // later entries would be lost behind the torn one
            self.compact()?;
        }
        Ok(())
    }

    fn apply(&mut self, kind: u8, payload: &[u8]) -> bool {
        let word = |i: usize| {
            payload
                .get(4 * i..4 * i + 4)
                .map(|_| u32_at(payload, 4 * i))
        };
        match (kind, word(0), word(1)) {
            (ENTRY_BEST, _, _) => match deserialize(payload) {
                Ok(best) => self.best = Some(best),
                Err(_) => return false,
            },
            (ENTRY_INDEX, Some(height), Some(r)) => {
                self.overrides.insert(height, r);
            }
            (ENTRY_INVALID, Some(r), _) => {
                self.invalid.insert(r);
            }
            (ENTRY_VALID, Some(r), _) => {
                self.invalid.remove(&r);
            }
            _ => return false,
        }
        true
    }

    fn entry(kind: u8, payload: &[u8]) -> Vec<u8> {
        let head = payload.len() as u32 | (kind as u32) << 16;
        let mut entry = Vec::with_capacity(8 + pad4(payload.len()));
        entry.extend_from_slice(&head.to_le_bytes());
        entry.extend_from_slice(payload);
        entry.resize(4 + pad4(payload.len()), 0xFF);
        entry.extend_from_slice(&crc32(&[&head.to_le_bytes(), payload]).to_le_bytes());
        entry
    }

    fn journal_append(&mut self, kind: u8, payload: &[u8]) -> Result<(), ChainstoreError> {
        let entry = Self::entry(kind, payload);
        let end = self.bank_at(self.journal.bank) + self.layout.journal_bank;
        if self.journal.at + entry.len() as u32 > end {
            self.compact()?;
            let end = self.bank_at(self.journal.bank) + self.layout.journal_bank;
            if self.journal.at + entry.len() as u32 > end {
                return Err(ChainstoreError::FullIndex);
            }
        }
        self.write(self.journal.at, &entry)?;
        self.journal.at += entry.len() as u32;
        Ok(())
    }

    /// Writes the current state at the start of the other bank, then makes it the active one.
    fn compact(&mut self) -> Result<(), ChainstoreError> {
        let bank = 1 - self.journal.bank;
        let start = self.bank_at(bank);
        let end = start + self.layout.journal_bank;
        self.erase(start, end)?;
        let mut entries = Vec::new();
        if let Some(best) = &self.best {
            entries.push(Self::entry(ENTRY_BEST, &serialize(best)));
        }
        for (&height, &r) in &self.overrides {
            let mut payload = [0; 8];
            payload[..4].copy_from_slice(&height.to_le_bytes());
            payload[4..].copy_from_slice(&r.to_le_bytes());
            entries.push(Self::entry(ENTRY_INDEX, &payload));
        }
        for &r in &self.invalid {
            entries.push(Self::entry(ENTRY_INVALID, &r.to_le_bytes()));
        }
        let mut at = start + BLOCK_HEADER_SIZE;
        for entry in entries {
            if at + entry.len() as u32 > end {
                return Err(ChainstoreError::FullIndex);
            }
            self.write(at, &entry)?;
            at += entry.len() as u32;
        }
        // the bank header last: until it is written, the other bank stays the active one
        let seq = self.journal.seq + 1;
        self.write(start, &block_header(JOURNAL_MAGIC, seq))?;
        self.journal = Journal { bank, seq, at };
        Ok(())
    }

    /// The sequence number of a journal bank or ring block starting at `at`, if it has a valid
    /// header.
    fn read_block_header(&self, at: u32, magic: u32) -> Result<Option<u32>, ChainstoreError> {
        let mut buf = [0; BLOCK_HEADER_SIZE as usize];
        self.read(at, &mut buf)?;
        let seq = u32_at(&buf, 4);
        Ok((u32_at(&buf, 0) == magic && seq ^ u32_at(&buf, 8) == u32::MAX).then_some(seq))
    }

    // accumulator ring

    fn ring_blocks(&self) -> u32 {
        self.layout.roots_size / self.layout.erase_size
    }

    fn ring_block_at(&self, block: u32) -> u32 {
        self.layout.roots_at + block * self.layout.erase_size
    }

    fn mount_ring(&mut self) -> Result<(), ChainstoreError> {
        let mut blocks = Vec::new();
        for block in 0..self.ring_blocks() {
            if let Some(seq) = self.read_block_header(self.ring_block_at(block), RING_MAGIC)? {
                blocks.push((seq, block));
            }
        }
        blocks.sort_unstable();
        for &(seq, block) in &blocks {
            let start = self.ring_block_at(block);
            let end = start + self.layout.erase_size;
            let mut at = start + BLOCK_HEADER_SIZE;
            let mut torn = false;
            while at + 8 <= end {
                let height = self.read_u32(at)?;
                if height == EMPTY {
                    break;
                }
                match self.read_roots(at, height)? {
                    Some(roots) => {
                        self.ring.entries.insert(height, at);
                        at += 12 + pad4(roots.len()) as u32;
                    }
                    None => {
                        torn = true;
                        break;
                    }
                }
            }
            self.ring.last = Some(block);
            self.ring.seq = seq;
            // after a torn entry, the next one goes to a new block
            self.ring.open = !torn;
            self.ring.at = at;
        }
        Ok(())
    }

    /// The accumulator of the ring entry at `at`, if it is whole and of `height`.
    fn read_roots(&self, at: u32, height: u32) -> Result<Option<Vec<u8>>, ChainstoreError> {
        let end = (at / self.layout.erase_size + 1) * self.layout.erase_size;
        let mut head = [0; 8];
        self.read(at, &mut head)?;
        let len = u32_at(&head, 4) as usize;
        if u32_at(&head, 0) != height
            || len > MAX_ACCUMULATOR_SIZE
            || at + 12 + pad4(len) as u32 > end
        {
            return Ok(None);
        }
        let mut roots = vec![0; len];
        self.read(at + 8, &mut roots)?;
        let crc = self.read_u32(at + 8 + pad4(len) as u32)?;
        Ok((crc == crc32(&[&head, &roots])).then_some(roots))
    }

    fn ring_append(&mut self, height: u32, roots: &[u8]) -> Result<(), ChainstoreError> {
        let size = 12 + pad4(roots.len()) as u32;
        let fits = match self.ring.last {
            Some(block) if self.ring.open => {
                self.ring.at + size <= self.ring_block_at(block) + self.layout.erase_size
            }
            _ => false,
        };
        if !fits {
            // the next block, the oldest one, is erased and its accumulators dropped
            let block = self.ring.last.map_or(0, |b| (b + 1) % self.ring_blocks());
            let start = self.ring_block_at(block);
            let end = start + self.layout.erase_size;
            self.erase(start, end)?;
            self.ring
                .entries
                .retain(|_, &mut at| !(start..end).contains(&at));
            let seq = self.ring.seq + 1;
            self.write(start, &block_header(RING_MAGIC, seq))?;
            self.ring.last = Some(block);
            self.ring.open = true;
            self.ring.seq = seq;
            self.ring.at = start + BLOCK_HEADER_SIZE;
        }
        let mut entry = Vec::with_capacity(size as usize);
        entry.extend_from_slice(&height.to_le_bytes());
        entry.extend_from_slice(&(roots.len() as u32).to_le_bytes());
        let crc = crc32(&[&entry, roots]);
        entry.extend_from_slice(roots);
        entry.resize(8 + pad4(roots.len()), 0xFF);
        entry.extend_from_slice(&crc.to_le_bytes());
        let at = self.ring.at;
        self.write(at, &entry)?;
        self.ring.at += size;
        self.ring.entries.insert(height, at);
        Ok(())
    }
}

/// The header of a journal bank or ring block: magic, sequence number, its complement.
fn block_header(magic: u32, seq: u32) -> [u8; BLOCK_HEADER_SIZE as usize] {
    let mut header = [0xFF; BLOCK_HEADER_SIZE as usize];
    header[..4].copy_from_slice(&magic.to_le_bytes());
    header[4..8].copy_from_slice(&seq.to_le_bytes());
    header[8..12].copy_from_slice(&(!seq).to_le_bytes());
    header
}

impl<F: Flash> ChainStore for FlashChainStore<F> {
    type Error = ChainstoreError;

    fn save_roots_for_block(&mut self, roots: Vec<u8>, height: u32) -> Result<(), Self::Error> {
        if roots.len() > MAX_ACCUMULATOR_SIZE {
            return Err(ChainstoreError::OversizedAccumulator);
        }
        self.ring_append(height, &roots)
    }

    fn load_roots_for_block(&mut self, height: u32) -> Result<Option<Vec<u8>>, Self::Error> {
        match self.ring.entries.get(&height) {
            Some(&at) => self.read_roots(at, height),
            None => Ok(None),
        }
    }

    fn load_height(&self) -> Result<Option<BestChain>, Self::Error> {
        Ok(self.best.clone())
    }

    fn save_height(&mut self, height: &BestChain) -> Result<(), Self::Error> {
        if self.best.as_ref() == Some(height) {
            return Ok(());
        }
        self.journal_append(ENTRY_BEST, &serialize(height))?;
        self.best = Some(height.clone());
        Ok(())
    }

    fn get_header(&self, block_hash: &BlockHash) -> Result<Option<DiskBlockHeader>, Self::Error> {
        match self.find(block_hash)? {
            Some(r) => Ok(Some(self.disk_header(r)?)),
            None => Ok(None),
        }
    }

    fn get_header_by_height(&self, height: u32) -> Result<Option<DiskBlockHeader>, Self::Error> {
        match self.index_entry(height)? {
            Some(r) => Ok(Some(self.disk_header(r)?)),
            None => Ok(None),
        }
    }

    fn save_header(&mut self, header: &DiskBlockHeader) -> Result<(), Self::Error> {
        let (state, height) = match *header {
            DiskBlockHeader::FullyValid(_, h) => (FULLY_VALID, Some(h)),
            DiskBlockHeader::AssumedValid(_, h) => (ASSUMED_VALID, Some(h)),
            DiskBlockHeader::Orphan(_) => (ORPHAN, None),
            DiskBlockHeader::HeadersOnly(_, h) => (HEADERS_ONLY, Some(h)),
            DiskBlockHeader::InFork(_, h) => (IN_FORK, Some(h)),
            DiskBlockHeader::InvalidChain(_) => (INVALID, None),
        };
        let hash = header.block_hash();
        let r = match self.find(&hash)? {
            Some(r) => {
                if height.is_some() && self.height_of(r)? != height {
                    return Err(ChainstoreError::Other(format!(
                        "header {hash} saved at another height than stored"
                    )));
                }
                r
            }
            None => self.append(header, height, &hash)?,
        };
        let invalid = self.invalid.contains(&r);
        if state == INVALID && !invalid {
            self.journal_append(ENTRY_INVALID, &r.to_le_bytes())?;
            self.invalid.insert(r);
        } else if state != INVALID && invalid {
            self.journal_append(ENTRY_VALID, &r.to_le_bytes())?;
            self.invalid.remove(&r);
        }
        self.states[r as usize] = state;
        Ok(())
    }

    fn get_block_hash(&self, height: u32) -> Result<Option<BlockHash>, Self::Error> {
        match self.index_entry(height)? {
            Some(r) => Ok(Some(self.record_hash(r)?)),
            None => Ok(None),
        }
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        // every write goes to the flash at once
        Ok(())
    }

    fn update_block_index(&mut self, height: u32, hash: BlockHash) -> Result<(), Self::Error> {
        if height >= self.layout.records {
            return Err(ChainstoreError::FullIndex);
        }
        let r = self.find(&hash)?.ok_or(ChainstoreError::HeaderNotFound)?;
        let at = self.layout.index_at + 4 * height;
        let written = self.read_u32(at)?;
        match self.overrides.get(&height) {
            Some(&o) if o == r => return Ok(()),
            Some(_) => {}
            None if written == r => return Ok(()),
            None if written == EMPTY => return self.write(at, &r.to_le_bytes()),
            None => {}
        }
        let mut payload = [0; 8];
        payload[..4].copy_from_slice(&height.to_le_bytes());
        payload[4..].copy_from_slice(&r.to_le_bytes());
        self.journal_append(ENTRY_INDEX, &payload)?;
        self.overrides.insert(height, r);
        Ok(())
    }

    fn check_integrity(&self) -> Result<(), Self::Error> {
        // checked when mounted
        Ok(())
    }

    fn size_on_disk(&self) -> Result<u64, Self::Error> {
        let fixed = self.layout.journal_bank as u64 * 2 + self.layout.roots_size as u64;
        Ok(fixed + self.records as u64 * (RECORD_SIZE as u64 + 4) + self.layout.slots as u64 * 4)
    }

    fn get_warnings(&self) -> Vec<ChainStoreWarning> {
        if self.records as u64 * 20 >= self.layout.records as u64 * 19 {
            vec![ChainStoreWarning::HeaderStorageFull]
        } else {
            Vec::new()
        }
    }
}
