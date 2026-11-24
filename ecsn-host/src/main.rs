//! Runs the ECSN node against one peer over TCP. Logs go to stderr (`RUST_LOG`, `info` by
//! default); at the end the validated tip and its accumulator go to stdout.
//!
//! The chain is kept in RAM, or with `--store` on the flash chain store, in a file holding a
//! flash image.

mod file_flash;

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use bitcoin::Network;
use clap::Parser;
use ecsn::node::{Config, Node};
use ecsn::p2p::Connection;
use ecsn::store::{FlashChainStore, MemChainStore};
use embedded_io_adapters::tokio_1::FromTokio;
use file_flash::FileFlash;
use floresta_chain::{BlockchainInterface, ChainParams, ChainStore};
use tokio::net::TcpStream;
use tracing_subscriber::EnvFilter;

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
    /// Keep following the peer's new blocks after the sync
    #[arg(long)]
    follow: bool,
    /// Keep the chain in this flash image (created, and formatted, if needed) instead of in RAM
    #[arg(long)]
    store: Option<PathBuf>,
    /// The size of a new `--store` flash image, in MiB
    #[arg(long, default_value_t = 128)]
    store_size: u32,
    /// Blocks asked for at once while syncing
    #[arg(long, default_value_t = 16)]
    blocks_in_flight: usize,
}

fn default_port(network: Network) -> u16 {
    match network {
        Network::Bitcoin => 8333,
        Network::Testnet => 18333,
        Network::Testnet4 => 48333,
        Network::Signet => 38333,
        _ => 18444,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let config = Config {
        network: args.network,
        user_agent: concat!("/ecsn:", env!("CARGO_PKG_VERSION"), "/").into(),
        assume_utreexo: args
            .assume_utreexo
            .then(|| ChainParams::get_assume_utreexo(args.network)),
        blocks_in_flight: args.blocks_in_flight,
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

    let stream = match TcpStream::connect(&peer).await {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("{peer}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _ = stream.set_nodelay(true);
    let mut conn = Connection::new(FromTokio::new(stream), args.network);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);

    let result = if args.follow {
        node.run(&mut conn, now).await
    } else {
        match node.handshake(&mut conn, now).await {
            Ok(_) => node.sync(&mut conn).await.map(|_| ()),
            Err(e) => Err(e),
        }
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
