//! The Bitcoin P2P messages the node uses (protocol version 70016, BIP 144 witness blocks), with
//! BIP 183's Utreexo proof messages and BIP 155's `addrv2`, and their framing over a byte stream:
//! the v1 transport (magic, command, length, checksum), or BIP 324's v2 (a key exchange, then
//! encrypted packets, the commonest commands as one byte).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use bip324::{
    FillBytes, GarbageResult, Handshake, InboundCipher, Initialized, OutboundCipher, PacketType,
    ReceivedKey, Role, VersionResult,
};
use bitcoin::block::Header;
use bitcoin::consensus::encode::{self, serialize, Decodable, Encodable, VarInt};
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::{Block, BlockHash, Network};
use embedded_io_async::{Read, Write};
use floresta_chain::{CompactLeafData, ScriptPubKeyKind};
use rustreexo::node_hash::BitcoinNodeHash;

/// The protocol version we speak: BIP 339's `wtxidrelay` era, the newest without a negotiation
/// of its own.
pub const PROTOCOL_VERSION: u32 = 70016;

/// The largest payload a message may carry.
pub const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

/// The most headers one `headers` message carries.
pub const MAX_HEADERS: usize = 2000;

/// The most entries of an `inv`, `getdata` or `notfound`.
const MAX_INV: usize = 50_000;

/// The most hashes of a block locator.
const MAX_LOCATOR: usize = 101;

/// The longest user agent we take.
const MAX_USER_AGENT: usize = 256;

/// The most addresses of an `addr` or `addrv2`.
pub const MAX_ADDR: usize = 1000;

/// The longest address of an `addrv2` entry (BIP 155).
const MAX_ADDRV2_LEN: usize = 512;

/// The most inputs a block can have.
const MAX_INPUTS_PER_BLOCK: usize = 24_386;

/// The most hashes a Utreexo proof can need: one proof per input, none shared, in a forest of
/// 64 rows.
const MAX_PROOF_HASHES: usize = MAX_INPUTS_PER_BLOCK * 64;

/// Service bits of the `version` message.
pub mod services {
    /// Serves the whole block chain.
    pub const NETWORK: u64 = 1 << 0;
    /// Serves witness data (BIP 144).
    pub const WITNESS: u64 = 1 << 3;
    /// Serves the last 288 blocks (BIP 159).
    pub const NETWORK_LIMITED: u64 = 1 << 10;
    /// Speaks the v2 transport (BIP 324).
    pub const P2P_V2: u64 = 1 << 11;
    /// Serves Utreexo proofs of new blocks and transactions (BIP 183).
    pub const UTREEXO: u64 = 1 << 12;
    /// Serves Utreexo proofs of every past block (BIP 183).
    pub const UTREEXO_ARCHIVE: u64 = 1 << 13;
}

/// Inventory types.
pub mod inv {
    /// A transaction.
    pub const TX: u32 = 1;
    /// A block.
    pub const BLOCK: u32 = 2;
    /// Asks for the witness serialization (BIP 144).
    pub const WITNESS_FLAG: u32 = 1 << 30;
    /// Asks for the Utreexo data with it: a transaction as a `utreexotx` (BIP 183).
    pub const UTREEXO_FLAG: u32 = 1 << 24;
    /// Four positions in the accumulator packed in the hash, after a transaction's entry: in an
    /// `inv`, of the leaves it spends; in a `getdata`, of the proof hashes asked for.
    pub const PROOF_POSITIONS: u32 = 6;
}

/// Packs accumulator positions four to a hash, little-endian, the last hash padded with
/// `u64::MAX` (the entries of kind [`inv::PROOF_POSITIONS`]).
pub fn pack_positions(positions: &[u64]) -> Vec<[u8; 32]> {
    positions
        .chunks(4)
        .map(|chunk| {
            let mut hash = [0xff; 32];
            for (i, p) in chunk.iter().enumerate() {
                hash[8 * i..8 * i + 8].copy_from_slice(&p.to_le_bytes());
            }
            hash
        })
        .collect()
}

/// The positions packed by [`pack_positions`].
pub fn unpack_positions(hashes: &[[u8; 32]]) -> Vec<u64> {
    let mut positions = Vec::with_capacity(hashes.len() * 4);
    for hash in hashes {
        for i in 0..4 {
            let mut bytes = [0; 8];
            bytes.copy_from_slice(&hash[8 * i..8 * i + 8]);
            let p = u64::from_le_bytes(bytes);
            if p == u64::MAX {
                break;
            }
            positions.push(p);
        }
    }
    positions
}

/// An entry of `inv`, `getdata` and `notfound`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inventory {
    pub kind: u32,
    pub hash: [u8; 32],
}

impl Inventory {
    /// A transaction with its witnesses and its Utreexo data (a `utreexotx`).
    pub fn utreexo_tx(txid: bitcoin::Txid) -> Self {
        Self {
            kind: inv::TX | inv::WITNESS_FLAG | inv::UTREEXO_FLAG,
            hash: txid.to_byte_array(),
        }
    }

    /// The txid of the transaction this entry names, if it names one.
    pub fn tx(&self) -> Option<bitcoin::Txid> {
        (self.kind & !(inv::WITNESS_FLAG | inv::UTREEXO_FLAG) == inv::TX)
            .then(|| bitcoin::Txid::from_byte_array(self.hash))
    }

    /// A block, with its witnesses.
    pub fn witness_block(hash: BlockHash) -> Self {
        Self {
            kind: inv::BLOCK | inv::WITNESS_FLAG,
            hash: hash.to_byte_array(),
        }
    }

    /// The hash of the block this entry names, if it names one.
    pub fn block(&self) -> Option<BlockHash> {
        (self.kind & !inv::WITNESS_FLAG == inv::BLOCK)
            .then(|| BlockHash::from_byte_array(self.hash))
    }
}

/// A peer's address as the `version` message carries it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetAddress {
    pub services: u64,
    /// IPv6, or IPv4 mapped into IPv6.
    pub ip: [u8; 16],
    pub port: u16,
}

/// A peer's address as `addr` and `addrv2` carry it, IPv4 or IPv6: the networks the node can
/// reach (entries of Tor, I2P or CJDNS are dropped when decoded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    /// When it was last seen, Unix seconds.
    pub time: u32,
    pub services: u64,
    pub socket: SocketAddr,
}

/// BIP 155's network IDs.
mod net_id {
    pub const IPV4: u8 = 1;
    pub const IPV6: u8 = 2;
}

/// The default P2P port of `network`.
pub fn default_port(network: Network) -> u16 {
    match network {
        Network::Bitcoin => 8333,
        Network::Testnet => 18333,
        Network::Testnet4 => 48333,
        Network::Signet => 38333,
        _ => 18444,
    }
}

/// The `version` message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub version: u32,
    pub services: u64,
    /// Seconds since the Unix epoch.
    pub timestamp: i64,
    pub receiver: NetAddress,
    pub sender: NetAddress,
    pub nonce: u64,
    pub user_agent: String,
    pub start_height: i32,
    /// Whether the peer should announce transactions (BIP 37).
    pub relay: bool,
}

