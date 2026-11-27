//! The ECSN node on an STM32H747I-DISCO (Cortex-M7 core): 400 MHz, the 32 MB SDRAM as the heap,
//! Ethernet with DHCP, then the node of `ecsn`: a connection to a Utreexo bridge (the configured
//! peer first, others the node learns of after it), a few to ordinary peers for new blocks and
//! addresses, each reconnecting to another address when it ends, and the network's DNS seeds. The
//! chain is kept on the 128 MB QSPI flash, formatted at the first start (which erases what the
//! flash holds, in a few minutes).
//!
//! Set at build time:
//! - `ECSN_PEER` (required): the bridge tried first, `ip:port`, for example `192.168.1.10:18444`;
//! - `ECSN_PEERS`: ordinary peers kept connected, 2 by default;
//! - `ECSN_V1`: set to stay in the v1 transport, the v2 (BIP 324, encrypted) being tried with the
//!   peers that speak it;
//! - `ECSN_V1_BRIDGE`: set to keep the bridge alone in the v1 transport, its blocks then not
//!   decrypted (about 6 % of the core while syncing);
//! - `ECSN_NETWORK`: `bitcoin`, `testnet`, `testnet4`, `signet` or `regtest` (the default);
//! - `ECSN_ASSUME_UTREEXO`: set to start from `floresta-chain`'s accumulator of the network;
//! - `ECSN_RAM_STORE`: set to keep the chain in RAM instead, lost on reset;
//! - `ECSN_BLOCKS_IN_FLIGHT`: blocks asked for at once while syncing, 4 by default (each is held
//!   in RAM until validated, up to a few megabytes on mainnet);
//! - `ECSN_LOG`: the most detailed log level of the node (`info` by default);
//! - `ECSN_NTP`: the SNTP server, a name or an IPv4 address, `pool.ntp.org` by default.
//!
//! The time of day comes from the SNTP server, asked at the start and every 5 minutes (every 30 s
//! until it first answers). Until it answers, the display shows the peer's time, from its
//! `version` message, and the node's own `version` message carries the build's time.
//!
//! At the start the firmware logs what reset the chip (the RCC's reset flags). The independent
//! watchdog (IWDG1, 32 s) is armed once the node reaches the block sync, past the one-off steps
//! that hold the core longer (marking the chain as assumed); from then on a core held 32 s resets
//! the board, which resumes from the flash. The executor's stalls and the heap's peak are logged
//! as they grow.
//!
//! LD1 (green) blinks while the firmware runs, LD2 (orange) is on while connected to a bridge.
//! The display shows the step the firmware is at while it starts, then the node's state, redrawn
//! every second.
//!
//! Ethernet needs the board modified: with the default solder bridges, PC1 is both ETH_MDC and
//! SAI4_D1 of the digital microphones.

#![no_std]
#![no_main]

extern crate alloc;

mod display;
mod gui;
mod heap;
mod log;
mod mpu;
mod qspi;
mod screen;
mod sdram;
mod text;

use alloc::format;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::mem::MaybeUninit;
use core::net::{IpAddr, SocketAddr, SocketAddrV4};
use core::str::FromStr;

use bitcoin::Network;
use core::net::Ipv4Addr;
use defmt::{info, unwrap, warn, Debug2Format};
use display::Display;
use ecsn::node::{Clock, Config, Dialer, Node, Phase, Resolver, Role, Source};
use ecsn::store::{FlashChainStore, MemChainStore};
use embassy_executor::Spawner;

use ecsn::sntp;
use embassy_futures::join::{join, join5, join_array};
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, Stack, StackResources};
use embassy_stm32::eth::{Ethernet, GenericPhy, PacketQueue, Sma};
use embassy_stm32::fmc::Fmc;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::peripherals::{ETH, ETH_SMA, IWDG1};
use embassy_stm32::rng::{self, Rng};
use embassy_stm32::wdg::IndependentWatchdog;
use embassy_stm32::{bind_interrupts, eth, peripherals, SharedData};
use embassy_time::{with_timeout, Delay, Duration, Instant, Timer};
use floresta_chain::{BlockchainInterface, ChainParams, ChainStore};
use heap::Heap;
use screen::Screen;
use static_cell::StaticCell;
use stm32_fmc::devices::is42s32800g_6::Is42s32800g;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    ETH => eth::InterruptHandler;
    RNG => rng::InterruptHandler<peripherals::RNG>;
});

