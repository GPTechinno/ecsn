//! The Bitcoin P2P messages the node uses (protocol version 70016, BIP 144 witness blocks), with
//! BIP 183's Utreexo proof messages and BIP 155's `addrv2`, and their framing over a byte stream
//! (the v1 transport: magic, command, length, checksum).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

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
}

/// An entry of `inv`, `getdata` and `notfound`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inventory {
    pub kind: u32,
    pub hash: [u8; 32],
}

impl Inventory {
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

/// What can go wrong on a connection.
#[derive(Debug)]
pub enum Error<E> {
    /// The stream failed.
    Io(E),
    /// The peer closed the stream.
    Closed,
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

/// A connection to a peer over a byte stream, in the v1 transport.
pub struct Connection<T> {
    io: T,
    magic: [u8; 4],
    /// The message being received, as far as read: its header, then its payload.
    head: [u8; 24],
    head_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
}

impl<T: Read + Write> Connection<T> {
    pub fn new(io: T, network: Network) -> Self {
        Self {
            io,
            magic: network.magic().to_bytes(),
            head: [0; 24],
            head_read: 0,
            payload: Vec::new(),
            payload_read: 0,
        }
    }

    /// Sends a message.
    pub async fn send(&mut self, message: &Message) -> Result<(), Error<T::Error>> {
        let payload = message.encode();
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
        while self.head_read < self.head.len() {
            let n = self
                .io
                .read(&mut self.head[self.head_read..])
                .await
                .map_err(Error::Io)?;
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
                self.payload = vec![0; len as usize];
                self.payload_read = 0;
            }
        }
        while self.payload_read < self.payload.len() {
            let n = self
                .io
                .read(&mut self.payload[self.payload_read..])
                .await
                .map_err(Error::Io)?;
            if n == 0 {
                return Err(Error::Closed);
            }
            self.payload_read += n;
        }
        // the whole message is here: the next call starts another
        self.head_read = 0;
        let payload = core::mem::take(&mut self.payload);
        if sha256d::Hash::hash(&payload)[..4] != self.head[20..] {
            return Err(Error::Checksum);
        }
        let mut command = [0; 12];
        command.copy_from_slice(&self.head[4..16]);
        Message::decode(&command, &payload).map_err(|error| Error::Decode { command, error })
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

    #[test]
    fn other_commands_pass_through() {
        let m = Message::decode(&command("sendcmpct"), &[0, 1, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(matches!(m, Message::Other { .. }));
        assert_eq!(m.command(), command("sendcmpct"));
    }
}