/// The parts of a Utreexo proof a `getuproof` asks for.
pub mod uproof_parts {
    /// The positions of the leaves in the forest.
    pub const TARGETS: u8 = 1 << 0;
    /// The hashes of the proof.
    pub const PROOF_HASHES: u8 = 1 << 1;
    /// The data of the spent outputs.
    pub const LEAF_DATA: u8 = 1 << 2;
    /// All of them.
    pub const ALL: u8 = TARGETS | PROOF_HASHES | LEAF_DATA;
}

/// BIP 183's `getuproof`: the Utreexo proof of a block's inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetUtreexoProof {
    pub block_hash: BlockHash,
    /// Which parts of the proof, [`uproof_parts`].
    pub parts: u8,
    /// One bit per proof hash, set for the hashes wanted; empty for all.
    pub proof_hashes: Vec<u8>,
    /// One bit per input spending an output of an earlier block, set for the leaves wanted;
    /// empty for all.
    pub leaves: Vec<u8>,
}

impl GetUtreexoProof {
    /// The whole proof of a block.
    pub fn all(block_hash: BlockHash) -> Self {
        Self {
            block_hash,
            parts: uproof_parts::ALL,
            proof_hashes: Vec::new(),
            leaves: Vec::new(),
        }
    }
}

/// BIP 183's `uproof`: the Utreexo proof of a block's inputs.
#[derive(Clone, Debug)]
pub struct UtreexoProof {
    pub block_hash: BlockHash,
    pub proof_hashes: Vec<BitcoinNodeHash>,
    /// The positions in the forest of the leaves the block spends.
    pub targets: Vec<u64>,
    /// The outputs the block spends, in the order of its inputs, without those created in the
    /// block itself.
    pub leaf_data: Vec<CompactLeafData>,
}

/// BIP 183's `utreexotx`: a transaction with the Utreexo proof of the outputs it spends that are in
/// the accumulator, and their data. On the wire each input's output index is shifted left, its low
/// bit set when the output is unconfirmed (in a mempool, not the accumulator).
#[derive(Clone, Debug)]
pub struct UtreexoTx {
    /// The transaction, its output indexes as they are.
    pub tx: bitcoin::Transaction,
    /// For each input, whether the output it spends is unconfirmed.
    pub unconfirmed: Vec<bool>,
    /// The positions in the forest of the leaves the confirmed inputs spend, in their order.
    pub targets: Vec<u64>,
    pub proof_hashes: Vec<BitcoinNodeHash>,
    /// The outputs the confirmed inputs spend, in their order.
    pub leaf_data: Vec<CompactLeafData>,
}

/// A message, decoded when the node uses it.
#[derive(Clone, Debug)]
pub enum Message {
    Version(Version),
    Verack,
    Ping(u64),
    Pong(u64),
    /// Headers after the first locator hash we share, up to `stop` (all zeros: up to
    /// [`MAX_HEADERS`]).
    GetHeaders {
        locator: Vec<BlockHash>,
        stop: BlockHash,
    },
    Headers(Vec<Header>),
    Inv(Vec<Inventory>),
    GetData(Vec<Inventory>),
    NotFound(Vec<Inventory>),
    Block(Block),
    GetUtreexoProof(GetUtreexoProof),
    UtreexoProof(UtreexoProof),
    /// Asks for addresses of peers.
    GetAddr,
    /// Addresses of peers, in the format of the `version` message.
    Addr(Vec<Address>),
    /// Asks for addresses in `addrv2` rather than `addr` (BIP 155): sent before `verack`.
    SendAddrV2,
    /// Addresses of peers, in BIP 155's format.
    AddrV2(Vec<Address>),
    /// A transaction with its Utreexo proof.
    UtreexoTx(UtreexoTx),
    /// Any other message, not decoded.
    Other {
        command: [u8; 12],
        payload: Vec<u8>,
    },
}

