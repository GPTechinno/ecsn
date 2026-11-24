//! The node: handshake with a peer, header sync, then each block validated with its Utreexo
//! proof (`floresta-chain`), in order, with several blocks asked for at once.
//!
//! Scripts are not checked (`floresta-chain` without `bitcoinkernel`): a block is accepted on
//! its proof of work, its structure, its Utreexo proof, the amounts and the other rules
//! `floresta-chain` checks.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use bitcoin::hashes::Hash;
use bitcoin::{Block, BlockHash, Network};
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

use crate::p2p::{
    self, services, Connection, GetUtreexoProof, Inventory, Message, NetAddress, Version,
    MAX_HEADERS, PROTOCOL_VERSION,
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
}

/// Validated blocks between two flushes of the chain while syncing, so that a reset loses
/// little of a long sync.
const FLUSH_EVERY: u32 = 100;
/// Validated heights between two progress lines while syncing.
const PROGRESS_EVERY: u32 = 1000;

/// A block asked for, until it is validated.
struct InFlight {
    height: u32,
    hash: BlockHash,
    block: Option<Block>,
    /// The outputs the block spends and their proof: asked for once the block is here, empty at
    /// once for a block of its coinbase alone.
    proof: Option<(Vec<CompactLeafData>, Proof)>,
}

/// What can stop the node.
#[derive(Debug)]
pub enum Error<E> {
    P2p(p2p::Error<E>),
    Chain(BlockchainError),
    /// The peer does not have a block we asked for, or its proof.
    NotFound(BlockHash),
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
}

impl<S: ChainStore> Node<S> {
    /// Opens the chain kept in `store`, or starts one from the genesis block.
    pub fn new(store: S, config: Config, nonce: u64) -> Result<Self, BlockchainError> {
        let chain = ChainState::open(store, config.network, AssumeValidArg::Disabled)?;
        Ok(Self {
            chain,
            config,
            nonce,
        })
    }

    pub fn chain(&self) -> &ChainState<S> {
        &self.chain
    }

    /// Exchanges `version` and `verack` with the peer. `now` is the Unix time in seconds.
    pub async fn handshake<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
        now: i64,
    ) -> Result<Version, Error<T::Error>> {
        let (height, _) = self.chain.get_best_block()?;
        conn.send(&Message::Version(Version {
            version: PROTOCOL_VERSION,
            services: services::WITNESS,
            timestamp: now,
            receiver: NetAddress::default(),
            sender: NetAddress::default(),
            nonce: self.nonce,
            user_agent: self.config.user_agent.clone(),
            start_height: height as i32,
            // no mempool yet: no transaction announcements
            relay: false,
        }))
        .await?;

        let mut theirs = None;
        let mut verack = false;
        loop {
            match next(conn).await? {
                Message::Version(version) => {
                    conn.send(&Message::Verack).await?;
                    theirs = Some(version);
                }
                Message::Verack => verack = true,
                _ => {}
            }
            if let (Some(version), true) = (&theirs, verack) {
                info!(
                    "peer {} version {} services {:#x} height {}",
                    version.user_agent, version.version, version.services, version.start_height
                );
                if version.services & services::UTREEXO == 0 {
                    warn!("the peer serves no Utreexo proofs");
                }
                return Ok(version.clone());
            }
        }
    }

    /// Downloads and checks headers until the peer has no more. Returns the best header height.
    pub async fn sync_headers<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
    ) -> Result<u32, Error<T::Error>> {
        loop {
            let locator = self.chain.get_block_locator()?;
            conn.send(&Message::GetHeaders {
                locator,
                stop: BlockHash::all_zeros(),
            })
            .await?;
            let headers = loop {
                if let Message::Headers(headers) = next(conn).await? {
                    break headers;
                }
            };
            let count = headers.len();
            for header in headers {
                self.chain.accept_header(header)?;
            }
            let (height, hash) = self.chain.get_best_block()?;
            debug!("headers: {count} more, best {height} {hash}");
            if count < MAX_HEADERS {
                info!("headers synced: best {height} {hash}");
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
        let window = self.config.blocks_in_flight.max(1);
        let (best, _) = self.chain.get_best_block()?;
        let mut asked = self.chain.get_validation_index()?;
        let mut in_flight: VecDeque<InFlight> = VecDeque::new();
        let mut validated = 0;
        loop {
            let mut inventory = Vec::new();
            while asked < best && in_flight.len() < window {
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
                self.connect_block(height, &block, &leaf_data, proof)?;
                debug!(
                    "block {height} {hash}: {} transactions, {} inputs proven",
                    block.txdata.len(),
                    leaf_data.len()
                );
                validated += 1;
                progress = true;
                if validated % FLUSH_EVERY == 0 {
                    self.chain.flush()?;
                }
                if height % PROGRESS_EVERY == 0 {
                    info!("validated up to {height} of {best}");
                }
            }
            if in_flight.is_empty() && asked >= best {
                return Ok(best);
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
                    if let Some(hash) = list
                        .iter()
                        .filter_map(Inventory::block)
                        .find(|h| in_flight.iter().any(|b| b.hash == *h))
                    {
                        return Err(Error::NotFound(hash));
                    }
                }
                _ => {}
            }
        }
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
        if height != self.chain.get_validation_index()? + 1
            || self.chain.get_block_hash(height)? != block.block_hash()
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
        Ok(())
    }

    /// Syncs headers and blocks with the peer, then follows its new blocks. Returns only on an
    /// error.
    pub async fn run<T: Read + Write>(
        &self,
        conn: &mut Connection<T>,
        now: i64,
    ) -> Result<(), Error<T::Error>> {
        self.handshake(conn, now).await?;
        conn.send(&Message::Other {
            command: *b"sendheaders\0",
            payload: Vec::new(),
        })
        .await?;
        self.sync(conn).await?;
        loop {
            let news = match next(conn).await? {
                Message::Inv(list) => list.iter().any(|i| i.block().is_some()),
                Message::Headers(headers) => !headers.is_empty(),
                _ => false,
            };
            if news {
                self.sync(conn).await?;
            }
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
        info!("validated up to {height}");
        Ok(height)
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