#[global_allocator]
static HEAP: Heap = Heap::empty();

/// The clock configuration, shared with the Cortex-M4 core.
#[unsafe(link_section = ".ram_d3.shared_data")]
static SHARED_DATA: MaybeUninit<SharedData> = MaybeUninit::uninit();

const PEER: &str = env!(
    "ECSN_PEER",
    "set ECSN_PEER to the peer's address, for example ECSN_PEER=192.168.1.10:18444"
);
const NETWORK: Option<&str> = option_env!("ECSN_NETWORK");
const ASSUME_UTREEXO: bool = option_env!("ECSN_ASSUME_UTREEXO").is_some();
const RAM_STORE: bool = option_env!("ECSN_RAM_STORE").is_some();
const BLOCKS_IN_FLIGHT: Option<&str> = option_env!("ECSN_BLOCKS_IN_FLIGHT");
const V1: bool = option_env!("ECSN_V1").is_some();
const V1_BRIDGE: bool = option_env!("ECSN_V1_BRIDGE").is_some();
/// The ordinary peers kept connected.
const PEERS: usize = match option_env!("ECSN_PEERS") {
    Some(n) => parse_count(n),
    None => 2,
};
/// The socket buffers of an ordinary peer: headers and addresses, a few kilobytes at a time.
const PEER_RX: usize = 16 * 1024;
const PEER_TX: usize = 4 * 1024;
/// The time the `version` message carries until SNTP answers: the build's, close enough for the
/// peer's time offset.
const BUILD_TIME: &str = env!("ECSN_BUILD_TIME");
/// The SNTP server.
const NTP_SERVER: &str = match option_env!("ECSN_NTP") {
    Some(server) => server,
    None => "pool.ntp.org",
};
/// How often the time is asked for once known, and until then.
const NTP_INTERVAL: Duration = Duration::from_secs(300);
const NTP_RETRY: Duration = Duration::from_secs(30);
/// The watchdog's timeout, the longest IWDG1 allows (32.768 s).
const WATCHDOG_TIMEOUT_US: u32 = 32_000_000;
/// How often the guard looks at the executor, the heap and the node, and pets the watchdog.
const GUARD_PERIOD: Duration = Duration::from_secs(1);

type Device = Ethernet<'static, ETH, GenericPhy<Sma<'static, ETH_SMA>>>;

/// What reset the chip, from the RCC's reset flags, which it then clears for the next start.
fn reset_cause() -> &'static str {
    let rcc = embassy_stm32::pac::RCC;
    let flags = rcc.rsr().read();
    let cause = if flags.iwdg1rstf() {
        "the watchdog"
    } else if flags.sftrstf() {
        "software"
    } else if flags.porrstf() {
        "power-on"
    } else if flags.borrstf() {
        "brown-out"
    } else if flags.pinrstf() {
        "the reset pin"
    } else {
        "unknown"
    };
    rcc.rsr().modify(|w| w.set_rmvf(true));
    cause
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Device>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn blink(mut led: Output<'static>) -> ! {
    loop {
        led.toggle();
        Timer::after_millis(500).await;
    }
}