/// Pads a command name to 12 bytes.
const fn command(name: &str) -> [u8; 12] {
    let mut out = [0; 12];
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

const VERSION: [u8; 12] = command("version");
const VERACK: [u8; 12] = command("verack");
const PING: [u8; 12] = command("ping");
const PONG: [u8; 12] = command("pong");
const GETHEADERS: [u8; 12] = command("getheaders");
const HEADERS: [u8; 12] = command("headers");
const INV: [u8; 12] = command("inv");
const GETDATA: [u8; 12] = command("getdata");
const NOTFOUND: [u8; 12] = command("notfound");
const BLOCK: [u8; 12] = command("block");
const GETUPROOF: [u8; 12] = command("getuproof");
const UPROOF: [u8; 12] = command("uproof");
const GETADDR: [u8; 12] = command("getaddr");
const ADDR: [u8; 12] = command("addr");
const SENDADDRV2: [u8; 12] = command("sendaddrv2");
const ADDRV2: [u8; 12] = command("addrv2");
const UTREEXOTX: [u8; 12] = command("utreexotx");

impl Message {
    /// The command name, padded to 12 bytes.
    pub fn command(&self) -> [u8; 12] {
        match self {
            Self::Version(_) => VERSION,
            Self::Verack => VERACK,
            Self::Ping(_) => PING,
            Self::Pong(_) => PONG,
            Self::GetHeaders { .. } => GETHEADERS,
            Self::Headers(_) => HEADERS,
            Self::Inv(_) => INV,
            Self::GetData(_) => GETDATA,
            Self::NotFound(_) => NOTFOUND,
            Self::Block(_) => BLOCK,
            Self::GetUtreexoProof(_) => GETUPROOF,
            Self::UtreexoProof(_) => UPROOF,
            Self::GetAddr => GETADDR,
            Self::Addr(_) => ADDR,
            Self::SendAddrV2 => SENDADDRV2,
            Self::AddrV2(_) => ADDRV2,
            Self::UtreexoTx(_) => UTREEXOTX,
            Self::Other { command, .. } => *command,
        }
    }

    /// The payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Version(v) => {
                put(&mut out, &v.version);
                put(&mut out, &v.services);
                put(&mut out, &v.timestamp);
                put_address(&mut out, &v.receiver);
                put_address(&mut out, &v.sender);
                put(&mut out, &v.nonce);
                put(&mut out, &VarInt(v.user_agent.len() as u64));
                out.extend_from_slice(v.user_agent.as_bytes());
                put(&mut out, &v.start_height);
                out.push(v.relay as u8);
            }
            Self::Verack => {}
            Self::Ping(nonce) | Self::Pong(nonce) => put(&mut out, nonce),
            Self::GetHeaders { locator, stop } => {
                put(&mut out, &PROTOCOL_VERSION);
                put(&mut out, locator);
                put(&mut out, stop);
            }
            Self::Headers(headers) => {
                put(&mut out, &VarInt(headers.len() as u64));
                for header in headers {
                    put(&mut out, header);
                    // no transactions
                    out.push(0);
                }
            }
            Self::Inv(list) | Self::GetData(list) | Self::NotFound(list) => {
                put(&mut out, &VarInt(list.len() as u64));
                for entry in list {
                    put(&mut out, &entry.kind);
                    out.extend_from_slice(&entry.hash);
                }
            }
            Self::Block(block) => out = serialize(block),
            Self::GetUtreexoProof(get) => {
                put(&mut out, &get.block_hash);
                out.push(get.parts);
                put(&mut out, &get.proof_hashes);
                put(&mut out, &get.leaves);
            }
            Self::UtreexoProof(proof) => {
                put(&mut out, &proof.block_hash);
                put(&mut out, &VarInt(proof.proof_hashes.len() as u64));
                for hash in &proof.proof_hashes {
                    out.extend_from_slice(&**hash);
                }
                put(&mut out, &VarInt(proof.targets.len() as u64));
                for target in &proof.targets {
                    put(&mut out, &VarInt(*target));
                }
                put(&mut out, &VarInt(proof.leaf_data.len() as u64));
                for leaf in &proof.leaf_data {
                    put(&mut out, &leaf.header_code);
                    put(&mut out, &leaf.amount);
                    put(&mut out, &leaf.spk_ty);
                }
            }
            Self::GetAddr | Self::SendAddrV2 => {}
            Self::Addr(list) => {
                put(&mut out, &VarInt(list.len() as u64));
                for a in list {
                    put(&mut out, &a.time);
                    put_address(
                        &mut out,
                        &NetAddress {
                            services: a.services,
                            ip: match a.socket.ip() {
                                IpAddr::V4(ip) => ip.to_ipv6_mapped().octets(),
                                IpAddr::V6(ip) => ip.octets(),
                            },
                            port: a.socket.port(),
                        },
                    );
                }
            }
            Self::AddrV2(list) => {
                put(&mut out, &VarInt(list.len() as u64));
                for a in list {
                    put(&mut out, &a.time);
                    put(&mut out, &VarInt(a.services));
                    match a.socket.ip() {
                        IpAddr::V4(ip) => {
                            out.extend_from_slice(&[net_id::IPV4, 4]);
                            out.extend_from_slice(&ip.octets());
                        }
                        IpAddr::V6(ip) => {
                            out.extend_from_slice(&[net_id::IPV6, 16]);
                            out.extend_from_slice(&ip.octets());
                        }
                    }
                    out.extend_from_slice(&a.socket.port().to_be_bytes());
                }
            }
            Self::UtreexoTx(utx) => {
                put(&mut out, &VarInt(utx.targets.len() as u64));
                for target in &utx.targets {
                    put(&mut out, &VarInt(*target));
                }
                put(&mut out, &VarInt(utx.proof_hashes.len() as u64));
                for hash in &utx.proof_hashes {
                    out.extend_from_slice(&**hash);
                }
                let mut tx = utx.tx.clone();
                for (input, unconfirmed) in tx.input.iter_mut().zip(&utx.unconfirmed) {
                    input.previous_output.vout =
                        input.previous_output.vout << 1 | *unconfirmed as u32;
                }
                put(&mut out, &tx);
                for leaf in &utx.leaf_data {
                    put(&mut out, &leaf.header_code);
                    put(&mut out, &leaf.amount);
                    put(&mut out, &leaf.spk_ty);
                }
            }
            Self::Other { payload, .. } => out.extend_from_slice(payload),
        }
        out
    }

    /// Decodes a payload. Commands the node does not use come back as [`Message::Other`].
    pub fn decode(command: &[u8; 12], payload: &[u8]) -> Result<Self, encode::Error> {
        let r = &mut &payload[..];
        Ok(match *command {
            VERSION => {
                let version = u32::consensus_decode(r)?;
                let services = u64::consensus_decode(r)?;
                let timestamp = i64::consensus_decode(r)?;
                let receiver = get_address(r)?;
                let sender = get_address(r)?;
                let nonce = u64::consensus_decode(r)?;
                let user_agent = get_bytes(r, MAX_USER_AGENT)?;
                let user_agent = String::from_utf8(user_agent)
                    .map_err(|_| encode::Error::ParseFailed("user agent not UTF-8"))?;
                let start_height = i32::consensus_decode(r)?;
                // BIP 37: absent means true
                let relay = r.first().is_none_or(|b| *b != 0);
                Self::Version(Version {
                    version,
                    services,
                    timestamp,
                    receiver,
                    sender,
                    nonce,
                    user_agent,
                    start_height,
                    relay,
                })
            }
            VERACK => Self::Verack,
            PING => Self::Ping(u64::consensus_decode(r)?),
            PONG => Self::Pong(u64::consensus_decode(r)?),
            GETHEADERS => {
                let _version = u32::consensus_decode(r)?;
                let n = get_len(r, MAX_LOCATOR)?;
                let mut locator = Vec::with_capacity(n);
                for _ in 0..n {
                    locator.push(BlockHash::consensus_decode(r)?);
                }
                let stop = BlockHash::consensus_decode(r)?;
                Self::GetHeaders { locator, stop }
            }
            HEADERS => {
                let n = get_len(r, MAX_HEADERS)?;
                let mut headers = Vec::with_capacity(n);
                for _ in 0..n {
                    headers.push(Header::consensus_decode(r)?);
                    if VarInt::consensus_decode(r)?.0 != 0 {
                        return Err(encode::Error::ParseFailed("header with transactions"));
                    }
                }
                Self::Headers(headers)
            }
            INV | GETDATA | NOTFOUND => {
                let n = get_len(r, MAX_INV)?;
                let mut list = Vec::with_capacity(n);
                for _ in 0..n {
                    let kind = u32::consensus_decode(r)?;
                    let hash = <[u8; 32]>::consensus_decode(r)?;
                    list.push(Inventory { kind, hash });
                }
                match *command {
                    INV => Self::Inv(list),
                    GETDATA => Self::GetData(list),
                    _ => Self::NotFound(list),
                }
            }
            BLOCK => Self::Block(Block::consensus_decode(r)?),
            GETUPROOF => {
                let block_hash = BlockHash::consensus_decode(r)?;
                let parts = u8::consensus_decode(r)?;
                let proof_hashes = get_bytes(r, MAX_PROOF_HASHES.div_ceil(8))?;
                let leaves = get_bytes(r, MAX_INPUTS_PER_BLOCK.div_ceil(8))?;
                Self::GetUtreexoProof(GetUtreexoProof {
                    block_hash,
                    parts,
                    proof_hashes,
                    leaves,
                })
            }
            UPROOF => {
                let block_hash = BlockHash::consensus_decode(r)?;
                let n = get_len(r, MAX_PROOF_HASHES)?;
                let mut proof_hashes = Vec::with_capacity(n);
                for _ in 0..n {
                    proof_hashes.push(BitcoinNodeHash::Some(<[u8; 32]>::consensus_decode(r)?));
                }
                let n = get_len(r, MAX_INPUTS_PER_BLOCK)?;
                let mut targets = Vec::with_capacity(n);
                for _ in 0..n {
                    targets.push(VarInt::consensus_decode(r)?.0);
                }
                let n = get_len(r, MAX_INPUTS_PER_BLOCK)?;
                let mut leaf_data = Vec::with_capacity(n);
                for _ in 0..n {
                    leaf_data.push(CompactLeafData {
                        header_code: u32::consensus_decode(r)?,
                        amount: u64::consensus_decode(r)?,
                        spk_ty: ScriptPubKeyKind::consensus_decode(r)?,
                    });
                }
                Self::UtreexoProof(UtreexoProof {
                    block_hash,
                    proof_hashes,
                    targets,
                    leaf_data,
                })
            }
            UTREEXOTX => {
                let n = get_len(r, MAX_INPUTS_PER_BLOCK)?;
                let mut targets = Vec::with_capacity(n);
                for _ in 0..n {
                    targets.push(VarInt::consensus_decode(r)?.0);
                }
                let n = get_len(r, MAX_PROOF_HASHES)?;
                let mut proof_hashes = Vec::with_capacity(n);
                for _ in 0..n {
                    proof_hashes.push(BitcoinNodeHash::Some(<[u8; 32]>::consensus_decode(r)?));
                }
                let mut tx = bitcoin::Transaction::consensus_decode(r)?;
                let mut unconfirmed = Vec::with_capacity(tx.input.len());
                for input in &mut tx.input {
                    unconfirmed.push(input.previous_output.vout & 1 == 1);
                    input.previous_output.vout >>= 1;
                }
                let confirmed = unconfirmed.iter().filter(|u| !**u).count();
                let mut leaf_data = Vec::with_capacity(confirmed);
                for _ in 0..confirmed {
                    leaf_data.push(CompactLeafData {
                        header_code: u32::consensus_decode(r)?,
                        amount: u64::consensus_decode(r)?,
                        spk_ty: ScriptPubKeyKind::consensus_decode(r)?,
                    });
                }
                Self::UtreexoTx(UtreexoTx {
                    tx,
                    unconfirmed,
                    targets,
                    proof_hashes,
                    leaf_data,
                })
            }
            GETADDR => Self::GetAddr,
            SENDADDRV2 => Self::SendAddrV2,
            ADDR => {
                let n = get_len(r, MAX_ADDR)?;
                let mut list = Vec::with_capacity(n);
                for _ in 0..n {
                    let time = u32::consensus_decode(r)?;
                    let a = get_address(r)?;
                    let ip = Ipv6Addr::from(a.ip);
                    let ip = match ip.to_ipv4_mapped() {
                        Some(v4) => IpAddr::V4(v4),
                        None => IpAddr::V6(ip),
                    };
                    list.push(Address {
                        time,
                        services: a.services,
                        socket: SocketAddr::new(ip, a.port),
                    });
                }
                Self::Addr(list)
            }
            ADDRV2 => {
                let n = get_len(r, MAX_ADDR)?;
                let mut list = Vec::with_capacity(n);
                for _ in 0..n {
                    let time = u32::consensus_decode(r)?;
                    let services = VarInt::consensus_decode(r)?.0;
                    let network = u8::consensus_decode(r)?;
                    let bytes = get_bytes(r, MAX_ADDRV2_LEN)?;
                    let port = u16::from_be_bytes(<[u8; 2]>::consensus_decode(r)?);
                    let ip = match (network, bytes.len()) {
                        (net_id::IPV4, 4) => {
                            IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]))
                        }
                        (net_id::IPV6, 16) => {
                            let mut octets = [0; 16];
                            octets.copy_from_slice(&bytes);
                            let ip = Ipv6Addr::from(octets);
                            // BIP 155: IPv4 and Tor addresses embedded in IPv6 ones are ignored
                            if ip.to_ipv4_mapped().is_some()
                                || octets[..6] == [0xfd, 0x87, 0xd8, 0x7e, 0xeb, 0x43]
                            {
                                continue;
                            }
                            IpAddr::V6(ip)
                        }
                        (net_id::IPV4 | net_id::IPV6, _) => {
                            return Err(encode::Error::ParseFailed("address of the wrong length"))
                        }
                        // the other networks: not reachable from here
                        _ => continue,
                    };
                    list.push(Address {
                        time,
                        services,
                        socket: SocketAddr::new(ip, port),
                    });
                }
                Self::AddrV2(list)
            }
            _ => Self::Other {
                command: *command,
                payload: payload.to_vec(),
            },
        })
    }
}

