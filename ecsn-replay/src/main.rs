//! Replays a recording of `ecsn-mempool` as a mempool on the board's heap, to measure how
//! fragmented the heap gets: every allocation of this program, the mempool's transactions and
//! indexes and the blocks included, goes to a TLSF allocator (rlsf with embedded-alloc's
//! parameters) over a 32 MiB arena, as on the board. Built for `wasm32-wasip1` and run with
//! wasmtime, its pointers and `usize` are 32-bit as the board's:
//!
//! ```text
//! cargo build -p ecsn-replay --release --target wasm32-wasip1
//! wasmtime run --dir <dir> target/wasm32-wasip1/release/ecsn-replay.wasm <dir>/<recording> <cap MiB> [decoded | raw]
//! ```
//!
//! The mempool: each transaction kept until a block confirms it or conflicts with it (or a later
//! one spends the same output, replacing it), with what spends from it, kept decoded, or as its raw
//! bytes with the outputs it spends (`raw`); beyond `cap` (its
//! transactions' serialized bytes) the oldest leave first. Each block: its payload, decoded, held
//! with a proof's room while validated, then dropped. The node's other memory at boot is a block
//! of 3 MiB allocated first.
//!
//! Printed: a line every 2,000 transactions and at each block (time, transactions, their bytes,
//! the heap taken, free, its largest free block), and a summary: the heap a transaction takes
//! against its size, the smallest largest-free-block at a block's arrival, the blocks whose
//! payload would not have fitted in one free block.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::UnsafeCell;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, Read};
use std::mem::MaybeUninit;
use std::process::ExitCode;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use bitcoin::consensus::encode::deserialize;
use bitcoin::{Block, OutPoint, Transaction, Txid};
use rlsf::Tlsf;

/// The board's heap: its 32 MiB of SDRAM.
const ARENA: usize = 32 << 20;
/// The node's memory taken at boot (chain state, accumulator, buffers), before any mempool.
const BOOT: usize = 3 << 20;

type TlsfHeap = Tlsf<'static, usize, usize, { usize::BITS as usize }, { usize::BITS as usize }>;

/// The arena's memory, aligned as the allocator's blocks are, so that it all becomes the pool.
#[repr(C, align(16))]
struct Memory([MaybeUninit<u8>; ARENA]);

struct Arena {
    tlsf: UnsafeCell<TlsfHeap>,
    memory: UnsafeCell<Memory>,
    /// The bytes of the arena the pool covers, once made.
    pool_len: UnsafeCell<usize>,
    allocated: AtomicUsize,
    peak: AtomicUsize,
}

// one thread
unsafe impl Sync for Arena {}

impl Arena {
    const fn new() -> Self {
        Self {
            tlsf: UnsafeCell::new(Tlsf::new()),
            memory: UnsafeCell::new(Memory([MaybeUninit::uninit(); ARENA])),
            pool_len: UnsafeCell::new(0),
            allocated: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// The arena's first `len` bytes.
    fn region(&self, len: usize) -> NonNull<[u8]> {
        let memory = self.memory.get() as *mut u8;
        NonNull::slice_from_raw_parts(NonNull::new(memory).unwrap(), len)
    }

    /// # Safety
    /// From this thread, outside of the allocator.
    // one thread, and the allocator never calls back into itself: one `&mut` at a time
    #[allow(clippy::mut_from_ref)]
    unsafe fn tlsf(&self) -> &mut TlsfHeap {
        unsafe {
            if *self.pool_len.get() == 0 {
                let covered = (*self.tlsf.get()).insert_free_block_ptr(self.region(ARENA));
                *self.pool_len.get() = covered.map_or(0, |n| n.get());
            }
            &mut *self.tlsf.get()
        }
    }

    /// Free bytes, and the largest free block.
    fn free(&self) -> (usize, usize) {
        let mut total = 0;
        let mut largest = 0;
        // SAFETY: called between allocations, from the one thread; the blocks are walked over
        // the bytes the pool covers, its last block a sentinel
        unsafe {
            let tlsf = self.tlsf();
            let pool = self.region(*self.pool_len.get());
            for block in tlsf.iter_blocks(pool) {
                if !block.is_occupied() {
                    total += block.max_payload_size();
                    largest = largest.max(block.max_payload_size());
                }
            }
        }
        (total, largest)
    }

    fn allocated(&self) -> usize {
        self.allocated.load(Relaxed)
    }
}

unsafe impl GlobalAlloc for Arena {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match unsafe { self.tlsf() }.allocate(layout) {
            Some(ptr) => {
                let now = self.allocated.fetch_add(layout.size(), Relaxed) + layout.size();
                self.peak.fetch_max(now, Relaxed);
                ptr.as_ptr()
            }
            None => std::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            self.tlsf()
                .deallocate(NonNull::new_unchecked(ptr), layout.align())
        };
        self.allocated.fetch_sub(layout.size(), Relaxed);
    }
}

#[global_allocator]
static HEAP: Arena = Arena::new();

/// A transaction kept: decoded, or its raw bytes with the outputs it spends.
enum Kept {
    Decoded(Transaction),
    Raw {
        /// Held, as a mempool holds what it relays; not read here.
        #[allow(dead_code)]
        bytes: Vec<u8>,
        spends: Vec<OutPoint>,
    },
}

struct Entry {
    kept: Kept,
    /// Its outputs.
    outputs: u32,
    size: usize,
}

impl Entry {
    fn spends(&self) -> Vec<OutPoint> {
        match &self.kept {
            Kept::Decoded(tx) => tx.input.iter().map(|i| i.previous_output).collect(),
            Kept::Raw { spends, .. } => spends.clone(),
        }
    }
}

/// The mempool, by txid, with the output each transaction spends and its arrival order.
#[derive(Default)]
struct Pool {
    txs: HashMap<Txid, Entry>,
    spends: HashMap<OutPoint, Txid>,
    order: VecDeque<Txid>,
    bytes: usize,
}

impl Pool {
    /// Removes `txid` and what spends from it.
    fn remove(&mut self, txid: Txid) -> usize {
        let Some(entry) = self.txs.remove(&txid) else {
            return 0;
        };
        self.bytes -= entry.size;
        for spent in entry.spends() {
            if self.spends.get(&spent) == Some(&txid) {
                self.spends.remove(&spent);
            }
        }
        let mut removed = 1;
        for vout in 0..entry.outputs {
            if let Some(child) = self.spends.get(&OutPoint { txid, vout }).copied() {
                removed += self.remove(child);
            }
        }
        removed
    }