/// Shows the step the firmware is at on the start screen, with details under it.
async fn show_step(screen: &mut Option<Screen>, step: &str, details: &[&str]) {
    if let Some(screen) = screen {
        let network = NETWORK.unwrap_or("regtest");
        let uptime = Instant::now().as_secs();
        screen
            .draw(|ui: screen::Ui<'_>| gui::start(ui, network, step, details, uptime))
            .await;
    }
}

/// SHA-256's block compression for `bitcoin_hashes` (its `sha256-extern` feature): its software
/// one, behind a symbol of its own, which the emulator runs natively.
///
/// # Safety
///
/// `state` and `block` point to a state and a block that nothing else accesses during the call.
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn bitcoin_hashes_sha256_compress(
    state: *mut [u32; 8],
    block: *const [u8; 64],
) {
    // SAFETY: the caller passes the state and buffer of one of its engines.
    unsafe { bitcoin_hashes::sha256::compress(&mut *state, &*block) }
}

/// A small decimal number, at build time: anything else fails the build.
const fn parse_count(s: &str) -> usize {
    let bytes = s.as_bytes();
    assert!(
        !bytes.is_empty() && bytes.len() <= 2,
        "ECSN_PEERS: a number up to 99"
    );
    let mut n = 0;
    let mut i = 0;
    while i < bytes.len() {
        assert!(bytes[i].is_ascii_digit(), "ECSN_PEERS: a number up to 99");
        n = n * 10 + (bytes[i] - b'0') as usize;
        i += 1;
    }
    n
}

/// SHA-512's block compression for `bitcoin_hashes` 1.1 (its `sha512-extern` feature), the
/// accumulator's hashes: its software one, behind a symbol of its own, which the emulator runs
/// natively.
///
/// # Safety
///
/// `state` and `block` point to a state and a block that nothing else accesses during the call.
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn bitcoin_hashes_sha512_compress(
    state: *mut [u64; 8],
    block: *const [u8; 128],
) {
    // SAFETY: the caller passes the state of one of its engines and a block of its input.
    unsafe { bitcoin_hashes_1::sha512::compress(&mut *state, &*block) }
}

/// The magic of the network named `name`, at build time: an unknown name fails the build.
const fn network_magic(name: &str) -> [u8; 4] {
    match name.as_bytes() {
        b"bitcoin" => [0xF9, 0xBE, 0xB4, 0xD9],
        b"testnet" => [0x0B, 0x11, 0x09, 0x07],
        b"testnet4" => [0x1C, 0x16, 0x3F, 0x28],
        b"signet" => [0x0A, 0x03, 0xCF, 0x40],
        b"regtest" => [0xFA, 0xBF, 0xB5, 0xDA],
        _ => panic!("ECSN_NETWORK: bitcoin, testnet, testnet4, signet or regtest"),
    }
}

/// The magic of the network the firmware is built for, at a symbol of its own: the emulator reads
/// it from the ELF to pick the flash image of that network.
#[unsafe(no_mangle)]
#[used]
static ECSN_NETWORK_MAGIC: [u8; 4] = network_magic(match NETWORK {
    Some(name) => name,
    None => "regtest",
});

fn network() -> Network {
    // read through the symbol, which keeps it in the image
    let magic = unsafe { core::ptr::read_volatile(&ECSN_NETWORK_MAGIC) };
    unwrap!(Network::from_magic(bitcoin::p2p::Magic::from_bytes(magic)))
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = embassy_stm32::Config::default();
    {
        use embassy_stm32::rcc::*;
        config.rcc.supply_config = SupplyConfig::DirectSMPS;
        config.rcc.hse = Some(Hse {
            freq: embassy_stm32::time::Hertz(25_000_000),
            mode: HseMode::Oscillator,
        });
        config.rcc.hsi48 = Some(Default::default()); // the RNG's clock
        config.rcc.pll1 = Some(Pll {
            source: PllSource::HSE,
            prediv: PllPreDiv::DIV5,
            mul: PllMul::MUL160,
            divp: Some(PllDiv::DIV2), // 400 MHz
            divq: Some(PllDiv::DIV4),
            divr: Some(PllDiv::DIV2),
        });
        // the LTDC's pixel clock: 25 / 5 × 132 / 24 = 27.5 MHz
        config.rcc.pll3 = Some(Pll {
            source: PllSource::HSE,
            prediv: PllPreDiv::DIV5,
            mul: PllMul::MUL132,
            divp: Some(PllDiv::DIV2),
            divq: Some(PllDiv::DIV2),
            divr: Some(PllDiv::DIV24),
        });
        config.rcc.sys = Sysclk::PLL1_P;
        config.rcc.ahb_pre = AHBPrescaler::DIV2; // 200 MHz, the FMC's clock: SDRAM at 100 MHz
        config.rcc.apb1_pre = APBPrescaler::DIV2;
        config.rcc.apb2_pre = APBPrescaler::DIV2;
        config.rcc.apb3_pre = APBPrescaler::DIV2;
        config.rcc.apb4_pre = APBPrescaler::DIV2;
        config.rcc.voltage_scale = VoltageScale::Scale1;
    }
    let p = embassy_stm32::init_primary(config, &SHARED_DATA);
    let cause = reset_cause();
    info!("ECSN {=str}", env!("CARGO_PKG_VERSION"));
    if cause == "the watchdog" {
        warn!("reset by {=str}", cause);
    } else {
        info!("reset by {=str}", cause);
    }
    // configured now, armed by the guard (see `follow`)
    let watchdog = IndependentWatchdog::new(p.IWDG1, WATCHDOG_TIMEOUT_US);

    let mut core = unwrap!(cortex_m::Peripherals::take());
    core.SCB.enable_icache();

    spawner.spawn(unwrap!(blink(Output::new(p.PI12, Level::High, Speed::Low))));
    let connected = Output::new(p.PI13, Level::High, Speed::Low);

    // SDRAM, then the heap in it
    mpu::configure(&mut core.MPU, &mut core.SCB);
    let mut sdram = Fmc::sdram_a12bits_d32bits_4banks_bank2(
        p.FMC,
        // A0..A11
        p.PF0,
        p.PF1,
        p.PF2,
        p.PF3,
        p.PF4,
        p.PF5,
        p.PF12,
        p.PF13,
        p.PF14,
        p.PF15,
        p.PG0,
        p.PG1,
        // BA0, BA1
        p.PG4,
        p.PG5,
        // D0..D31
        p.PD14,
        p.PD15,
        p.PD0,
        p.PD1,
        p.PE7,
        p.PE8,
        p.PE9,
        p.PE10,
        p.PE11,
        p.PE12,
        p.PE13,
        p.PE14,
        p.PE15,
        p.PD8,
        p.PD9,
        p.PD10,
        p.PH8,
        p.PH9,
        p.PH10,
        p.PH11,
        p.PH12,
        p.PH13,
        p.PH14,
        p.PH15,
        p.PI0,
        p.PI1,
        p.PI2,
        p.PI3,
        p.PI6,
        p.PI7,
        p.PI9,
        p.PI10,
        // NBL0..NBL3
        p.PE0,
        p.PE1,
        p.PI4,
        p.PI5,
        // SDCKE1, SDCLK, SDNCAS, SDNE1, SDNRAS, SDNWE
        p.PH7,
        p.PG8,
        p.PG15,
        p.PH6,
        p.PF11,
        p.PH5,
        // the IS42S32800J has the timings of the IS42S32800G
        Is42s32800g {},
    );
    let ram = sdram.init(&mut Delay);
    if let Err((offset, read)) = sdram::check(ram) {
        defmt::panic!(
            "SDRAM: wrong word {=u32:#x} at offset {=usize:#x}",
            read,
            offset
        );
    }
    unsafe { HEAP.init(sdram::BASE, sdram::SIZE) };
    info!("SDRAM: {=usize} MB of heap", sdram::SIZE >> 20);
    log::init();

    let mut screen = match Display::new(
        Output::new(p.PG3, Level::High, Speed::Low),
        Output::new(p.PJ12, Level::Low, Speed::Low),
        p.LTDC,
        p.DSIHOST,
    )
    .await
    {
        Ok(display) => {
            info!("display: {}", display.controller);
            Some(Screen::new(display))
        }
        Err(e) => {
            warn!("display: {}", e);
            None
        }
    };

    static RNG: StaticCell<RefCell<Rng<'static, peripherals::RNG>>> = StaticCell::new();
    let rng = RNG.init(RefCell::new(Rng::new(p.RNG, Irqs)));
    let (seed, nonce, ntp_cookie) = {
        let mut rng = rng.borrow_mut();
        (rng.next_u64(), rng.next_u64(), rng.next_u64())
    };

    // a locally administered address from the chip's unique ID
    let uid = embassy_stm32::uid::uid();
    let mac = [0x02, uid[0], uid[1], uid[2], uid[3], uid[4]];
    static PACKETS: StaticCell<PacketQueue<8, 8>> = StaticCell::new();
    let device = Ethernet::new(
        PACKETS.init(PacketQueue::new()),
        p.ETH,
        Irqs,
        p.PA1,  // REF_CLK
        p.PA7,  // CRS_DV
        p.PC4,  // RXD0
        p.PC5,  // RXD1
        p.PG13, // TXD0
        p.PG12, // TXD1
        p.PG11, // TX_EN
        mac,
        p.ETH_SMA,
        p.PA2, // MDIO
        p.PC1, // MDC
    );
    // DHCP, DNS, SNTP's UDP socket, the bridge's TCP socket and the ordinary peers', and one spare
    static RESOURCES: StaticCell<StackResources<{ 5 + PEERS }>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        device,
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.spawn(unwrap!(net_task(runner)));
    info!("Ethernet: MAC {:02x}, waiting for DHCP", mac);
    let mac_line = gui::mac_text(mac);
    show_step(
        &mut screen,
        "Waiting for an address (DHCP)",
        &[mac_line.as_str()],
    )
    .await;
    stack.wait_config_up().await;
    if let Some(v4) = stack.config_v4() {
        info!("Ethernet: {}", v4.address);
    }

    let network = network();
    let peer = match SocketAddrV4::from_str(PEER) {
        Ok(peer) => peer,
        Err(_) => defmt::panic!("ECSN_PEER: not an ip:port: {=str}", PEER),
    };
    let config = Config {
        network,
        user_agent: concat!("/ecsn:", env!("CARGO_PKG_VERSION"), "/").into(),
        assume_utreexo: ASSUME_UTREEXO.then(|| ChainParams::get_assume_utreexo(network)),
        blocks_in_flight: match BLOCKS_IN_FLIGHT.map(str::parse) {
            None => 4,
            Some(Ok(n)) => n,
            Some(Err(_)) => defmt::panic!("ECSN_BLOCKS_IN_FLIGHT: not a number"),
        },
        ipv6: false,
        v2: !V1,
        v2_bridge: !V1_BRIDGE && !V1,
    };
    let link = Link {
        stack,
        peer,
        mac,
        nonce,
        ntp_cookie,
        rng,
        connected,
        watchdog,
    };
    if RAM_STORE {
        info!("chain store: RAM");
        follow(MemChainStore::new(), None, config, link, screen).await
    }

    let flash = match qspi::QspiFlash::new(
        p.QUADSPI, p.PB2, p.PG6, p.PD11, p.PF9, p.PF7, p.PF6, p.PH2, p.PH3, p.PG9, p.PG14,
    ) {
        Ok(flash) => flash,
        Err(e) => defmt::panic!("QSPI flash: {}", e),
    };
    info!("QSPI flash: mounting the chain store (formatting a flash without one takes minutes)");
    // a store of another network is formatted for this one: say so first
    let other = match FlashChainStore::stored_network(&flash) {
        Ok(Some(stored)) if stored != network => Some(stored),
        _ => None,
    };
    let other_line;
    let details: [&str; 2] = match other {
        Some(stored) => {
            warn!(
                "QSPI flash: it holds a chain of {}, formatted now for {}",
                defmt::Display2Format(&stored),
                defmt::Display2Format(&network)
            );
            other_line = format!("the flash holds a chain of {stored}:");
            [
                other_line.as_str(),
                "formatted for this network, a few minutes",
            ]
        }
        None => ["a flash without one is formatted first:", "a few minutes"],
    };
    show_step(&mut screen, "Mounting the chain store", &details).await;
    let store = match FlashChainStore::mount(flash, network, true) {
        Ok((store, formatted)) => {
            info!(
                "QSPI flash: {=str}, {=u32} headers stored, room for {=u32}",
                if formatted { "formatted" } else { "mounted" },
                store.records(),
                store.layout().records
            );
            store
        }
        Err(e) => defmt::panic!("QSPI flash: {}", Debug2Format(&e)),
    };
    let capacity = store.layout().records;
    follow(store, Some(capacity), config, link, screen).await
}

/// The network side of the node: the stack, the peer, what the `version` message needs, the
/// first SNTP cookie, LD2, and the watchdog the guard arms.
struct Link {
    stack: Stack<'static>,
    peer: SocketAddrV4,
    mac: [u8; 6],
    nonce: u64,
    ntp_cookie: u64,
    /// The hardware's random numbers, for the v2 transport's keys.
    rng: &'static RefCell<Rng<'static, peripherals::RNG>>,
    connected: Output<'static>,
    watchdog: IndependentWatchdog<'static, IWDG1>,
}

/// The time of day, from SNTP: Unix milliseconds at an instant of the board's clock.
struct WallClock(Cell<Option<(i64, Instant)>>);

impl WallClock {
    const fn new() -> Self {
        Self(Cell::new(None))
    }

    fn set(&self, unix_ms: i64, at: Instant) {
        self.0.set(Some((unix_ms, at)));
    }

    /// Unix milliseconds now, once SNTP has answered.
    fn now_ms(&self) -> Option<i64> {
        self.0
            .get()
            .map(|(unix_ms, at)| unix_ms + at.elapsed().as_millis() as i64)
    }

    /// Unix seconds now, once SNTP has answered.
    fn now(&self) -> Option<i64> {
        self.now_ms().map(|ms| ms.div_euclid(1000))
    }
}

/// Why the SNTP server gave no time.
enum NtpError {
    /// Its name did not resolve to an IPv4 address.
    Resolve,
    Socket,
    /// No reply within 5 s.
    Timeout,
    Reply(sntp::Error),
}

impl defmt::Format for NtpError {
    fn format(&self, f: defmt::Formatter) {
        match self {
            NtpError::Resolve => defmt::write!(f, "no IPv4 address for the name"),
            NtpError::Socket => defmt::write!(f, "UDP socket error"),
            NtpError::Timeout => defmt::write!(f, "no reply in 5 s"),
            NtpError::Reply(e) => defmt::write!(f, "{}", defmt::Display2Format(e)),
        }
    }
}

/// The node's clock: the board's, and the time of day once SNTP answered (the build's until then).
struct BoardClock<'a> {
    wall: &'a WallClock,
    build_time: i64,
}

