//! The node: its connections to peers, the header sync, then each block validated with its
//! Utreexo proof (`floresta-chain`), in order, with several blocks asked for at once.
//!
//! One connection is to a Utreexo bridge (BIP 183): the headers, then the blocks with their
//! proofs, then each new block. The others are to ordinary peers: they announce new blocks with
//! their headers, which the node checks and adds to its header chain (so that a bridge that falls
//! behind or keeps a weaker chain does not go unnoticed), and they give addresses of peers. Each
//! connection runs in a slot ([`Node::run_slot`]) that connects to one address of the book after
//! another; the DNS seeds fill the book at the start ([`Node::discover`]).
//!
//! Once synced, the bridge relays unconfirmed transactions with their Utreexo proofs (BIP 183's
//! `utreexotx`) to a mempool ([`Config::mempool`]): each announced with the positions of the
//! leaves it spends, asked for with the positions of the proof hashes that prove them, checked
//! against the accumulator of the validated tip when it comes ([`crate::mempool`]). A transaction
//! whose parent is not held waits aside for it. Each transaction is asked for once: utreexod
//! 0.5.1 changes its own copy of a transaction each time it sends one.
//!
//! Everything runs in one task: the connections share the node through `&Node`.
//!
//! Scripts are not checked (`floresta-chain` without `bitcoinkernel`): a block is accepted on
//! its proof of work, its structure, its Utreexo proof, the amounts and the other rules
//! `floresta-chain` checks.