    /// Adds a transaction, replacing those that spend the same outputs, then makes room under
    /// `cap`. Returns whether it was new.
    fn add(&mut self, tx: Transaction, raw: Option<Vec<u8>>, size: usize, cap: usize) -> bool {
        let txid = tx.compute_txid();
        if self.txs.contains_key(&txid) {
            return false;
        }
        for input in &tx.input {
            if let Some(other) = self.spends.get(&input.previous_output).copied() {
                self.remove(other);
            }
        }
        for input in &tx.input {
            self.spends.insert(input.previous_output, txid);
        }
        let outputs = tx.output.len() as u32;
        let kept = match raw {
            Some(bytes) => Kept::Raw {
                bytes,
                spends: tx.input.iter().map(|i| i.previous_output).collect(),
            },
            None => Kept::Decoded(tx),
        };
        self.txs.insert(
            txid,
            Entry {
                kept,
                outputs,
                size,
            },
        );
        self.order.push_back(txid);
        self.bytes += size;
        while self.bytes > cap {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.remove(oldest);
        }
        // the queue also holds the txids of transactions gone: drop them when it doubles
        if self.order.len() > 2 * self.txs.len() + 1024 {
            let txs = &self.txs;
            self.order.retain(|t| txs.contains_key(t));
        }
        true
    }