impl Clock for BoardClock<'_> {
    fn now(&self) -> u64 {
        Instant::now().as_secs()
    }

    fn unix(&self) -> i64 {
        self.wall.now().unwrap_or(self.build_time)
    }

    async fn sleep(&self, seconds: u64) {
        Timer::after_secs(seconds).await;
    }
}

/// A slot's TCP connections, over its own buffers.
struct TcpDialer {
    stack: Stack<'static>,
    rx: &'static mut [u8],
    tx: &'static mut [u8],
    rng: &'static RefCell<Rng<'static, peripherals::RNG>>,
}

impl Dialer for TcpDialer {
    type Io<'a> = TcpSocket<'a>;

    async fn connect(&mut self, addr: SocketAddr) -> Option<TcpSocket<'_>> {
        let SocketAddr::V4(addr) = addr else {
            return None;
        };
        let mut socket = TcpSocket::new(self.stack, self.rx, self.tx);
        match with_timeout(Duration::from_secs(10), socket.connect(addr)).await {
            Ok(Ok(())) => {}
            _ => return None,
        }
        // the peers ping every few minutes
        socket.set_timeout(Some(Duration::from_secs(600)));
        socket.set_keep_alive(Some(Duration::from_secs(60)));
        Some(socket)
    }

    fn random(&mut self) -> [u8; 64] {
        let mut rng = self.rng.borrow_mut();
        let mut bytes = [0; 64];
        for chunk in bytes.chunks_mut(8) {
            chunk.copy_from_slice(&rng.next_u64().to_le_bytes());
        }
        bytes
    }
}