use alloc::collections::{BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::future::{pending, poll_fn};
use core::net::{IpAddr, SocketAddr};
use core::task::{Poll, Waker};

use bitcoin::hashes::Hash;
use bitcoin::{Block, BlockHash, Network, Txid};
use embassy_futures::select::{select, Either};
use embedded_io_async::{Read, Write};
use floresta_chain::extensions::BlockExt;
use floresta_chain::proof_util::process_proof;
use floresta_chain::pruned_utreexo::{IBDState, UpdatableChainstate};
use floresta_chain::{
    AssumeUtreexoValue, AssumeValidArg, BlockValidationErrors, BlockchainError,
    BlockchainInterface, ChainState, ChainStore, CompactLeafData,
};
use rustreexo::proof::Proof;
use rustreexo::stump::Stump;
use tracing::{debug, info, warn};

use crate::addrbook::{dns_seeds, AddrBook};
pub use crate::addrbook::{Outcome, Role, Source};
use crate::mempool::{proof_positions, Mempool, MempoolStats, Reject};
use crate::p2p::{
    self, inv, services, Connection, GetUtreexoProof, Inventory, Message, NetAddress, UtreexoTx,
    Version, MAX_HEADERS, PROTOCOL_VERSION,
};

/// What the node is set up with.
pub struct Config {
    pub network: Network,
    /// The user agent of our `version` message (BIP 14).
    pub user_agent: String,
    /// An accumulator to start from, taken as valid once its block is in the best header chain.
    pub assume_utreexo: Option<AssumeUtreexoValue>,
    /// Blocks asked for at once while syncing. Each is held until it is validated: up to a few
    /// megabytes on mainnet.
    pub blocks_in_flight: usize,
    /// Whether peers are reached over IPv6 too (their addresses kept).
    pub ipv6: bool,
    /// Whether the v2 transport (BIP 324) is tried with the ordinary peers that speak it.
    pub v2: bool,
    /// Whether it is tried with the bridge too, whose blocks then cost their decryption.
    pub v2_bridge: bool,
    /// The bytes the heap has left and its largest free block, if known: while syncing, a block
    /// is asked for only if they leave room for it (beyond the first block in flight, always
    /// asked for).
    pub free_memory: Option<fn() -> (usize, usize)>,
    /// The most serialized bytes of unconfirmed transactions the mempool holds; 0 for no
    /// mempool, the bridge then asked for no transactions.
    pub mempool: usize,
}

/// Validated heights between two progress lines while syncing.
const PROGRESS_EVERY: u32 = 1000;

/// The heap kept free while syncing for a block's validation and for the decoding of the block
/// arriving (its payload and the block it becomes, side by side).
const MEMORY_RESERVE: usize = 8 << 20;
/// The heap a block asked for takes once here (a mainnet block of 2 MB takes about twice that
/// decoded), counted for each block asked and not here yet.
const BLOCK_ROOM: usize = 4 << 20;
/// The largest free block kept while syncing: a block's decoding and validation allocate pieces
/// of up to about 600 KB at once (its transactions, its proof's hashes).
const CONTIGUOUS_RESERVE: usize = 2 << 20;

/// Seconds a slot waits when the book has no address for it.
const IDLE: u64 = 5;
/// Seconds a connection has to do its handshake.
const HANDSHAKE_TIMEOUT: u64 = 15;
/// A connection that ends sooner than this many seconds after it opened counts as a failure.
const SHORT_LIVED: u64 = 60;
/// Seconds between two looks at the book by [`Node::discover`].
const DISCOVER_EVERY: u64 = 600;
/// Below this many addresses of ordinary peers, [`Node::discover`] asks the DNS seeds.
const FEW_PEERS: usize = 32;
/// The most transactions asked of the bridge and not here yet.
const MAX_ASKED: usize = 5000;

/// What the node needs to wait and to tell the time.
#[allow(async_fn_in_trait)]
pub trait Clock {
    /// Monotonic seconds.
    fn now(&self) -> u64;
    /// Unix seconds: the time of day, or the best guess until it is known.
    fn unix(&self) -> i64;
    async fn sleep(&self, seconds: u64);
}

/// Opens a slot's connections.
#[allow(async_fn_in_trait)]
pub trait Dialer {
    /// A connection's byte stream.
    type Io<'a>: Read + Write
    where
        Self: 'a;
    /// Opens a TCP connection to `addr`: `None` if it fails, within a few seconds.
    async fn connect(&mut self, addr: SocketAddr) -> Option<Self::Io<'_>>;
    /// 64 random bytes, for the keys of a v2 connection.
    fn random(&mut self) -> [u8; 64];
}

/// Resolves the DNS seeds.
#[allow(async_fn_in_trait)]
pub trait Resolver {
    /// The addresses `name` resolves to, none if it does not.
    async fn resolve(&mut self, name: &str) -> Vec<IpAddr>;
}

/// Something any number of connections wait for, counted.
#[derive(Default)]
struct Event {
    count: Cell<u32>,
    wakers: RefCell<Vec<Waker>>,
}

impl Event {
    fn fire(&self) {
        self.count.set(self.count.get().wrapping_add(1));
        for waker in self.wakers.borrow_mut().drain(..) {
            waker.wake();
        }
    }

    fn count(&self) -> u32 {
        self.count.get()
    }

    /// Waits until it happens after `seen`, its count then.
    async fn after(&self, seen: u32) {
        poll_fn(|cx| {
            if self.count.get() != seen {
                return Poll::Ready(());
            }
            let mut wakers = self.wakers.borrow_mut();
            if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
                wakers.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await
    }
}

/// A flag the bridge's connection waits on, which any connection sets.
#[derive(Default)]
struct Notify {
    set: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

impl Notify {
    fn notify(&self) {
        self.set.set(true);
        if let Some(waker) = self.waker.borrow_mut().take() {
            waker.wake();
        }
    }

    fn is_set(&self) -> bool {
        self.set.get()
    }

    fn take(&self) -> bool {
        self.set.replace(false)
    }

    /// Waits until the flag is set, which it leaves set.
    async fn wait(&self) {
        poll_fn(|cx| {
            if self.set.get() {
                Poll::Ready(())
            } else {
                *self.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

/// What the node is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// No connection to a peer.
    #[default]
    Idle,
    /// Exchanging `version` and `verack`.
    Handshake,
    Headers,
    /// Taking the configured accumulator as valid.
    Assume,
    Blocks,
    /// Synced, waiting for new blocks.
    Following,
}

/// A peer's `version` message.
#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub user_agent: String,
    pub version: u32,
    pub services: u64,
    /// Its best height when it connected.
    pub height: i32,
    /// Its time when it connected, Unix seconds.
    pub time: i64,
}

/// A block validated.
#[derive(Clone, Copy, Debug)]
pub struct BlockInfo {
    pub height: u32,
    pub hash: BlockHash,
    /// Its header's time, Unix seconds.
    pub time: u32,
    pub transactions: usize,
    /// The inputs its Utreexo proof proves.
    pub inputs: usize,
    /// Its serialized size, bytes.
    pub size: usize,
}

/// A connection, open or being opened.
#[derive(Clone, Debug)]
pub struct PeerStatus {
    pub slot: usize,
    pub role: Role,
    /// Its address; `None` for a connection the node was handed without it.
    pub addr: Option<SocketAddr>,
    /// Its `version` message, once the handshake is done.
    pub info: Option<PeerInfo>,
    /// Whether the connection is in the v2 transport.
    pub v2: bool,
}

/// What the node is doing and what it did since it started, for a display.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// What the bridge's connection is doing.
    pub phase: Phase,
    /// The connections, open or being opened, by slot.
    pub peers: Vec<PeerStatus>,
    /// Handshakes done, which tells connections apart.
    pub connections: u32,
    /// The last block validated.
    pub last_block: Option<BlockInfo>,
    /// Blocks validated.
    pub validated: u32,
    /// Addresses in the book, and those of bridges among them.
    pub known: usize,
    pub known_bridges: usize,
    /// What the mempool holds.
    pub mempool: MempoolStats,
    /// Transactions the mempool took, and those it refused (invalid, or checked against another
    /// accumulator than ours, or below its lowest fee rate).
    pub taken: u32,
    pub refused: u32,
}

impl Status {
    /// The bridge's connection, if one is open or being opened.
    pub fn bridge(&self) -> Option<&PeerStatus> {
        self.peers.iter().find(|p| p.role == Role::Bridge)
    }
}

/// A block asked for, until it is validated.
struct InFlight {
    height: u32,
    hash: BlockHash,
    block: Option<Block>,
    /// The outputs the block spends and their proof: asked for once the block is here, empty at
    /// once for a block of its coinbase alone.
    proof: Option<(Vec<CompactLeafData>, Proof)>,
}

/// What ends a connection.
#[derive(Debug)]
pub enum Error<E> {
    P2p(p2p::Error<E>),
    Chain(BlockchainError),
    /// A bridge's connection to a peer that serves no Utreexo proofs of past blocks.
    NotABridge,
    /// An ordinary peer sent a header that is not valid.
    InvalidHeader,
}

impl<E> Error<E> {
    /// How the connection ended, for the address book, its handshake done.
    pub fn outcome(&self) -> Outcome {
        match self {
            Self::P2p(p2p::Error::Io(_) | p2p::Error::Closed) => Outcome::Ended,
            Self::P2p(_) | Self::InvalidHeader => Outcome::Misbehaved,
            Self::NotABridge => Outcome::WrongRole,
            // ours, or the best chain changed under the sync (another peer's headers)
            Self::Chain(BlockchainError::Database(_))
            | Self::Chain(BlockchainError::BlockValidation(
                BlockValidationErrors::BlockDoesntExtendTip,
            )) => Outcome::Ended,
            // an invalid block or proof
            Self::Chain(_) => Outcome::Misbehaved,
        }
    }
}

impl<E> From<p2p::Error<E>> for Error<E> {
    fn from(e: p2p::Error<E>) -> Self {
        Self::P2p(e)
    }
}

impl<E> From<BlockchainError> for Error<E> {
    fn from(e: BlockchainError) -> Self {
        Self::Chain(e)
    }
}

pub struct Node<S: ChainStore> {
    chain: ChainState<S>,
    config: Config,
    /// The nonce of our `version` messages, to notice a connection to ourselves.
    nonce: u64,
    /// A block was announced, by any peer: the bridge's connection syncs again.
    news: Notify,
    /// The bridge's connection ended a header sync: the ordinary peers ahead of the header chain
    /// ask for theirs.
    headers_synced: Event,
    /// The best height the bridge has, from its handshake, its headers and its announcements:
    /// no block beyond is asked of it. Unknown (no limit) until a bridge's connection sets it.
    bridge_best: Cell<u32>,
    book: RefCell<AddrBook>,
    status: RefCell<Status>,
    mempool: RefCell<Mempool>,
    /// The transactions asked of the bridge, until they come, it has them no more, or the next
    /// block.
    asked: RefCell<BTreeSet<Txid>>,
    /// The last transaction an `inv` announced, with the positions so far.
    inv_tail: RefCell<Vec<Inventory>>,
}

impl<S: ChainStore> Node<S> {
    /// Opens the chain kept in `store`, or starts one from the genesis block. `nonce` is random:
    /// it tells our `version` messages apart and drives the address book's picks.
    pub fn new(store: S, config: Config, nonce: u64) -> Result<Self, BlockchainError> {
        let chain = ChainState::open(store, config.network, AssumeValidArg::Disabled)?;
        let book = AddrBook::new(config.ipv6, nonce ^ 0x5eed);
        let mempool = Mempool::new(config.mempool, nonce ^ 0x6d70_6f6f_6c00);
        Ok(Self {
            chain,
            config,
            nonce,
            news: Notify::default(),
            headers_synced: Event::default(),
            bridge_best: Cell::new(u32::MAX),
            book: RefCell::new(book),
            status: RefCell::new(Status::default()),
            mempool: RefCell::new(mempool),
            asked: RefCell::new(BTreeSet::new()),
            inv_tail: RefCell::new(Vec::new()),
        })
    }

    pub fn chain(&self) -> &ChainState<S> {
        &self.chain
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// What the node is doing.
    pub fn status(&self) -> Status {
        let mut status = self.status.borrow().clone();
        let book = self.book.borrow();
        status.known = book.len();
        status.known_bridges = book.count(Role::Bridge);
        status.mempool = self.mempool.borrow().stats();
        status
    }

    /// The mempool.
    pub fn mempool(&self) -> core::cell::Ref<'_, Mempool> {
        self.mempool.borrow()
    }

    /// Adds a peer's address to the book: `Source::Configured` for the bridge to try first.
    pub fn add_address(&self, addr: SocketAddr, services: u64, source: Source) -> bool {
        self.book.borrow_mut().add(addr, services, source)
    }

    /// A connection of `role` to `addr` is being opened in `slot`.
    fn slot_open(&self, slot: usize, role: Role, addr: Option<SocketAddr>) {
        let mut status = self.status.borrow_mut();
        status.peers.retain(|p| p.slot != slot);
        let at = status.peers.partition_point(|p| p.slot < slot);
        status.peers.insert(
            at,
            PeerStatus {
                slot,
                role,
                addr,
                info: None,
                v2: false,
            },
        );
    }

    /// The handshake of the connection in `slot` is done, in the v2 transport or not.
    fn slot_ready(
        &self,
        slot: usize,
        role: Role,
        addr: Option<SocketAddr>,
        version: &Version,
        v2: bool,
    ) {
        if self
            .status
            .borrow()
            .peers
            .iter()
            .all(|p| p.slot != slot || p.role != role)
        {
            self.slot_open(slot, role, addr);
        }
        if let Some(addr) = addr {
            self.book.borrow_mut().handshake(addr, version.services);
        }
        let mut status = self.status.borrow_mut();
        if let Some(peer) = status.peers.iter_mut().find(|p| p.slot == slot) {
            peer.v2 = v2;
            peer.info = Some(PeerInfo {
                user_agent: version.user_agent.clone(),
                version: version.version,
                services: version.services,
                height: version.start_height,
                time: version.timestamp,
            });
        }
    }

    fn slot_ready_now(&self, slot: usize) -> bool {
        self.status
            .borrow()
            .peers
            .iter()
            .any(|p| p.slot == slot && p.info.is_some())
    }

    fn slot_closed(&self, slot: usize) {
        self.status.borrow_mut().peers.retain(|p| p.slot != slot);
    }

    fn set_phase(&self, phase: Phase) {
        self.status.borrow_mut().phase = phase;
    }

    /// Exchanges `version` and `verack` with the peer, asking for `addrv2` (BIP 155), and for no
    /// transactions. `now` is the Unix time in seconds.
    pub async fn handshake<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
        now: i64,
    ) -> Result<Version, Error<T::Error>> {
        self.handshake_with(conn, now, false).await
    }

    /// The handshake, asking for the transactions (`relay`) as a Utreexo node does: announced
    /// with the positions of the leaves they spend, relayed with their proofs (BIP 183).
    async fn handshake_with<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
        now: i64,
        relay: bool,
    ) -> Result<Version, Error<T::Error>> {
        let (height, _) = self.chain.get_best_block()?;
        conn.send(&Message::Version(Version {
            version: PROTOCOL_VERSION,
            services: if relay {
                services::WITNESS | services::UTREEXO
            } else {
                services::WITNESS
            },
            timestamp: now,
            receiver: NetAddress::default(),
            sender: NetAddress::default(),
            nonce: self.nonce,
            user_agent: self.config.user_agent.clone(),
            start_height: height as i32,
            relay,
        }))
        .await?;

        let mut theirs = None;
        let mut verack = false;
        loop {
            match next(conn).await? {
                Message::Version(version) => {
                    // before our `verack`
                    conn.send(&Message::SendAddrV2).await?;
                    conn.send(&Message::Verack).await?;
                    theirs = Some(version);
                }
                Message::Verack => verack = true,
                _ => {}
            }
            if let (Some(version), true) = (&theirs, verack) {
                debug!(
                    "peer {} version {} services {:#x} height {}",
                    version.user_agent, version.version, version.services, version.start_height
                );
                self.status.borrow_mut().connections += 1;
                return Ok(version.clone());
            }
        }
    }

    /// Downloads and checks headers until the peer has no more. Returns the best header height.
    /// The next batch is asked for as soon as a full one comes, from its last header, so that it
    /// comes while this one is checked and the connection does not idle.
    pub async fn sync_headers<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
    ) -> Result<u32, Error<T::Error>> {
        self.set_phase(Phase::Headers);
        let locator = self.chain.get_block_locator().inspect_err(|e| {
            warn!("block locator: {e:?}");
        })?;
        conn.send(&Message::GetHeaders {
            locator,
            stop: BlockHash::all_zeros(),
        })
        .await?;
        loop {
            let headers = loop {
                let message = next(conn).await?;
                if let Message::Headers(headers) = message {
                    // A header announcement (`sendheaders`) can come before the answer. One that
                    // does not connect to a header we have is skipped: the answer brings its
                    // parents, or a sync after this one does.
                    if let Some(first) = headers.first() {
                        if self.chain.get_block_header(&first.prev_blockhash).is_err() {
                            debug!(
                                "headers from {} skipped: unknown parent",
                                first.prev_blockhash
                            );
                            self.news.notify();
                            continue;
                        }
                    }
                    break headers;
                }
                self.note(&message);
            };
            let count = headers.len();
            let headers_last = headers.last().map(|h| h.block_hash());
            // a failure to ask is returned once this batch is kept
            let asked = if count == MAX_HEADERS {
                conn.send(&Message::GetHeaders {
                    locator: vec![headers[count - 1].block_hash()],
                    stop: BlockHash::all_zeros(),
                })
                .await
            } else {
                Ok(())
            };
            for header in headers {
                self.chain.accept_header(header).inspect_err(|e| {
                    warn!(
                        "header {} on {}: {e:?}",
                        header.block_hash(),
                        header.prev_blockhash
                    );
                })?;
            }
            // A reset then keeps the headers synced so far as the best chain: without it they
            // would be found beyond the saved best chain, and the whole chain reindexed.
            self.chain.flush()?;
            if let Some(last) = headers_last {
                self.bridge_has(&last);
            }
            asked?;
            let (height, hash) = self.chain.get_best_block()?;
            debug!("headers: {count} more, best {height} {hash}");
            if count < MAX_HEADERS {
                info!("headers synced: best {height} {hash}");
                self.headers_synced.fire();
                return Ok(height);
            }
        }
    }

    /// Takes the configured accumulator as valid if its block is in the best header chain and
    /// beyond the blocks validated so far. Returns whether it did.
    pub fn assume_utreexo(&self) -> Result<bool, BlockchainError> {
        let Some(assumed) = &self.config.assume_utreexo else {
            return Ok(false);
        };
        if self.chain.get_validation_index()? >= assumed.height
            || self.chain.get_block_hash(assumed.height).ok() != Some(assumed.block_hash)
        {
            return Ok(false);
        }
        let acc = Stump {
            leaves: assumed.leaves,
            roots: assumed.roots.clone(),
        };
        self.set_phase(Phase::Assume);
        self.chain.mark_chain_as_assumed(acc, assumed.block_hash)?;
        info!(
            "assumed the accumulator of block {} {}",
            assumed.height, assumed.block_hash
        );
        Ok(true)
    }

    /// Downloads and validates every block after the last validated one, up to the best header.
    /// Up to [`Config::blocks_in_flight`] blocks are asked for at once, the proof of each as it
    /// comes, and they are validated in order whatever order they come in. Returns the height
    /// of the last validated block.
    pub async fn sync_blocks<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
    ) -> Result<u32, Error<T::Error>> {
        self.set_phase(Phase::Blocks);
        let window = self.config.blocks_in_flight.max(1);
        let (best, _) = self.chain.get_best_block()?;
        // what the bridge has, which may be less than the best header
        let mut best = best.min(self.bridge_best.get());
        let mut asked = self.chain.get_validation_index()?;
        let mut in_flight: VecDeque<InFlight> = VecDeque::new();
        let mut held = false;
        loop {
            let mut inventory = Vec::new();
            while asked < best && in_flight.len() < window {
                if !in_flight.is_empty() && !self.room_for_another(&in_flight) {
                    if !held {
                        debug!("{} blocks in flight: no room for more", in_flight.len());
                    }
                    held = true;
                    break;
                }
                held = false;
                asked += 1;
                let hash = self.chain.get_block_hash(asked)?;
                inventory.push(Inventory::witness_block(hash));
                in_flight.push_back(InFlight {
                    height: asked,
                    hash,
                    block: None,
                    proof: None,
                });
            }
            if !inventory.is_empty() {
                conn.send(&Message::GetData(inventory)).await?;
            }

            let mut progress = false;
            while in_flight
                .front()
                .is_some_and(|b| b.block.is_some() && b.proof.is_some())
            {
                let Some(InFlight {
                    height,
                    hash,
                    block: Some(block),
                    proof: Some((leaf_data, proof)),
                }) = in_flight.pop_front()
                else {
                    break;
                };
                block
                    .check_block_structure()
                    .map_err(BlockchainError::from)?;
                let proof_hashes = proof.hashes.len();
                self.connect_block(height, &block, &leaf_data, proof)?;
                debug!(
                    "block {height} {hash}: {} transactions, {} inputs proven by {proof_hashes} hashes",
                    block.txdata.len(),
                    leaf_data.len()
                );
                progress = true;
                // the block's header, index and accumulator are in the store already: the best
                // chain with them, a reset resumes after this block
                self.chain.flush()?;
                if height % PROGRESS_EVERY == 0 {
                    info!("validated up to {height} of {best}");
                }
            }
            if in_flight.is_empty() && asked >= best {
                return Ok(self.chain.get_validation_index()?);
            }
            if progress {
                // ask for more before waiting
                continue;
            }

            match next(conn).await? {
                Message::Block(block) => {
                    let hash = block.block_hash();
                    let Some(slot) = in_flight
                        .iter_mut()
                        .find(|b| b.hash == hash && b.block.is_none())
                    else {
                        continue;
                    };
                    // a block of its coinbase alone spends nothing
                    if block.txdata.len() > 1 {
                        conn.send(&Message::GetUtreexoProof(GetUtreexoProof::all(hash)))
                            .await?;
                    } else {
                        slot.proof = Some((Vec::new(), Proof::default()));
                    }
                    slot.block = Some(block);
                }
                Message::UtreexoProof(uproof) => {
                    if let Some(slot) = in_flight.iter_mut().find(|b| {
                        b.hash == uproof.block_hash && b.block.is_some() && b.proof.is_none()
                    }) {
                        let proof = Proof {
                            hashes: uproof.proof_hashes,
                            targets: uproof.targets,
                        };
                        slot.proof = Some((uproof.leaf_data, proof));
                    }
                }
                Message::NotFound(list) => {
                    // the bridge does not have it yet: neither it nor those after it are asked of
                    // the bridge until it announces them
                    let missing = list
                        .iter()
                        .filter_map(Inventory::block)
                        .filter_map(|h| in_flight.iter().find(|b| b.hash == h).map(|b| b.height))
                        .min();
                    if let Some(height) = missing {
                        debug!("the bridge has no block {height} yet");
                        self.bridge_best.set(height - 1);
                        best = best.min(height - 1);
                        in_flight.retain(|b| b.height < height);
                        asked = asked.min(height - 1);
                    }
                }
                other => self.note(&other),
            }
        }
    }

    /// Whether the heap left takes one more block asked for: the reserve, and the room of each
    /// block asked and not here yet, that one included, with a free block of
    /// [`CONTIGUOUS_RESERVE`] at least. Yes when the heap left is not known.
    fn room_for_another(&self, in_flight: &VecDeque<InFlight>) -> bool {
        let Some(free_memory) = self.config.free_memory else {
            return true;
        };
        let (free, largest) = free_memory();
        let coming = in_flight.iter().filter(|b| b.block.is_none()).count() + 1;
        free >= MEMORY_RESERVE + coming * BLOCK_ROOM && largest >= CONTIGUOUS_RESERVE
    }

    /// Validates the block at `height`, the one after the last validated, with the outputs it
    /// spends (`leaf_data`) and their Utreexo proof, then makes it the validated tip. The block's
    /// structure (merkle root, witness commitment, weight) must have been checked already.
    pub fn connect_block(
        &self,
        height: u32,
        block: &Block,
        leaf_data: &[CompactLeafData],
        proof: Proof,
    ) -> Result<(), BlockchainError> {
        // `ChainState::connect_block` takes the validated tip again and adds its outputs to the
        // accumulator a second time
        let hash = block.block_hash();
        if height != self.chain.get_validation_index()? + 1
            || self.chain.get_block_hash(height)? != hash
        {
            return Err(BlockchainError::BlockValidation(
                BlockValidationErrors::BlockDoesntExtendTip,
            ));
        }
        let (del_hashes, inputs) = process_proof(
            leaf_data,
            &block.txdata,
            height,
            |h| self.chain.get_block_hash(h),
            |h| self.chain.get_mtp_by_height(h),
        )?;
        self.chain.connect_block(block, proof, inputs, del_hashes)?;
        self.mempool.borrow_mut().block_connected(block);
        // what was asked for or announced against the accumulator before this block
        self.asked.borrow_mut().clear();
        self.inv_tail.borrow_mut().clear();
        let mut status = self.status.borrow_mut();
        status.last_block = Some(BlockInfo {
            height,
            hash,
            time: block.header.time,
            transactions: block.txdata.len(),
            inputs: leaf_data.len(),
            size: block.total_size(),
        });
        status.validated += 1;
        Ok(())
    }

    /// The bridge's connection: syncs headers and blocks with the peer, then follows the new
    /// blocks, those it announces and those the other peers announce. Returns only on an error.
    /// `addr` is its address, if known, and `slot` where the status shows it.
    pub async fn run_bridge<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
        slot: usize,
        addr: Option<SocketAddr>,
        now: i64,
    ) -> Result<(), Error<T::Error>> {
        let result = self.bridge_connected(conn, slot, addr, now).await;
        self.set_phase(Phase::Idle);
        result
    }

    async fn bridge_connected<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
        slot: usize,
        addr: Option<SocketAddr>,
        now: i64,
    ) -> Result<(), Error<T::Error>> {
        self.set_phase(Phase::Handshake);
        let version = self
            .handshake_with(conn, now, self.config.mempool > 0)
            .await?;
        self.slot_ready(slot, Role::Bridge, addr, &version, conn.is_v2());
        info!(
            "bridge {}: {} height {} services {:#x}{}",
            Show(addr),
            version.user_agent,
            version.start_height,
            version.services,
            if conn.is_v2() { ", v2" } else { "" }
        );
        if version.services & services::UTREEXO_ARCHIVE == 0 {
            if !addr.is_none_or(|a| self.configured(a)) {
                return Err(Error::NotABridge);
            }
            warn!("the configured peer serves no Utreexo proofs of past blocks");
        }
        self.bridge_best.set(version.start_height.max(0) as u32);
        conn.send(&Message::Other {
            command: *b"sendheaders\0",
            payload: Vec::new(),
        })
        .await?;
        conn.send(&Message::GetAddr).await?;
        self.sync(conn).await?;
        loop {
            // blocks announced during the last sync, or since
            if !self.news.is_set() {
                match select(conn.recv(), self.news.wait()).await {
                    Either::First(message) => match message? {
                        Message::Ping(nonce) => conn.send(&Message::Pong(nonce)).await?,
                        Message::UtreexoTx(utx) => self.take_transaction(utx),
                        message => {
                            self.note(&message);
                            match &message {
                                Message::Inv(list) => self.ask_transactions(conn, list).await?,
                                Message::NotFound(list) => self.not_found(list),
                                _ => {}
                            }
                        }
                    },
                    Either::Second(()) => {}
                }
            }
            if self.news.take() {
                self.sync(conn).await?;
            }
        }
    }

    /// Asks the bridge for the transactions it announces in `list` that the mempool lacks, each
    /// with the positions of the proof hashes that prove the leaves it spends: once synced, with
    /// a mempool and the room for it. The last transaction of an `inv` waits for the next: its
    /// positions may go on there (an `inv` holds a limited number of entries). Each transaction
    /// is asked for once.
    async fn ask_transactions<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
        list: &[Inventory],
    ) -> Result<(), Error<T::Error>> {
        let mut entries = core::mem::take(&mut *self.inv_tail.borrow_mut());
        if self.config.mempool == 0
            || self.status.borrow().phase != Phase::Following
            || !self.room_for_transactions()
        {
            return Ok(());
        }
        entries.extend_from_slice(list);
        if let Some(last) = entries.iter().rposition(|e| e.tx().is_some()) {
            *self.inv_tail.borrow_mut() = entries.split_off(last);
        }
        let wanted = self.wanted(&entries, self.chain.acc().leaves);
        if !wanted.is_empty() {
            conn.send(&Message::GetData(wanted)).await?;
        }
        Ok(())
    }

    /// The `getdata` entries for the transactions announced in `list` that the mempool lacks
    /// and that were not asked for already, which it notes as asked for.
    fn wanted(&self, list: &[Inventory], leaves: u64) -> Vec<Inventory> {
        let mempool = self.mempool.borrow();
        let mut asked = self.asked.borrow_mut();
        let mut wanted = Vec::new();
        let mut i = 0;
        while i < list.len() {
            let Some(txid) = list[i].tx() else {
                i += 1;
                continue;
            };
            // the positions of the leaves it spends follow it
            let end = list[i + 1..]
                .iter()
                .position(|e| e.kind != inv::PROOF_POSITIONS)
                .map_or(list.len(), |n| i + 1 + n);
            if !mempool.contains(&txid) && asked.len() < MAX_ASKED && asked.insert(txid) {
                let packed: Vec<[u8; 32]> = list[i + 1..end].iter().map(|e| e.hash).collect();
                let targets = p2p::unpack_positions(&packed);
                wanted.push(Inventory::utreexo_tx(txid));
                let positions = proof_positions(&targets, leaves);
                wanted.extend(
                    p2p::pack_positions(&positions)
                        .into_iter()
                        .map(|hash| Inventory {
                            kind: inv::PROOF_POSITIONS,
                            hash,
                        }),
                );
            }
            i = end;
        }
        wanted
    }

    /// A transaction the bridge relays with its proof, into the mempool if it passes: one asked
    /// for only, a transaction the bridge changed on its way having another txid (utreexod
    /// 0.5.1 shifts the output indexes of its own copy each time it sends one).
    fn take_transaction(&self, utx: UtreexoTx) {
        if self.config.mempool == 0 || self.status.borrow().phase != Phase::Following {
            return;
        }
        let txid = utx.tx.compute_txid();
        if !self.asked.borrow_mut().remove(&txid) {
            debug!("transaction {txid} not asked for");
            self.status.borrow_mut().refused += 1;
            return;
        }
        let result = self.mempool.borrow_mut().admit(utx, &self.chain);
        let mut status = self.status.borrow_mut();
        match result {
            Ok(taken) => {
                status.taken += taken.len() as u32;
                debug!(
                    "transaction {txid} into the mempool{}",
                    if taken.len() > 1 {
                        ", with orphans"
                    } else {
                        ""
                    }
                );
            }
            Err(Reject::Orphan(parents)) => {
                debug!("transaction {txid} waits for {} parents", parents.len());
            }
            Err(Reject::Known) => {}
            Err(e) => {
                status.refused += 1;
                debug!("transaction {txid} refused: {e:?}");
            }
        }
    }

    /// Transactions the bridge no longer has.
    fn not_found(&self, list: &[Inventory]) {
        let mut asked = self.asked.borrow_mut();
        for txid in list.iter().filter_map(Inventory::tx) {
            debug!("the bridge has no transaction {txid}");
            asked.remove(&txid);
        }
    }

    /// Whether the heap left takes more transactions: the reserve and a block's room, with a
    /// free block of [`CONTIGUOUS_RESERVE`] at least. Yes when the heap left is not known.
    fn room_for_transactions(&self) -> bool {
        let Some(free_memory) = self.config.free_memory else {
            return true;
        };
        let (free, largest) = free_memory();
        free >= MEMORY_RESERVE + BLOCK_ROOM && largest >= CONTIGUOUS_RESERVE
    }

    /// An ordinary peer's connection: its headers checked and added to the header chain, the
    /// blocks it announces passed on to the bridge's connection, its addresses to the book.
    /// Returns only on an error.
    pub async fn run_peer<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
        slot: usize,
        addr: Option<SocketAddr>,
        now: i64,
    ) -> Result<(), Error<T::Error>> {
        let version = self.handshake(conn, now).await?;
        self.slot_ready(slot, Role::Peer, addr, &version, conn.is_v2());
        info!(
            "peer {}: {} height {} services {:#x}{}",
            Show(addr),
            version.user_agent,
            version.start_height,
            version.services,
            if conn.is_v2() { ", v2" } else { "" }
        );
        conn.send(&Message::Other {
            command: *b"sendheaders\0",
            payload: Vec::new(),
        })
        .await?;
        conn.send(&Message::GetAddr).await?;
        // ahead of the header chain while the bridge's connection synced headers: its headers
        // are asked for once that sync ends
        let mut ahead = false;
        loop {
            if ahead && self.status.borrow().phase != Phase::Headers {
                ahead = false;
                self.ask_headers(conn).await?;
            }
            let message = if ahead {
                let seen = self.headers_synced.count();
                match select(conn.recv(), self.headers_synced.after(seen)).await {
                    Either::First(message) => message?,
                    Either::Second(()) => continue,
                }
            } else {
                conn.recv().await?
            };
            match message {
                Message::Ping(nonce) => conn.send(&Message::Pong(nonce)).await?,
                Message::Headers(headers) => ahead |= self.peer_headers(conn, headers).await?,
                Message::Inv(list) => {
                    // a block announced without its header: ask for the headers
                    let unknown = list
                        .iter()
                        .filter_map(Inventory::block)
                        .any(|h| self.chain.get_block_height(&h).ok().flatten().is_none());
                    if unknown {
                        self.ask_headers(conn).await?;
                    }
                }
                message @ (Message::Addr(_) | Message::AddrV2(_)) => self.note(&message),
                _ => {}
            }
        }
    }

    /// Asks the peer for the headers after our best chain.
    async fn ask_headers<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
    ) -> Result<(), Error<T::Error>> {
        let locator = self.chain.get_block_locator()?;
        conn.send(&Message::GetHeaders {
            locator,
            stop: BlockHash::all_zeros(),
        })
        .await?;
        Ok(())
    }

    /// Headers from an ordinary peer: an announcement, or an answer to `getheaders`. Returns
    /// whether the peer is ahead of the header chain while the bridge's connection syncs headers,
    /// its headers to ask for once that sync ends.
    async fn peer_headers<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
        headers: Vec<bitcoin::block::Header>,
    ) -> Result<bool, Error<T::Error>> {
        let Some(first) = headers.first() else {
            return Ok(false);
        };
        if self.chain.get_block_header(&first.prev_blockhash).is_err() {
            // ahead of our header chain: the headers in between from this peer, once the
            // bridge's connection has synced its own
            if self.status.borrow().phase == Phase::Headers {
                return Ok(true);
            }
            self.ask_headers(conn).await?;
            return Ok(false);
        }
        let (before, _) = self.chain.get_best_block()?;
        let count = headers.len();
        let last = headers[count - 1].block_hash();
        for header in headers {
            if let Err(e) = self.chain.accept_header(header) {
                warn!("header {} from a peer: {e:?}", header.block_hash());
                return Err(match e {
                    BlockchainError::Database(_) => Error::Chain(e),
                    _ => Error::InvalidHeader,
                });
            }
        }
        self.chain.flush()?;
        if count == MAX_HEADERS {
            conn.send(&Message::GetHeaders {
                locator: vec![last],
                stop: BlockHash::all_zeros(),
            })
            .await?;
        }
        let (after, hash) = self.chain.get_best_block()?;
        if after != before {
            debug!("headers from a peer: best {after} {hash}");
            self.news.notify();
        }
        Ok(false)
    }

    /// Notes what a message of the bridge's connection tells, outside of what it is waiting for:
    /// a block announced (an `inv` of a block, or headers), addresses of peers.
    fn note(&self, message: &Message) {
        match message {
            Message::Inv(list) => {
                let mut announces = false;
                for hash in list.iter().filter_map(Inventory::block) {
                    announces = true;
                    self.bridge_has(&hash);
                }
                if announces {
                    self.news.notify();
                }
            }
            Message::Headers(headers) if !headers.is_empty() => {
                if let Some(last) = headers.last() {
                    self.bridge_has(&last.block_hash());
                }
                self.news.notify();
            }
            Message::Addr(list) | Message::AddrV2(list) => {
                let mut book = self.book.borrow_mut();
                let added = list
                    .iter()
                    .filter(|a| book.add(a.socket, a.services, Source::Peer))
                    .count();
                debug!("{} addresses, {added} new", list.len());
            }
            _ => {}
        }
    }

    /// The bridge has the block `hash`: if in our header chain, its height bounds what is asked
    /// of the bridge.
    fn bridge_has(&self, hash: &BlockHash) {
        if let Ok(Some(height)) = self.chain.get_block_height(hash) {
            if self.bridge_best.get() == u32::MAX || height > self.bridge_best.get() {
                self.bridge_best.set(height);
            }
        }
    }

    /// Whether `addr` is the configured peer.
    fn configured(&self, addr: SocketAddr) -> bool {
        self.book
            .borrow()
            .entries()
            .iter()
            .any(|e| e.addr == addr && e.source == Source::Configured)
    }

    /// Keeps a connection of `role` open in `slot`, to one address of the book after another:
    /// the bridge's ([`Node::run_bridge`]) or an ordinary peer's ([`Node::run_peer`]).
    pub async fn run_slot<D: Dialer, C: Clock>(
        &self,
        slot: usize,
        role: Role,
        dialer: &mut D,
        clock: &C,
    ) -> ! {
        let name = match role {
            Role::Bridge => "bridge",
            Role::Peer => "peer",
        };
        // the slot's receive buffer, kept from one connection to the next
        let mut buffer = Vec::new();
        loop {
            let picked = self.book.borrow_mut().pick(role, clock.now());
            let Some(addr) = picked else {
                clock.sleep(IDLE).await;
                continue;
            };
            self.slot_open(slot, role, Some(addr));
            debug!("{name} {addr}: connecting");
            let opened = clock.now();
            // in v2 with a peer that speaks it, in v1 again at once if its v2 handshake fails
            let outcome = loop {
                let v2 = self.config.v2
                    && (role == Role::Peer || self.config.v2_bridge)
                    && self.book.borrow().wants_v2(addr);
                let random = v2.then(|| dialer.random());
                let Some(io) = dialer.connect(addr).await else {
                    debug!("{name} {addr}: not reached");
                    break Outcome::Unreached;
                };
                let mut conn =
                    Connection::with_buffer(io, self.config.network, core::mem::take(&mut buffer));
                let now = clock.unix();
                let keyed = Cell::new(false);
                let run = async {
                    if let Some(random) = random {
                        conn.start_v2(bip324::Role::Initiator, random)
                            .await
                            .map_err(Error::P2p)?;
                    }
                    keyed.set(true);
                    match role {
                        Role::Bridge => self.run_bridge(&mut conn, slot, Some(addr), now).await,
                        Role::Peer => self.run_peer(&mut conn, slot, Some(addr), now).await,
                    }
                };
                let handshake = async {
                    clock.sleep(HANDSHAKE_TIMEOUT).await;
                    if self.slot_ready_now(slot) {
                        pending::<()>().await;
                    }
                };
                let result = select(run, handshake).await;
                buffer = conn.into_buffer();
                if v2 && !keyed.get() {
                    match &result {
                        Either::First(Err(e)) => debug!("{name} {addr}: no v2 ({e:?}), v1"),
                        _ => debug!("{name} {addr}: no v2 handshake in {HANDSHAKE_TIMEOUT} s, v1"),
                    }
                    self.book.borrow_mut().v1_only(addr);
                    continue;
                }
                break match result {
                    Either::First(result) => {
                        let ready = self.slot_ready_now(slot);
                        let e = match result {
                            Ok(()) => unreachable!("a connection returns only on an error"),
                            Err(e) => e,
                        };
                        let short = clock.now() - opened < SHORT_LIVED;
                        let outcome = match (ready, e.outcome()) {
                            (_, Outcome::Misbehaved) => Outcome::Misbehaved,
                            (false, _) => Outcome::Unreached,
                            // a peer that hangs up at once waits longer each time
                            (true, Outcome::Ended) if short => Outcome::Unreached,
                            (true, outcome) => outcome,
                        };
                        match outcome {
                            Outcome::Misbehaved => warn!("{name} {addr}: {e:?}, banned"),
                            Outcome::WrongRole => debug!("{name} {addr}: not a bridge"),
                            _ => info!("{name} {addr}: {e:?}"),
                        }
                        outcome
                    }
                    Either::Second(()) => {
                        debug!("{name} {addr}: no handshake in {HANDSHAKE_TIMEOUT} s");
                        Outcome::Unreached
                    }
                };
            };
            self.slot_closed(slot);
            self.book.borrow_mut().release(addr, outcome, clock.now());
            clock.sleep(1).await;
        }
    }

    /// Fills the book from the network's DNS seeds at the start, and again whenever it runs
    /// short of ordinary peers.
    pub async fn discover<R: Resolver, C: Clock>(&self, resolver: &mut R, clock: &C) -> ! {
        let port = p2p::default_port(self.config.network);
        loop {
            if self.book.borrow().count(Role::Peer) < FEW_PEERS {
                for seed in dns_seeds(self.config.network) {
                    let ips = resolver.resolve(seed.name).await;
                    let mut book = self.book.borrow_mut();
                    let added = ips
                        .iter()
                        .filter(|ip| {
                            book.add(SocketAddr::new(**ip, port), seed.services, Source::Seed)
                        })
                        .count();
                    info!(
                        "DNS seed {}: {} addresses, {added} new",
                        seed.name,
                        ips.len()
                    );
                }
            }
            clock.sleep(DISCOVER_EVERY).await;
        }
    }

    /// Headers, the assumed accumulator if any, then blocks. Returns the height of the last
    /// validated block.
    pub async fn sync<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
    ) -> Result<u32, Error<T::Error>> {
        self.sync_headers(conn).await?;
        self.assume_utreexo()?;
        self.chain.update_ibd(IBDState::ProofSync);
        let height = self.sync_blocks(conn).await?;
        self.chain.update_ibd(IBDState::Done);
        self.chain.flush()?;
        self.set_phase(Phase::Following);
        info!("validated up to {height}");
        Ok(height)
    }
}

/// An address that may be unknown, for the logs.
struct Show(Option<SocketAddr>);

impl core::fmt::Display for Show {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(addr) => write!(f, "{addr}"),
            None => f.write_str("(unknown)"),
        }
    }
}

/// The next message, answering pings on the way.
async fn next<T: Read + Write>(conn: &mut Connection<T>) -> Result<Message, p2p::Error<T::Error>> {
    loop {
        match conn.recv().await? {
            Message::Ping(nonce) => conn.send(&Message::Pong(nonce)).await?,
            message => return Ok(message),
        }
    }
}
