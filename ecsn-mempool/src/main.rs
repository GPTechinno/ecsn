//! Records what a node would keep in its mempool: every transaction the peers relay, as it comes,
//! and every new block, with their time, for `ecsn-replay` to replay on the board's heap.
//!
//! It connects to a few peers of the network's DNS seeds (others when one leaves), asks for
//! transaction announcements (`relay` in its `version`), and fetches every transaction and block
//! announced, once. Nothing is validated.
//!
//! The recording: the magic `ECSNMPL1`, then records of a time (milliseconds since the start, 8
//! bytes), a kind (1: a transaction, 2: a block), a length (4 bytes), and the raw bytes in their
//! witness serialization; little-endian.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitcoin::consensus::encode::{deserialize, serialize};
use bitcoin::{Network, Transaction};
use clap::Parser;
use ecsn::addrbook::dns_seeds;
use ecsn::p2p::{
    default_port, inv, Connection, Inventory, Message, NetAddress, Version, PROTOCOL_VERSION,
};
use embedded_io_adapters::tokio_1::FromTokio;
use tokio::net::TcpStream;

/// Kinds of records.
const TX: u8 = 1;
const BLOCK: u8 = 2;

#[derive(Parser)]
struct Args {
    /// bitcoin, testnet, testnet4 or signet
    #[arg(long, default_value = "bitcoin")]
    network: Network,
    /// Peers to keep connected
    #[arg(long, default_value_t = 3)]
    peers: usize,
    /// How long to record
    #[arg(long, default_value_t = 120)]
    minutes: u64,
    /// The recording
    #[arg(long)]
    out: PathBuf,
}

/// What the connections share: what was recorded, and the recording.
struct Recorder {
    start: Instant,
    txs: HashSet<[u8; 32]>,
    blocks: HashSet<[u8; 32]>,
    out: BufWriter<File>,
    tx_bytes: u64,
    block_bytes: u64,
}

impl Recorder {
    fn write(&mut self, kind: u8, bytes: &[u8]) -> std::io::Result<()> {
        let ms = self.start.elapsed().as_millis() as u64;
        self.out.write_all(&ms.to_le_bytes())?;
        self.out.write_all(&[kind])?;
        self.out.write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.out.write_all(bytes)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();
    let file = match File::create(&args.out) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}: {e}", args.out.display());
            return ExitCode::FAILURE;
        }
    };
    let mut out = BufWriter::new(file);
    if out.write_all(b"ECSNMPL1").is_err() {
        return ExitCode::FAILURE;
    }
    let recorder = Rc::new(RefCell::new(Recorder {
        start: Instant::now(),
        txs: HashSet::new(),
        blocks: HashSet::new(),
        out,
        tx_bytes: 0,
        block_bytes: 0,
    }));

    // the seeds' addresses, IPv4
    let port = default_port(args.network);
    let mut candidates: Vec<SocketAddr> = Vec::new();
    for seed in dns_seeds(args.network) {
        if let Ok(addrs) = tokio::net::lookup_host((seed.name, port)).await {
            candidates.extend(addrs.filter(SocketAddr::is_ipv4));
        }
    }
    candidates.sort();
    candidates.dedup();
    eprintln!("{} addresses from the DNS seeds", candidates.len());
    let candidates = Rc::new(RefCell::new(candidates));

    let network = args.network;
    let slots = (0..args.peers).map(|slot| {
        let recorder = recorder.clone();
        let candidates = candidates.clone();
        async move {
            loop {
                let addr = {
                    let mut c = candidates.borrow_mut();
                    if c.is_empty() {
                        return;
                    }
                    // spread the slots over the list
                    let i = (slot * 7919) % c.len();
                    c.swap_remove(i)
                };
                match record_from(addr, network, &recorder).await {
                    Ok(()) => {}
                    Err(e) => eprintln!("{addr}: {e}"),
                }
            }
        }
    });
    let report = async {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let r = recorder.borrow();
            eprintln!(
                "{} min: {} transactions ({} kB), {} blocks ({} kB)",
                r.start.elapsed().as_secs() / 60,
                r.txs.len(),
                r.tx_bytes / 1000,
                r.blocks.len(),
                r.block_bytes / 1000
            );
        }
    };
    let all = async {
        tokio::join!(futures_join(slots.collect()), report);
    };
    let _ = tokio::time::timeout(Duration::from_secs(args.minutes * 60), all).await;
    let mut r = recorder.borrow_mut();
    if r.out.flush().is_err() {
        return ExitCode::FAILURE;
    }
    eprintln!(
        "recorded {} transactions ({} kB) and {} blocks ({} kB) into {}",
        r.txs.len(),
        r.tx_bytes / 1000,
        r.blocks.len(),
        r.block_bytes / 1000,
        args.out.display()
    );
    ExitCode::SUCCESS
}