fn put<T: Encodable + ?Sized>(out: &mut Vec<u8>, value: &T) {
    // writing into a vector does not fail
    let _ = value.consensus_encode(out);
}

fn put_address(out: &mut Vec<u8>, address: &NetAddress) {
    put(out, &address.services);
    out.extend_from_slice(&address.ip);
    out.extend_from_slice(&address.port.to_be_bytes());
}

fn get_address(r: &mut &[u8]) -> Result<NetAddress, encode::Error> {
    let services = u64::consensus_decode(r)?;
    let ip = <[u8; 16]>::consensus_decode(r)?;
    let port = <[u8; 2]>::consensus_decode(r)?;
    Ok(NetAddress {
        services,
        ip,
        port: u16::from_be_bytes(port),
    })
}

/// A length prefix, refused above `max`.
fn get_len(r: &mut &[u8], max: usize) -> Result<usize, encode::Error> {
    let n = VarInt::consensus_decode(r)?.0;
    if n > max as u64 {
        return Err(encode::Error::ParseFailed("too many entries"));
    }
    Ok(n as usize)
}

/// A length-prefixed byte string, refused above `max` bytes.
fn get_bytes(r: &mut &[u8], max: usize) -> Result<Vec<u8>, encode::Error> {
    let n = get_len(r, max)?;
    if r.len() < n {
        return Err(encode::Error::ParseFailed("truncated"));
    }
    let (bytes, rest) = r.split_at(n);
    *r = rest;
    Ok(bytes.to_vec())
}

/// The commands BIP 324 sends as one byte, from 1.
const V2_COMMANDS: [&str; 28] = [
    "addr",
    "block",
    "blocktxn",
    "cmpctblock",
    "feefilter",
    "filteradd",
    "filterclear",
    "filterload",
    "getblocks",
    "getblocktxn",
    "getdata",
    "getheaders",
    "headers",
    "inv",
    "mempool",
    "merkleblock",
    "notfound",
    "ping",
    "pong",
    "sendcmpct",
    "tx",
    "getcfilters",
    "cfilter",
    "getcfheaders",
    "cfheaders",
    "getcfcheckpt",
    "cfcheckpt",
    "addrv2",
];

/// The one-byte IDs Utreexo nodes give BIP 183's messages in the v2 transport, after BIP 324's.
const V2_UTREEXO: [(u8, &str); 3] = [(29, "uproof"), (30, "getuproof"), (34, "utreexotx")];

/// The one-byte ID of `name` in the v2 transport, if it has one.
fn v2_id(name: &[u8; 12]) -> Option<u8> {
    V2_COMMANDS
        .iter()
        .position(|c| command(c) == *name)
        .map(|i| i as u8 + 1)
        .or_else(|| {
            V2_UTREEXO
                .iter()
                .find(|(_, c)| command(c) == *name)
                .map(|(id, _)| *id)
        })
}

/// The command of a one-byte ID of the v2 transport, if known.
fn v2_command(id: u8) -> Option<[u8; 12]> {
    let known = match id {
        1..=28 => Some(V2_COMMANDS[id as usize - 1]),
        _ => V2_UTREEXO.iter().find(|(i, _)| *i == id).map(|(_, c)| *c),
    };
    known.map(command)
}

/// The most garbage a v2 peer may send before its terminator.
const MAX_GARBAGE: usize = 4095;
/// The longest packet taken during the v2 handshake (decoys and the version packet).
const MAX_HANDSHAKE_PACKET: usize = 4096;
/// A v2 packet's bytes beside its contents: length, header byte, tag.
const V2_OVERHEAD: usize = 3 + 1 + 16;