/// The DNS seeds' names, through the stack's resolver (IPv4).
struct StackResolver(Stack<'static>);

impl Resolver for StackResolver {
    async fn resolve(&mut self, name: &str) -> Vec<IpAddr> {
        match self.0.dns_query(name, DnsQueryType::A).await {
            Ok(addresses) => addresses
                .iter()
                .map(|IpAddress::Ipv4(address)| IpAddr::V4(*address))
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Asks [`NTP_SERVER`] for the time with `cookie`: Unix milliseconds at the returned instant.
async fn ask_time(stack: Stack<'static>, cookie: u64) -> Result<(i64, Instant), NtpError> {
    let server = match Ipv4Addr::from_str(NTP_SERVER) {
        Ok(address) => address,
        Err(_) => {
            let addresses = stack
                .dns_query(NTP_SERVER, DnsQueryType::A)
                .await
                .map_err(|_| NtpError::Resolve)?;
            match addresses.first() {
                Some(IpAddress::Ipv4(address)) => *address,
                None => return Err(NtpError::Resolve),
            }
        }
    };
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx = [0; 2 * sntp::PACKET];
    let mut tx = [0; sntp::PACKET];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    socket.bind(0).map_err(|_| NtpError::Socket)?;
    let sent = Instant::now();
    socket
        .send_to(&sntp::request(cookie), (server, sntp::PORT))
        .await
        .map_err(|_| NtpError::Socket)?;
    let mut packet = [0; sntp::PACKET];
    loop {
        let remaining = Duration::from_secs(5).checked_sub(sent.elapsed());
        let received = with_timeout(
            remaining.unwrap_or(Duration::MIN),
            socket.recv_from(&mut packet),
        )
        .await;
        let (n, _) = received
            .map_err(|_| NtpError::Timeout)?
            .map_err(|_| NtpError::Socket)?;
        let arrived = Instant::now();
        match sntp::reply(&packet[..n], cookie) {
            Ok(reply) => return Ok((reply.arrival((arrived - sent).as_millis()), arrived)),
            // a late reply to an earlier request
            Err(sntp::Error::NotOurs) => continue,
            Err(e) => return Err(NtpError::Reply(e)),
        }
    }
}

/// Runs the node on `store` against the peer, reconnecting when the connection ends, and shows
/// its state on the display. `capacity`: the headers the flash has room for.
async fn follow<S: ChainStore>(
    store: S,
    capacity: Option<u32>,
    config: Config,
    link: Link,
    screen: Option<Screen>,
) -> ! {
    let Link {
        stack,
        peer,
        mac,
        nonce,
        ntp_cookie,
        rng,
        mut connected,
        mut watchdog,
    } = link;
    let build_time: i64 = BUILD_TIME.parse().unwrap_or(0);
    let wall = WallClock::new();
    let node = match Node::new(store, config, nonce) {
        Ok(node) => node,
        Err(e) => defmt::panic!("chain: {}", Debug2Format(&e)),
    };
    node.add_address(SocketAddr::V4(peer), 0, Source::Configured);
    let clock = BoardClock {
        wall: &wall,
        build_time,
    };

    // the bridge's buffers in the internal RAM, the ordinary peers' in the SDRAM
    static RX: StaticCell<[u8; 32 * 1024]> = StaticCell::new();
    static TX: StaticCell<[u8; 8 * 1024]> = StaticCell::new();
    let mut bridge_dialer = TcpDialer {
        stack,
        rx: RX.init([0; 32 * 1024]),
        tx: TX.init([0; 8 * 1024]),
        rng,
    };
    let mut peer_dialers: [TcpDialer; PEERS] = core::array::from_fn(|_| TcpDialer {
        stack,
        rx: alloc::vec![0; PEER_RX].leak(),
        tx: alloc::vec![0; PEER_TX].leak(),
        rng,
    });
    let mut resolver = StackResolver(stack);
    let connections = async {
        let mut slot = 0;
        let peers = peer_dialers.each_mut().map(|dialer| {
            slot += 1;
            node.run_slot(slot, Role::Peer, dialer, &clock)
        });
        join(
            node.run_slot(0, Role::Bridge, &mut bridge_dialer, &clock),
            join_array(peers),
        )
        .await
    };
    let discovery = node.discover(&mut resolver, &clock);
    let time = async {
        let mut cookie = ntp_cookie;
        loop {
            cookie = cookie.wrapping_add(1);
            let wait = match ask_time(stack, cookie).await {
                Ok((unix_ms, at)) => {
                    let ours = wall.now_ms();
                    wall.set(unix_ms, at);
                    let text = gui::date_time(unix_ms.div_euclid(1000));
                    match ours {
                        None => info!("clock: {=str} UTC from {=str}", text.as_str(), NTP_SERVER),
                        Some(ours) => info!(
                            "clock: {=str} UTC from {=str}, ours was {=i64} ms off",
                            text.as_str(),
                            NTP_SERVER,
                            ours - (unix_ms + at.elapsed().as_millis() as i64)
                        ),
                    }
                    NTP_INTERVAL
                }
                Err(e) => {
                    warn!("clock: {=str}: {}", NTP_SERVER, e);
                    NTP_RETRY
                }
            };
            Timer::after(wait).await;
        }
    };
    // The guard: how long the executor was held between two of its looks, the heap's peak, and
    // the watchdog, armed once the node reaches the block sync.
    let guard = async {
        let mut armed = false;
        let mut longest = Duration::from_ticks(0);
        let mut heap_peak = 0;
        let mut last = Instant::now();
        loop {
            Timer::after(GUARD_PERIOD).await;
            let now = Instant::now();
            let stall = (now - last)
                .checked_sub(GUARD_PERIOD)
                .unwrap_or(Duration::from_ticks(0));
            last = now;
            let phase = node.status().phase;
            if stall > longest && stall >= Duration::from_secs(1) {
                longest = stall;
                warn!(
                    "the executor was held {=u64} ms, the longest so far ({})",
                    stall.as_millis(),
                    Debug2Format(&phase)
                );
            }
            if !armed && matches!(phase, Phase::Blocks | Phase::Following) {
                watchdog.unleash();
                armed = true;
                info!("watchdog armed: {=u32} s", WATCHDOG_TIMEOUT_US / 1_000_000);
            }
            if armed {
                watchdog.pet();
            }
            // LD2 while a bridge is connected
            if node.status().bridge().is_some_and(|b| b.info.is_some()) {
                connected.set_low();
            } else {
                connected.set_high();
            }
            let peak = HEAP.peak();
            if peak >= heap_peak + (1 << 20) {
                heap_peak = peak;
                info!(
                    "heap: {=usize} KB allocated at most, {=usize} KB now, {=usize} KB of the SDRAM taken",
                    peak >> 10,
                    HEAP.allocated() >> 10,
                    (sdram::SIZE - HEAP.free()) >> 10
                );
            }
        }
    };
    let dashboard = async {
        let Some(mut screen) = screen else {
            return core::future::pending::<()>().await;
        };
        let network_name = NETWORK.unwrap_or("regtest");
        // the peer's clock at its handshake, and ours then, until SNTP answers
        let mut peer_clock: Option<(i64, Instant)> = None;
        let mut connections = 0;
        loop {
            let status = node.status();
            if status.connections != connections {
                connections = status.connections;
                // the bridge's clock, else another peer's
                let info = status
                    .bridge()
                    .and_then(|b| b.info.as_ref())
                    .or_else(|| status.peers.iter().find_map(|p| p.info.as_ref()));
                if let Some(info) = info {
                    peer_clock = Some((info.time, Instant::now()));
                }
            }
            let chain = node.chain();
            let best_header = chain.get_best_block().map_or(0, |(height, _)| height);
            let validated = chain.get_validation_index().unwrap_or(0);
            let tip = chain.get_block_hash(validated).ok().and_then(|hash| {
                let header = chain.get_block_header(&hash).ok()?;
                Some((format!("{hash}"), header.time))
            });
            let acc = chain.acc();
            let view = gui::Dashboard {
                network: network_name,
                clock: wall
                    .now()
                    .or_else(|| peer_clock.map(|(time, at)| time + at.elapsed().as_secs() as i64)),
                uptime: Instant::now().as_secs(),
                address: stack.config_v4().map(|v4| format!("{}", v4.address)),
                mac,
                status: &status,
                best_header,
                validated,
                tip,
                assumed: node.config().assume_utreexo.as_ref().map(|a| a.height),
                leaves: acc.leaves,
                roots: acc.roots.len(),
                capacity,
                // `TlsfHeap::used` borrows the heap twice and panics
                heap: (sdram::SIZE - HEAP.free(), sdram::SIZE),
            };
            screen
                .draw(|ui: screen::Ui<'_>| gui::dashboard(ui, &view))
                .await;
            Timer::after_secs(1).await;
        }
    };
    join5(connections, discovery, time, guard, dashboard).await;
    defmt::unreachable!()
}
