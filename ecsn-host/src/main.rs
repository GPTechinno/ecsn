//! Runs the ECSN node over TCP. Logs go to stderr (`RUST_LOG`, `info` by default).
//!
//! Without `--follow` it syncs from one peer and exits, the validated tip and its accumulator on
//! stdout. With `--follow` it keeps following the chain: the peer is the bridge tried first, others
//! come from the DNS seeds and the peers' addresses, and `--peers` ordinary peers announce the new
//! blocks; the connections are logged every minute.
//!
//! The chain is kept in RAM, or with `--store` on the flash chain store, in a file holding a
//! flash image.
//!
//! The heap's peak is logged as it grows by a MiB (as the firmware does), and `--heap-limit`
//! fails the allocations beyond a size, as a board's RAM does.

mod file_flash;
mod heap;

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitcoin::Network;
use clap::Parser;
use ecsn::node::{Clock, Config, Dialer, Node, Resolver, Role, Source};
use ecsn::p2p::{default_port, Connection};
use ecsn::store::{FlashChainStore, MemChainStore};
use embedded_io_adapters::tokio_1::FromTokio;
use file_flash::FileFlash;
use floresta_chain::{BlockchainInterface, ChainParams, ChainStore};
use futures::future::join_all;
use tokio::net::TcpStream;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[global_allocator]
static HEAP: heap::Counted = heap::Counted::new();

#[derive(Parser)]
struct Args {
    /// bitcoin, testnet, testnet4, signet or regtest
    #[arg(long, default_value = "regtest")]
    network: Network,
    /// The peer, host:port (default: localhost on the network's port)
    #[arg(long)]
    peer: Option<String>,
    /// Start from floresta-chain's accumulator of the network
    #[arg(long)]
    assume_utreexo: bool,
    /// Keep following the chain after the sync: the peer is then the bridge tried first
    #[arg(long)]
    follow: bool,
    /// Ordinary peers to keep connected with `--follow`, for new blocks and addresses
    #[arg(long, default_value_t = 2)]
    peers: usize,
    /// Reach peers over IPv6 too
    #[arg(long)]
    ipv6: bool,
    /// Never the v2 transport (BIP 324), tried by default with the peers that speak it
    #[arg(long)]
    v1: bool,
    /// Without `--follow`: the v2 transport with the peer
    #[arg(long, conflicts_with = "v1")]
    v2: bool,
    /// Keep the chain in this flash image (created, and formatted, if needed) instead of in RAM
    #[arg(long)]
    store: Option<PathBuf>,
    /// The size of a new `--store` flash image, in MiB
    #[arg(long, default_value_t = 128)]
    store_size: u32,
    /// Blocks asked for at once while syncing
    #[arg(long, default_value_t = 16)]
    blocks_in_flight: usize,
    /// Fail the allocations that would take the heap beyond this many MiB, as a board's RAM does
    #[arg(long, value_name = "MIB")]
    heap_limit: Option<usize>,
    /// With `--follow`: the most unconfirmed transactions the mempool holds, MB of their
    /// serialized bytes (4 is a fair size); 0, the default, for no mempool. A utreexod 0.5.1
    /// bridge changes its own copy of each transaction it relays this way (see the README)
    #[arg(long, value_name = "MB", default_value_t = 0)]
    mempool: usize,
    /// Check no script: by default the blocks' and the mempool's are checked with Bitcoin Core's
    /// script interpreter (libbitcoinconsensus)
    #[arg(long)]
    no_scripts: bool,
    /// With `--follow`: keep the address book in this file across runs (read at the start,
    /// written each minute it changed)
    #[arg(long)]
    book: Option<PathBuf>,
}

/// The most bytes of a saved address book.
const BOOK_BYTES: usize = 64 * 1024;

/// TCP connections, given 10 s to open.
struct TcpDialer;

impl Dialer for TcpDialer {
    type Io<'a> = FromTokio<TcpStream>;