/// Runs the futures together, in this task.
async fn futures_join<F: std::future::Future<Output = ()>>(futures: Vec<F>) {
    let mut futures: Vec<_> = futures.into_iter().map(Box::pin).collect();
    std::future::poll_fn(|cx| {
        futures.retain_mut(|f| f.as_mut().poll(cx).is_pending());
        if futures.is_empty() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await
}

/// Records from the peer at `addr` until it leaves.
async fn record_from(
    addr: SocketAddr,
    network: Network,
    recorder: &Rc<RefCell<Recorder>>,
) -> Result<(), String> {
    let stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
        .await
        .map_err(|_| "no connection in 10 s".to_string())?
        .map_err(|e| e.to_string())?;
    let mut conn = Connection::new(FromTokio::new(stream), network);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    conn.send(&Message::Version(Version {
        version: PROTOCOL_VERSION,
        services: ecsn::p2p::services::WITNESS,
        timestamp: now,
        receiver: NetAddress::default(),
        sender: NetAddress::default(),
        nonce: now as u64 ^ 0x6d70_6c72,
        user_agent: "/ecsn-mempool:0.1.0/".into(),
        start_height: 0,
        relay: true,
    }))
    .await
    .map_err(|e| format!("{e:?}"))?;
    loop {
        let message = conn.recv().await.map_err(|e| format!("{e:?}"))?;
        match message {
            Message::Version(v) => {
                eprintln!("{addr}: {}", v.user_agent);
                conn.send(&Message::Verack)
                    .await
                    .map_err(|e| format!("{e:?}"))?;
            }
            Message::Ping(nonce) => {
                conn.send(&Message::Pong(nonce))
                    .await
                    .map_err(|e| format!("{e:?}"))?;
            }
            Message::Inv(list) => {
                let wanted: Vec<Inventory> = {
                    let r = recorder.borrow();
                    list.iter()
                        .filter_map(|i| match i.kind {
                            inv::TX if !r.txs.contains(&i.hash) => Some(Inventory {
                                kind: inv::TX | inv::WITNESS_FLAG,
                                hash: i.hash,
                            }),
                            inv::BLOCK if !r.blocks.contains(&i.hash) => Some(Inventory {
                                kind: inv::BLOCK | inv::WITNESS_FLAG,
                                hash: i.hash,
                            }),
                            _ => None,
                        })
                        .collect()
                };
                if !wanted.is_empty() {
                    conn.send(&Message::GetData(wanted))
                        .await
                        .map_err(|e| format!("{e:?}"))?;
                }
            }
            Message::Other { command, payload } if command.starts_with(b"tx\0") => {
                let Ok(tx) = deserialize::<Transaction>(&payload) else {
                    continue;
                };
                let txid = bitcoin::hashes::Hash::to_byte_array(tx.compute_txid());
                let mut r = recorder.borrow_mut();
                if r.txs.insert(txid) {
                    r.tx_bytes += payload.len() as u64;
                    r.write(TX, &payload).map_err(|e| e.to_string())?;
                }
            }
            Message::Block(block) => {
                let hash = bitcoin::hashes::Hash::to_byte_array(block.block_hash());
                let mut r = recorder.borrow_mut();
                if r.blocks.insert(hash) {
                    let bytes = serialize(&block);
                    eprintln!(
                        "block {} ({} kB, {} transactions)",
                        block.block_hash(),
                        bytes.len() / 1000,
                        block.txdata.len()
                    );
                    r.block_bytes += bytes.len() as u64;
                    r.write(BLOCK, &bytes).map_err(|e| e.to_string())?;
                }
            }
            _ => {}
        }
    }
}