/// What can go wrong on a connection.
#[derive(Debug)]
pub enum Error<E> {
    /// The stream failed.
    Io(E),
    /// The peer closed the stream.
    Closed,
    /// The v2 transport: a handshake or a packet that does not check out.
    V2(bip324::Error),
    /// A message of another network.
    WrongNetwork([u8; 4]),
    /// A payload above [`MAX_PAYLOAD`].
    Oversized(u32),
    /// A payload whose checksum does not match.
    Checksum,
    /// A payload that does not decode.
    Decode {
        command: [u8; 12],
        error: encode::Error,
    },
}

/// Random bytes given beforehand, 32 at a time.
struct Given([u8; 64], usize);

impl FillBytes for Given {
    fn fill_bytes(&mut self, dest: &mut [u8; 32]) {
        let at = self.1 % 2 * 32;
        dest.copy_from_slice(&self.0[at..at + 32]);
        self.1 += 1;
    }
}

/// The ciphers of a v2 connection.
struct V2 {
    inbound: InboundCipher,
    outbound: OutboundCipher,
}

/// Bytes from `io`, those of `ahead` first: 0 when it closed. Cancel-safe when `io`'s `read` is.
async fn read_ahead<T: Read>(
    ahead: &mut Vec<u8>,
    io: &mut T,
    buf: &mut [u8],
) -> Result<usize, Error<T::Error>> {
    if !ahead.is_empty() {
        let n = buf.len().min(ahead.len());
        buf[..n].copy_from_slice(&ahead[..n]);
        ahead.drain(..n);
        return Ok(n);
    }
    io.read(buf).await.map_err(Error::Io)
}

/// A connection to a peer over a byte stream: in the v1 transport, or in the v2 once
/// [`Connection::start_v2`] did its handshake.
pub struct Connection<T> {
    io: T,
    magic: [u8; 4],
    v2: Option<V2>,
    /// Bytes read ahead during the v2 handshake, taken before the stream's.
    ahead: Vec<u8>,
    /// The message being received, as far as read: its header (24 bytes in v1, the 3 bytes of
    /// the encrypted length in v2), then its payload (the packet in v2). The payload's buffer is
    /// kept from one message to the next, grown to the largest: a heap is not cut up by a large
    /// buffer freed after each message.
    head: [u8; 24],
    head_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
}

/// Makes `buffer` `len` bytes long, growing its capacity to that exactly if it must.
fn size_to(buffer: &mut Vec<u8>, len: usize) {
    if len > buffer.capacity() {
        buffer.reserve_exact(len - buffer.len());
    }
    buffer.resize(len, 0);
}

impl<T: Read + Write> Connection<T> {
    /// A connection in the v1 transport.
    pub fn new(io: T, network: Network) -> Self {
        Self::with_buffer(io, network, Vec::new())
    }

    /// A connection in the v1 transport receiving into `buffer`, that of an earlier connection
    /// ([`Connection::into_buffer`]).
    pub fn with_buffer(io: T, network: Network, buffer: Vec<u8>) -> Self {
        Self {
            io,
            magic: network.magic().to_bytes(),
            v2: None,
            ahead: Vec::new(),
            head: [0; 24],
            head_read: 0,
            payload: buffer,
            payload_read: 0,
        }
    }

    /// The buffer the connection received into, for the next one.
    pub fn into_buffer(self) -> Vec<u8> {
        self.payload
    }