    /// A block: its transactions leave, and those that conflict with them.
    fn confirm(&mut self, block: &Block) {
        for tx in &block.txdata {
            let txid = tx.compute_txid();
            if let Some(entry) = self.txs.remove(&txid) {
                self.bytes -= entry.size;
                for spent in entry.spends() {
                    self.spends.remove(&spent);
                }
                continue;
            }
            for input in &tx.input {
                if let Some(other) = self.spends.get(&input.previous_output).copied() {
                    self.remove(other);
                }
            }
        }
    }
}

struct Stats {
    /// The smallest largest-free-block at a block's arrival, and the block's size then.
    worst: Option<(usize, usize)>,
    /// Blocks whose payload did not fit in one free block.
    misses: usize,
    blocks: usize,
    /// The most transactions and bytes the mempool held, and the heap then taken beyond the boot.
    most: (usize, usize, usize),
}

fn line(ms: u64, what: &str, pool: &Pool) {
    let (free, largest) = HEAP.free();
    println!(
        "{:>6.1} min {:<16} {:>6} tx {:>6} kB  heap {:>6} kB  free {:>6} kB  largest {:>6} kB",
        ms as f64 / 60_000.0,
        what,
        pool.txs.len(),
        pool.bytes / 1000,
        HEAP.allocated() / 1000,
        free / 1000,
        largest / 1000
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 && args.len() != 4 {
        eprintln!("usage: ecsn-replay <recording> <mempool cap, MiB> [decoded | raw]");
        return ExitCode::FAILURE;
    }
    let cap: usize = match args[2].parse::<usize>() {
        Ok(mib) => mib << 20,
        Err(_) => {
            eprintln!("{}: not a number of MiB", args[2]);
            return ExitCode::FAILURE;
        }
    };
    let raw = match args.get(3).map(String::as_str) {
        None | Some("decoded") => false,
        Some("raw") => true,
        Some(other) => {
            eprintln!("{other}: decoded or raw");
            return ExitCode::FAILURE;
        }
    };
    let boot = vec![0u8; BOOT];
    let file = match File::open(&args[1]) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}: {e}", args[1]);
            return ExitCode::FAILURE;
        }
    };
    let mut input = BufReader::with_capacity(16 * 1024, file);
    let mut magic = [0; 8];
    if input.read_exact(&mut magic).is_err() || &magic != b"ECSNMPL1" {
        eprintln!("{}: not a recording of ecsn-mempool", args[1]);
        return ExitCode::FAILURE;
    }
    println!(
        "arena {} MiB, boot {} MiB, mempool cap {} MiB, transactions kept {}",
        ARENA >> 20,
        BOOT >> 20,
        cap >> 20,
        if raw { "raw" } else { "decoded" }
    );

    let mut pool = Pool::default();
    let mut stats = Stats {
        worst: None,
        misses: 0,
        blocks: 0,
        most: (0, 0, 0),
    };
    let mut txs = 0u64;
    let mut last_ms = 0;
    loop {
        let mut head = [0; 13];
        if input.read_exact(&mut head).is_err() {
            break;
        }
        let ms = u64::from_le_bytes(head[..8].try_into().unwrap());
        let kind = head[8];
        let len = u32::from_le_bytes(head[9..13].try_into().unwrap()) as usize;
        last_ms = ms;
        if kind == 2 {
            // a block: its payload in one piece, as received
            let (_, largest) = HEAP.free();
            stats.blocks += 1;
            if stats.worst.is_none_or(|(w, _)| largest < w) {
                stats.worst = Some((largest, len));
            }
            if largest < len {
                stats.misses += 1;
                line(ms, "BLOCK MISSED", &pool);
                // the node would make room: the oldest transactions leave until it fits
                while HEAP.free().1 < len {
                    let Some(oldest) = pool.order.pop_front() else {
                        break;
                    };
                    pool.remove(oldest);
                }
            }
            let mut payload = vec![0u8; len];
            if input.read_exact(&mut payload).is_err() {
                break;
            }
            let block: Block = match deserialize(&payload) {
                Ok(b) => b,
                Err(_) => continue,
            };
            drop(payload);
            // its proof while validated: about 8 hashes and a leaf an input
            let inputs: usize = block.txdata.iter().map(|t| t.input.len()).sum();
            let hashes = vec![[0u8; 32]; inputs * 8];
            let leaves = vec![[0u8; 16]; inputs];
            line(ms, &format!("block {} kB", len / 1000), &pool);
            pool.confirm(&block);
            drop((hashes, leaves, block));
            line(ms, "  confirmed", &pool);
        } else {
            let mut payload = vec![0u8; len];
            if input.read_exact(&mut payload).is_err() {
                break;
            }
            let tx: Transaction = match deserialize(&payload) {
                Ok(t) => t,
                Err(_) => continue,
            };
            // raw: the bytes as received, in a buffer of their size
            let raw_bytes = raw.then(|| payload[..len].to_vec());
            drop(payload);
            if pool.add(tx, raw_bytes, len, cap) {
                txs += 1;
                if pool.txs.len() > stats.most.0 {
                    stats.most = (pool.txs.len(), pool.bytes, HEAP.allocated() - BOOT);
                }
                if txs % 2000 == 0 {
                    line(ms, "", &pool);
                }
            }
        }
    }
    line(last_ms, "end", &pool);
    let (most_txs, most_bytes, most_heap) = stats.most;
    println!(
        "{} transactions, {} blocks; the mempool held {} transactions at most, {} kB, taking {} kB of heap ({:.2} bytes of heap a byte)",
        txs,
        stats.blocks,
        most_txs,
        most_bytes / 1000,
        most_heap / 1000,
        most_heap as f64 / most_bytes.max(1) as f64
    );
    if let Some((largest, size)) = stats.worst {
        println!(
            "at a block's arrival the largest free block was {} kB at the least (that block {} kB); {} of {} blocks did not fit in one",
            largest / 1000,
            size / 1000,
            stats.misses,
            stats.blocks
        );
    }
    println!(
        "heap peak {} kB of {} MiB",
        HEAP.peak.load(Relaxed) / 1000,
        ARENA >> 20
    );
    drop(boot);
    ExitCode::SUCCESS
}