    async fn connect(&mut self, addr: SocketAddr) -> Option<Self::Io<'_>> {
        let stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
            .await
            .ok()?
            .ok()?;
        let _ = stream.set_nodelay(true);
        Some(FromTokio::new(stream))
    }

    fn random(&mut self) -> [u8; 64] {
        random()
    }
}

/// 64 random bytes from the system.
fn random() -> [u8; 64] {
    use std::io::Read;
    let mut bytes = [0; 64];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("/dev/urandom");
    bytes
}

struct TokioClock(Instant);

impl Clock for TokioClock {
    fn now(&self) -> u64 {
        self.0.elapsed().as_secs()
    }

    fn unix(&self) -> i64 {
        unix_now()
    }

    async fn sleep(&self, seconds: u64) {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }
}

/// The system's resolver.
struct SystemResolver;

impl Resolver for SystemResolver {
    async fn resolve(&mut self, name: &str) -> Vec<IpAddr> {
        match tokio::net::lookup_host((name, 0)).await {
            Ok(addrs) => addrs.map(|a| a.ip()).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// The bytes left under the limit, as one block: the system's allocator does not fragment it.
fn heap_free() -> (usize, usize) {
    (HEAP.free(), HEAP.free())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    if let Some(mib) = args.heap_limit {
        HEAP.set_limit(mib << 20);
    }
    // The heap's peak, looked at between the node's steps, as the firmware's guard does; the
    // peak counts the allocations made inside a step too.
    tokio::spawn(async {
        let mut logged = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let peak = HEAP.peak();
            if peak >= logged + (1 << 20) {
                logged = peak;
                info!(
                    "heap: {} KiB at most, {} KiB now",
                    peak >> 10,
                    HEAP.used() >> 10
                );
            }
        }
    });
    let config = Config {
        network: args.network,
        user_agent: concat!("/ecsn:", env!("CARGO_PKG_VERSION"), "/").into(),
        assume_utreexo: args
            .assume_utreexo
            .then(|| ChainParams::get_assume_utreexo(args.network)),
        blocks_in_flight: args.blocks_in_flight,
        ipv6: args.ipv6,
        v2: !args.v1,
        v2_bridge: !args.v1,
        // the blocks in flight fit the heap left under the limit, as on a board
        free_memory: args.heap_limit.map(|_| heap_free as fn() -> (usize, usize)),
        mempool: if args.follow {
            args.mempool * 1_000_000
        } else {
            0
        },
        verify_scripts: (!args.no_scripts)
            .then_some(ecsn_consensus::verify_scripts as ecsn::scripts::VerifyScripts),
    };

    let Some(path) = &args.store else {
        return run(&args, MemChainStore::new(), config).await;
    };
    let flash = match FileFlash::open(path, args.store_size << 20) {
        Ok(flash) => flash,
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    match FlashChainStore::mount(flash, args.network, true) {
        Ok((store, formatted)) => {
            let what = if formatted { "formatted" } else { "mounted" };
            eprintln!(
                "{}: {what}, {} headers stored, room for {}",
                path.display(),
                store.records(),
                store.layout().records
            );
            run(&args, store, config).await
        }
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}

async fn run<S: ChainStore>(args: &Args, store: S, config: Config) -> ExitCode {
    let peer = args
        .peer
        .clone()
        .unwrap_or_else(|| format!("127.0.0.1:{}", default_port(args.network)));
    let nonce = RandomState::new().build_hasher().finish();
    let node = match Node::new(store, config, nonce) {
        Ok(node) => node,
        Err(e) => {
            eprintln!("chain: {e:?}");
            return ExitCode::FAILURE;
        }
    };

    if args.follow {
        return follow(args, &node, &peer).await;
    }

    let stream = match TcpStream::connect(&peer).await {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("{peer}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _ = stream.set_nodelay(true);
    let mut conn = Connection::new(FromTokio::new(stream), args.network);
    if args.v2 {
        if let Err(e) = conn.start_v2(bip324::Role::Initiator, random()).await {
            eprintln!("{peer}: v2 handshake: {e:?}");
            return ExitCode::FAILURE;
        }
        info!("{peer}: v2 transport");
    }
    let result = match node.handshake(&mut conn, unix_now()).await {
        Ok(_) => node.sync(&mut conn).await.map(|_| ()),
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        eprintln!("{peer}: {e:?}");
        return ExitCode::FAILURE;
    }

    let chain = node.chain();
    let (height, hash) = chain.get_best_block().expect("the best block");
    let acc = chain.acc();
    println!("height {height}");
    println!("hash {hash}");
    println!("leaves {}", acc.leaves);
    for root in &acc.roots {
        println!("root {root}");
    }
    ExitCode::SUCCESS
}

/// Follows the chain until stopped: the bridge's slot (`peer` tried first), the ordinary peers'
/// slots, the DNS seeds, and the connections logged every minute.
async fn follow<S: ChainStore>(args: &Args, node: &Node<S>, peer: &str) -> ExitCode {
    if let Some(path) = &args.book {
        if let Ok(saved) = std::fs::read(path) {
            let added = node.load_book(&saved);
            info!("{}: {added} addresses", path.display());
        }
    }
    match tokio::net::lookup_host(peer).await {
        Ok(addrs) => {
            for addr in addrs.filter(|a| args.ipv6 || a.is_ipv4()) {
                node.add_address(addr, 0, Source::Configured);
            }
        }
        Err(e) => {
            eprintln!("{peer}: {e}");
            return ExitCode::FAILURE;
        }
    }
    let clock = TokioClock(Instant::now());
    let mut dialers: Vec<TcpDialer> = (0..=args.peers).map(|_| TcpDialer).collect();
    let slots = dialers.iter_mut().enumerate().map(|(slot, dialer)| {
        let role = if slot == 0 { Role::Bridge } else { Role::Peer };
        node.run_slot(slot, role, dialer, &clock)
    });
    let mut resolver = SystemResolver;
    let report = async {
        let mut book_saved = node.book_changes();
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if let Some(path) = &args.book {
                if node.book_changes() != book_saved {
                    book_saved = node.book_changes();
                    if let Err(e) = std::fs::write(path, node.book_bytes(BOOK_BYTES)) {
                        warn!("{}: {e}", path.display());
                    }
                }
            }
            let status = node.status();
            let chain = node.chain();
            let validated = chain.get_validation_index().unwrap_or(0);
            let best = chain.get_best_block().map_or(0, |(h, _)| h);
            let mut line = format!(
                "validated {validated} of {best}, {:?}; {} addresses ({} bridges);",
                status.phase, status.known, status.known_bridges
            );
            if node.config().mempool > 0 {
                let m = status.mempool;
                line += &format!(
                    " mempool {} tx ({} kB, {} sat of fees, from {} sat/kvB), {} orphans, {} taken, {} refused;",
                    m.transactions,
                    m.bytes / 1000,
                    m.fees,
                    m.min_rate,
                    m.orphans,
                    status.taken,
                    status.refused
                );
            }
            for p in &status.peers {
                let role = match p.role {
                    Role::Bridge => "bridge",
                    Role::Peer => "peer",
                };
                let addr = p.addr.map_or_else(|| "?".into(), |a| a.to_string());
                let v2 = if p.v2 { " v2" } else { "" };
                match &p.info {
                    Some(info) => line += &format!(" {role} {addr} {}{v2}", info.user_agent),
                    None => line += &format!(" {role} {addr} (connecting)"),
                }
            }
            info!("{line}");
        }
    };
    tokio::join!(
        join_all(slots),
        node.discover(&mut resolver, &clock),
        report
    );
    ExitCode::SUCCESS
}