    /// Whether the connection is in the v2 transport.
    pub fn is_v2(&self) -> bool {
        self.v2.is_some()
    }

    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error<T::Error>> {
        let mut done = 0;
        while done < buf.len() {
            match read_ahead(&mut self.ahead, &mut self.io, &mut buf[done..]).await? {
                0 => return Err(Error::Closed),
                n => done += n,
            }
        }
        Ok(())
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<(), Error<T::Error>> {
        self.io.write_all(bytes).await.map_err(Error::Io)?;
        self.io.flush().await.map_err(Error::Io)
    }

    /// The v2 handshake (BIP 324) as `role`, with no garbage nor decoys of ours: the keys, then
    /// each side's version packet; every message after it is encrypted. `random`: 64 random
    /// bytes, the secret key and the randomness of its encoding. A v1 peer closes the connection
    /// at our key, or sends its `version` (`Error::V2(V1Protocol)`).
    pub async fn start_v2(&mut self, role: Role, random: [u8; 64]) -> Result<(), Error<T::Error>> {
        let curve = secp256k1::Secp256k1::verification_only();
        let mut random = Given(random, 0);
        let handshake =
            Handshake::<Initialized>::new_with_rng(self.magic, role, &mut random, &curve)
                .map_err(Error::V2)?;
        let mut key = [0; 64];
        let handshake = handshake.send_key(None, &mut key).map_err(Error::V2)?;
        self.write(&key).await?;
        let mut theirs = [0; 64];
        self.read_exact(&mut theirs).await?;
        let handshake = handshake.receive_key(theirs).map_err(Error::V2)?;
        let mut out = vec![0; Handshake::<ReceivedKey>::send_version_len(None)];
        let handshake = handshake.send_version(&mut out, None).map_err(Error::V2)?;
        self.write(&out).await?;

        // their garbage up to its terminator
        let mut garbage = vec![0; 16];
        self.read_exact(&mut garbage).await?;
        let mut waiting = handshake;
        let (mut handshake, used) = loop {
            match waiting.receive_garbage(&garbage).map_err(Error::V2)? {
                GarbageResult::FoundGarbage {
                    handshake,
                    consumed_bytes,
                } => break (handshake, consumed_bytes),
                GarbageResult::NeedMoreData(h) => {
                    waiting = h;
                    if garbage.len() > MAX_GARBAGE + 16 {
                        return Err(Error::V2(bip324::Error::NoGarbageTerminator));
                    }
                    let mut more = [0; 256];
                    match read_ahead(&mut self.ahead, &mut self.io, &mut more).await? {
                        0 => return Err(Error::Closed),
                        n => garbage.extend_from_slice(&more[..n]),
                    }
                }
            }
        };
        // what came after the terminator is read before the stream
        let mut rest = garbage[used..].to_vec();
        rest.extend_from_slice(&self.ahead);
        self.ahead = rest;
        // their decoys, then their version packet
        loop {
            let mut length = [0; 3];
            self.read_exact(&mut length).await?;
            let len = handshake.decrypt_packet_len(length).map_err(Error::V2)?;
            if len > MAX_HANDSHAKE_PACKET {
                return Err(Error::V2(bip324::Error::PacketTooBig));
            }
            let mut packet = vec![0; len];
            self.read_exact(&mut packet).await?;
            match handshake.receive_version(&mut packet).map_err(Error::V2)? {
                VersionResult::Complete { cipher } => {
                    let (inbound, outbound) = cipher.into_split();
                    self.v2 = Some(V2 { inbound, outbound });
                    return Ok(());
                }
                VersionResult::Decoy(h) => handshake = h,
            }
        }
    }

    /// Sends a message.
    pub async fn send(&mut self, message: &Message) -> Result<(), Error<T::Error>> {
        let payload = message.encode();
        if let Some(v2) = &mut self.v2 {
            let name = message.command();
            let mut contents = Vec::with_capacity(13 + payload.len());
            match v2_id(&name) {
                Some(id) => contents.push(id),
                None => {
                    contents.push(0);
                    contents.extend_from_slice(&name);
                }
            }
            contents.extend_from_slice(&payload);
            let mut packet = vec![0; contents.len() + V2_OVERHEAD];
            v2.outbound
                .encrypt(&contents, &mut packet, PacketType::Genuine, None)
                .map_err(Error::V2)?;
            return self.write(&packet).await;
        }
        let mut head = [0; 24];
        head[..4].copy_from_slice(&self.magic);
        head[4..16].copy_from_slice(&message.command());
        head[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        head[20..].copy_from_slice(&sha256d::Hash::hash(&payload)[..4]);
        self.io.write_all(&head).await.map_err(Error::Io)?;
        self.io.write_all(&payload).await.map_err(Error::Io)?;
        self.io.flush().await.map_err(Error::Io)
    }

    /// Receives the next message. Cancel-safe when the stream's `read` is: a call dropped before
    /// it returns keeps what it read, and the next call goes on from there.
    pub async fn recv(&mut self) -> Result<Message, Error<T::Error>> {
        if self.v2.is_some() {
            return self.recv_v2().await;
        }
        while self.head_read < self.head.len() {
            let n = read_ahead(
                &mut self.ahead,
                &mut self.io,
                &mut self.head[self.head_read..],
            )
            .await?;
            if n == 0 {
                return Err(Error::Closed);
            }
            self.head_read += n;
            if self.head_read == self.head.len() {
                let mut magic = [0; 4];
                magic.copy_from_slice(&self.head[..4]);
                if magic != self.magic {
                    return Err(Error::WrongNetwork(magic));
                }
                let mut len = [0; 4];
                len.copy_from_slice(&self.head[16..20]);
                let len = u32::from_le_bytes(len);
                if len as usize > MAX_PAYLOAD {
                    return Err(Error::Oversized(len));
                }
                size_to(&mut self.payload, len as usize);
                self.payload_read = 0;
            }
        }
        self.read_payload().await?;
        // the whole message is here: the next call starts another
        self.head_read = 0;
        if sha256d::Hash::hash(&self.payload)[..4] != self.head[20..] {
            return Err(Error::Checksum);
        }
        let mut command = [0; 12];
        command.copy_from_slice(&self.head[4..16]);
        Message::decode(&command, &self.payload).map_err(|error| Error::Decode { command, error })
    }

    /// Reads the rest of the payload being received.
    async fn read_payload(&mut self) -> Result<(), Error<T::Error>> {
        while self.payload_read < self.payload.len() {
            let n = read_ahead(
                &mut self.ahead,
                &mut self.io,
                &mut self.payload[self.payload_read..],
            )
            .await?;
            if n == 0 {
                return Err(Error::Closed);
            }
            self.payload_read += n;
        }
        Ok(())
    }

    /// [`Connection::recv`] in the v2 transport: the encrypted length, then the packet; decoys
    /// and commands of unknown one-byte IDs skipped.
    async fn recv_v2(&mut self) -> Result<Message, Error<T::Error>> {
        loop {
            while self.head_read < 3 {
                let n = read_ahead(
                    &mut self.ahead,
                    &mut self.io,
                    &mut self.head[self.head_read..3],
                )
                .await?;
                if n == 0 {
                    return Err(Error::Closed);
                }
                self.head_read += n;
                if self.head_read == 3 {
                    let v2 = self.v2.as_mut().expect("a v2 connection");
                    let length = [self.head[0], self.head[1], self.head[2]];
                    let len = v2.inbound.decrypt_packet_len(length);
                    if len > MAX_PAYLOAD + 13 + V2_OVERHEAD {
                        return Err(Error::Oversized(len as u32));
                    }
                    size_to(&mut self.payload, len);
                    self.payload_read = 0;
                }
            }
            self.read_payload().await?;
            self.head_read = 0;
            let v2 = self.v2.as_mut().expect("a v2 connection");
            let (kind, plain) = v2
                .inbound
                .decrypt_in_place(&mut self.payload, None)
                .map_err(Error::V2)?;
            if kind == PacketType::Decoy {
                continue;
            }
            // the header byte, then the command, then the payload
            let contents = &plain[1..];
            let (name, payload) = match contents.first() {
                Some(0) if contents.len() >= 13 => {
                    let mut name = [0; 12];
                    name.copy_from_slice(&contents[1..13]);
                    (name, &contents[13..])
                }
                Some(0) | None => return Err(Error::V2(bip324::Error::CiphertextTooSmall)),
                Some(&id) => match v2_command(id) {
                    Some(name) => (name, &contents[1..]),
                    None => continue,
                },
            };
            return Message::decode(&name, payload).map_err(|error| Error::Decode {
                command: name,
                error,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use bitcoin::consensus::encode::deserialize_hex;

    use super::*;

    fn roundtrip(message: Message) -> Message {
        Message::decode(&message.command(), &message.encode()).unwrap()
    }

    #[test]
    fn version() {
        let version = Version {
            version: PROTOCOL_VERSION,
            services: services::WITNESS,
            timestamp: 1_700_000_000,
            receiver: NetAddress::default(),
            sender: NetAddress {
                services: 0,
                ip: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 127, 0, 0, 1],
                port: 8333,
            },
            nonce: 0x0123_4567_89ab_cdef,
            user_agent: "/ecsn:0.1.0/".into(),
            start_height: 42,
            relay: false,
        };
        let encoded = Message::Version(version.clone()).encode();
        // 4 + 8 + 8 + 26 + 26 + 8 + (1 + 12) + 4 + 1
        assert_eq!(encoded.len(), 98);
        assert_eq!(&encoded[46 + 20..46 + 26], &[127, 0, 0, 1, 0x20, 0x8d]);
        match roundtrip(Message::Version(version.clone())) {
            Message::Version(v) => assert_eq!(v, version),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn version_without_relay_relays() {
        let mut encoded = Message::Version(Version {
            version: 70001,
            services: 0,
            timestamp: 0,
            receiver: NetAddress::default(),
            sender: NetAddress::default(),
            nonce: 0,
            user_agent: String::new(),
            start_height: 0,
            relay: false,
        })
        .encode();
        encoded.pop();
        match Message::decode(&VERSION, &encoded).unwrap() {
            Message::Version(v) => assert!(v.relay),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn headers() {
        let genesis = bitcoin::constants::genesis_block(Network::Bitcoin).header;
        let encoded = Message::Headers(vec![genesis, genesis]).encode();
        assert_eq!(encoded.len(), 1 + 2 * 81);
        match roundtrip(Message::Headers(vec![genesis, genesis])) {
            Message::Headers(h) => assert_eq!(h, vec![genesis, genesis]),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn header_with_transactions_is_refused() {
        let genesis = bitcoin::constants::genesis_block(Network::Bitcoin).header;
        let mut encoded = Message::Headers(vec![genesis]).encode();
        *encoded.last_mut().unwrap() = 1;
        assert!(Message::decode(&HEADERS, &encoded).is_err());
    }

    #[test]
    fn getdata_witness_block() {
        let hash = bitcoin::constants::genesis_block(Network::Bitcoin).block_hash();
        let encoded = Message::GetData(vec![Inventory::witness_block(hash)]).encode();
        assert_eq!(&encoded[..5], &[1, 2, 0, 0, 0x40]);
        match roundtrip(Message::GetData(vec![Inventory::witness_block(hash)])) {
            Message::GetData(list) => assert_eq!(list[0].block(), Some(hash)),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn getuproof_asks_for_everything() {
        let hash = bitcoin::constants::genesis_block(Network::Bitcoin).block_hash();
        let encoded = Message::GetUtreexoProof(GetUtreexoProof::all(hash)).encode();
        let mut expected = serialize(&hash);
        expected.extend_from_slice(&[0b111, 0, 0]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn uproof() {
        // block hash, 1 proof hash, 2 targets (1 and 300), 1 leaf (P2PKH)
        let mut payload: Vec<u8> = deserialize_hex::<Vec<u8>>(
            "20\
             6fe28c0ab6f1b372c1a6a246ae63f74f931e8365e15a089c68d6190000000000",
        )
        .unwrap();
        payload.push(1);
        payload.extend_from_slice(&[0xaa; 32]);
        payload.extend_from_slice(&[2, 1, 0xfd, 0x2c, 0x01]);
        payload.push(1);
        payload.extend_from_slice(&(7u32 << 1).to_le_bytes());
        payload.extend_from_slice(&5_000_000_000u64.to_le_bytes());
        payload.push(1);
        match Message::decode(&UPROOF, &payload).unwrap() {
            Message::UtreexoProof(proof) => {
                assert_eq!(proof.proof_hashes, vec![BitcoinNodeHash::Some([0xaa; 32])]);
                assert_eq!(proof.targets, vec![1, 300]);
                assert_eq!(proof.leaf_data.len(), 1);
                assert_eq!(proof.leaf_data[0].header_code, 14);
                assert_eq!(proof.leaf_data[0].amount, 5_000_000_000);
                assert_eq!(proof.leaf_data[0].spk_ty, ScriptPubKeyKind::PubKeyHash);
                assert_eq!(Message::UtreexoProof(proof).encode(), payload);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn addr_carries_ipv4_mapped_into_ipv6() {
        let list = vec![
            Address {
                time: 1_700_000_000,
                services: services::NETWORK | services::WITNESS,
                socket: "1.2.3.4:8333".parse().unwrap(),
            },
            Address {
                time: 1,
                services: 0,
                socket: "[2001:db8::1]:18333".parse().unwrap(),
            },
        ];
        let encoded = Message::Addr(list.clone()).encode();
        // the count, then time, services, address and port: 30 bytes each
        assert_eq!(encoded.len(), 1 + 2 * 30);
        assert_eq!(
            &encoded[13..31],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 1, 2, 3, 4, 0x20, 0x8d]
        );
        match roundtrip(Message::Addr(list.clone())) {
            Message::Addr(l) => assert_eq!(l, list),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn addrv2_keeps_the_ip_addresses() {
        let mut payload = vec![3];
        // 1.2.3.4:8333, NETWORK | WITNESS
        payload.extend_from_slice(&1_700_000_000u32.to_le_bytes());
        payload.extend_from_slice(&[9, 1, 4, 1, 2, 3, 4, 0x20, 0x8d]);
        // a Tor v3 address: dropped
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&[0, 4, 32]);
        payload.extend_from_slice(&[0xab; 32]);
        payload.extend_from_slice(&[0x20, 0x8d]);
        // [2001:db8::1]:18333, UTREEXO | UTREEXO_ARCHIVE in a 3-byte CompactSize
        payload.extend_from_slice(&5u32.to_le_bytes());
        payload.extend_from_slice(&[0xfd, 0x00, 0x30, 2, 16, 0x20, 0x01, 0x0d, 0xb8]);
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0x47, 0x9d]);
        let expected = vec![
            Address {
                time: 1_700_000_000,
                services: 9,
                socket: "1.2.3.4:8333".parse().unwrap(),
            },
            Address {
                time: 5,
                services: services::UTREEXO | services::UTREEXO_ARCHIVE,
                socket: "[2001:db8::1]:18333".parse().unwrap(),
            },
        ];
        match Message::decode(&ADDRV2, &payload).unwrap() {
            Message::AddrV2(list) => assert_eq!(list, expected),
            m => panic!("{m:?}"),
        }
        match roundtrip(Message::AddrV2(expected.clone())) {
            Message::AddrV2(list) => assert_eq!(list, expected),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn addrv2_refuses_an_ipv4_address_of_the_wrong_length() {
        let mut payload = vec![1];
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&[9, 1, 5, 1, 2, 3, 4, 5, 0x20, 0x8d]);
        assert!(Message::decode(&ADDRV2, &payload).is_err());
    }

    #[tokio::test]
    async fn a_receive_dropped_halfway_goes_on_from_there() {
        use embedded_io_adapters::tokio_1::FromTokio;
        use tokio::io::AsyncWriteExt;

        let (ours, mut theirs) = tokio::io::duplex(1 << 10);
        let mut conn = Connection::new(FromTokio::new(ours), Network::Regtest);
        let payload = Message::Ping(7).encode();
        let mut frame = Network::Regtest.magic().to_bytes().to_vec();
        frame.extend_from_slice(&PING);
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&sha256d::Hash::hash(&payload)[..4]);
        frame.extend_from_slice(&payload);
        // the header and part of the payload, then the receive is dropped
        theirs.write_all(&frame[..28]).await.unwrap();
        let wait = std::time::Duration::from_millis(20);
        assert!(tokio::time::timeout(wait, conn.recv()).await.is_err());
        theirs.write_all(&frame[28..]).await.unwrap();
        assert!(matches!(conn.recv().await.unwrap(), Message::Ping(7)));
    }

    /// 64 bytes random enough for tests: SplitMix64 from `seed`.
    fn random(mut seed: u64) -> [u8; 64] {
        let mut out = [0; 64];
        for chunk in out.chunks_mut(8) {
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            chunk.copy_from_slice(&(z ^ (z >> 31)).to_le_bytes());
        }
        out
    }

    type Io = embedded_io_adapters::tokio_1::FromTokio<tokio::io::DuplexStream>;

    /// Two connections in the v2 transport over a stream of `buffer` bytes.
    async fn v2_pair(buffer: usize) -> (Connection<Io>, Connection<Io>) {
        use embedded_io_adapters::tokio_1::FromTokio;
        let (a, b) = tokio::io::duplex(buffer);
        let mut a = Connection::new(FromTokio::new(a), Network::Signet);
        let mut b = Connection::new(FromTokio::new(b), Network::Signet);
        let (ra, rb) = tokio::join!(
            a.start_v2(Role::Initiator, random(1)),
            b.start_v2(Role::Responder, random(2))
        );
        ra.unwrap();
        rb.unwrap();
        assert!(a.is_v2() && b.is_v2());
        (a, b)
    }

    #[tokio::test]
    async fn v2_carries_messages_of_one_byte_ids_and_long_commands() {
        let (mut a, mut b) = v2_pair(1 << 16).await;
        let genesis = bitcoin::constants::genesis_block(Network::Signet);
        let hash = genesis.block_hash();
        let messages = vec![
            Message::Verack,
            Message::Ping(42),
            Message::Headers(vec![genesis.header]),
            Message::GetUtreexoProof(GetUtreexoProof::all(hash)),
            Message::SendAddrV2,
        ];
        for m in &messages {
            a.send(m).await.unwrap();
        }
        for m in &messages {
            let got = b.recv().await.unwrap();
            assert_eq!(got.command(), m.command());
            assert_eq!(got.encode(), m.encode());
        }
        // and back
        b.send(&Message::Pong(42)).await.unwrap();
        assert!(matches!(a.recv().await.unwrap(), Message::Pong(42)));
    }

    #[test]
    fn the_commonest_commands_take_one_byte() {
        assert_eq!(v2_id(&command("addr")), Some(1));
        assert_eq!(v2_id(&command("block")), Some(2));
        assert_eq!(v2_id(&command("headers")), Some(13));
        assert_eq!(v2_id(&command("addrv2")), Some(28));
        assert_eq!(v2_id(&command("version")), None);
        assert_eq!(v2_id(&command("uproof")), Some(29));
        assert_eq!(v2_id(&command("getuproof")), Some(30));
        assert_eq!(v2_command(29), Some(command("uproof")));
        assert_eq!(v2_command(31), None);
    }

    /// Two streams joined by a relay that passes 4 kB every 2 ms one way, at once the other.
    fn slow_link() -> (Io, Io, impl core::future::Future<Output = ()>) {
        use embedded_io_adapters::tokio_1::FromTokio;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (a, a_far) = tokio::io::duplex(1 << 20);
        let (b, b_far) = tokio::io::duplex(1 << 20);
        let relay = async move {
            let (mut ar, mut aw) = tokio::io::split(a_far);
            let (mut br, mut bw) = tokio::io::split(b_far);
            let forward = async {
                let mut buf = [0; 4096];
                while let Ok(n @ 1..) = ar.read(&mut buf).await {
                    if bw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                }
            };
            let back = async {
                let _ = tokio::io::copy(&mut br, &mut aw).await;
            };
            tokio::join!(forward, back);
        };
        (FromTokio::new(a), FromTokio::new(b), relay)
    }

    /// Sends a 162 kB `headers` from `a` while `b`'s receive is dropped every millisecond.
    async fn interrupted(a: &mut Connection<Io>, b: &mut Connection<Io>) {
        let genesis = bitcoin::constants::genesis_block(Network::Signet).header;
        let big = Message::Headers(vec![genesis; MAX_HEADERS]);
        let send = async { a.send(&big).await.unwrap() };
        let receive = async {
            let mut drops = 0;
            loop {
                let wait = std::time::Duration::from_millis(1);
                match tokio::time::timeout(wait, b.recv()).await {
                    Ok(m) => break (m.unwrap(), drops),
                    Err(_) => drops += 1,
                }
            }
        };
        let ((), (got, drops)) = tokio::join!(send, receive);
        assert_eq!(got.encode(), big.encode());
        assert!(drops > 10, "{drops} drops");
    }

    #[tokio::test]
    async fn a_large_message_survives_receives_dropped_halfway_in_v1() {
        let (a, b, relay) = slow_link();
        let mut a = Connection::new(a, Network::Signet);
        let mut b = Connection::new(b, Network::Signet);
        tokio::select! {
            () = relay => panic!("the relay stopped"),
            () = interrupted(&mut a, &mut b) => {}
        }
    }

    #[tokio::test]
    async fn a_large_message_survives_receives_dropped_halfway_in_v2() {
        let (a, b, relay) = slow_link();
        let mut a = Connection::new(a, Network::Signet);
        let mut b = Connection::new(b, Network::Signet);
        let test = async {
            let (ra, rb) = tokio::join!(
                a.start_v2(Role::Initiator, random(4)),
                b.start_v2(Role::Responder, random(5))
            );
            ra.unwrap();
            rb.unwrap();
            interrupted(&mut a, &mut b).await;
        };
        tokio::select! {
            () = relay => panic!("the relay stopped"),
            () = test => {}
        }
    }

    #[tokio::test]
    async fn a_v1_peer_ends_the_v2_handshake() {
        use embedded_io_adapters::tokio_1::FromTokio;
        let (a, b) = tokio::io::duplex(1 << 16);
        let mut ours = Connection::new(FromTokio::new(a), Network::Signet);
        let theirs = async move {
            // a v1 peer: our key is not a message of its network, it hangs up
            let mut v1 = Connection::new(FromTokio::new(b), Network::Signet);
            assert!(matches!(v1.recv().await, Err(Error::WrongNetwork(_))));
        };
        let (result, ()) = tokio::join!(ours.start_v2(Role::Initiator, random(3)), theirs);
        assert!(matches!(result, Err(Error::Closed)), "{result:?}");
        assert!(!ours.is_v2());
    }

    #[test]
    fn positions_pack_four_to_a_hash_padded() {
        let positions = [1, 300, 1 << 40, 7, 9];
        let packed = pack_positions(&positions);
        assert_eq!(packed.len(), 2);
        assert_eq!(&packed[0][..8], &1u64.to_le_bytes());
        assert_eq!(&packed[1][8..16], &[0xff; 8]);
        assert_eq!(unpack_positions(&packed), positions);
        assert!(pack_positions(&[]).is_empty());
    }

    #[test]
    fn a_utreexotx_marks_its_unconfirmed_inputs() {
        use bitcoin::{transaction, Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Witness};
        let input = |vout| TxIn {
            previous_output: OutPoint {
                txid: bitcoin::Txid::from_byte_array([7; 32]),
                vout,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[[1u8; 72]]),
        };
        let utx = UtreexoTx {
            tx: bitcoin::Transaction {
                version: transaction::Version::TWO,
                lock_time: bitcoin::absolute::LockTime::ZERO,
                input: vec![input(3), input(0)],
                output: vec![TxOut {
                    value: Amount::from_sat(1000),
                    script_pubkey: ScriptBuf::new(),
                }],
            },
            unconfirmed: vec![false, true],
            targets: vec![42],
            proof_hashes: vec![BitcoinNodeHash::Some([0xaa; 32])],
            leaf_data: vec![CompactLeafData {
                header_code: 100 << 1,
                amount: 5000,
                spk_ty: ScriptPubKeyKind::WitnessV0PubKeyHash,
            }],
        };
        let encoded = Message::UtreexoTx(utx.clone()).encode();
        // targets, hashes, then the transaction (version, segwit marker and flag, input count):
        // its first input's index is 3 << 1, its second's (after txid, index, an empty script and
        // the sequence) 0 << 1 | 1
        let first = 1 + 1 + 1 + 32 + 4 + 2 + 1 + 32;
        assert_eq!(&encoded[first..first + 4], &6u32.to_le_bytes());
        let second = first + 4 + 1 + 4 + 32;
        assert_eq!(&encoded[second..second + 4], &1u32.to_le_bytes());
        match roundtrip(Message::UtreexoTx(utx.clone())) {
            Message::UtreexoTx(got) => {
                assert_eq!(got.tx, utx.tx);
                assert_eq!(got.unconfirmed, utx.unconfirmed);
                assert_eq!(got.targets, utx.targets);
                assert_eq!(got.proof_hashes, utx.proof_hashes);
                assert_eq!(got.leaf_data.len(), 1);
                assert_eq!(got.leaf_data[0].amount, 5000);
            }
            m => panic!("{m:?}"),
        }
        assert_eq!(v2_id(&command("utreexotx")), Some(34));
    }

    #[test]
    fn other_commands_pass_through() {
        let m = Message::decode(&command("sendcmpct"), &[0, 1, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(matches!(m, Message::Other { .. }));
        assert_eq!(m.command(), command("sendcmpct"));
    }
}
