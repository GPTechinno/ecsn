//! The ECSN node on an STM32H747I-DISCO (Cortex-M7 core): 400 MHz, the 32 MB SDRAM as the heap,
//! Ethernet with DHCP, then the node of `ecsn` against one peer, reconnecting when the connection
//! ends. The chain stays in RAM across connections and is lost on reset.
//!
//! Set at build time:
//! - `ECSN_PEER` (required): the peer, `ip:port`, for example `192.168.1.10:18444`;
//! - `ECSN_NETWORK`: `bitcoin`, `testnet`, `testnet4`, `signet` or `regtest` (the default);
//! - `ECSN_ASSUME_UTREEXO`: set to start from `floresta-chain`'s accumulator of the network;
//! - `ECSN_LOG`: the most detailed log level of the node (`info` by default).
//!
//! LD1 (green) blinks while the firmware runs, LD2 (orange) is on while connected to the peer.
//!
//! Ethernet needs the board modified: with the default solder bridges, PC1 is both ETH_MDC and
//! SAI4_D1 of the digital microphones.

#![no_std]
#![no_main]

extern crate alloc;

mod log;
mod sdram;

use core::mem::MaybeUninit;
use core::net::SocketAddrV4;
use core::str::FromStr;

use bitcoin::Network;
use defmt::{error, info, unwrap, warn, Debug2Format};
use ecsn::node::{Config, Node};
use ecsn::p2p::Connection;
use ecsn::store::MemChainStore;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::StackResources;
use embassy_stm32::eth::{Ethernet, GenericPhy, PacketQueue, Sma};
use embassy_stm32::fmc::Fmc;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::peripherals::{ETH, ETH_SMA};
use embassy_stm32::rng::{self, Rng};
use embassy_stm32::{bind_interrupts, eth, peripherals, SharedData};
use embassy_time::{Delay, Duration, Timer};
use embedded_alloc::TlsfHeap;
use floresta_chain::{BlockchainInterface, ChainParams};
use static_cell::StaticCell;
use stm32_fmc::devices::is42s32800g_6::Is42s32800g;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    ETH => eth::InterruptHandler;
    RNG => rng::InterruptHandler<peripherals::RNG>;
});

#[global_allocator]
static HEAP: TlsfHeap = TlsfHeap::empty();

/// The clock configuration, shared with the Cortex-M4 core.
#[unsafe(link_section = ".ram_d3.shared_data")]
static SHARED_DATA: MaybeUninit<SharedData> = MaybeUninit::uninit();

const PEER: &str = env!(
    "ECSN_PEER",
    "set ECSN_PEER to the peer's address, for example ECSN_PEER=192.168.1.10:18444"
);
const NETWORK: Option<&str> = option_env!("ECSN_NETWORK");
const ASSUME_UTREEXO: bool = option_env!("ECSN_ASSUME_UTREEXO").is_some();
/// The board has no clock set: the build's time, close enough for the peer's time offset.
const BUILD_TIME: &str = env!("ECSN_BUILD_TIME");

type Device = Ethernet<'static, ETH, GenericPhy<Sma<'static, ETH_SMA>>>;

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

fn network() -> Network {
    match NETWORK.unwrap_or("regtest") {
        "bitcoin" => Network::Bitcoin,
        "testnet" => Network::Testnet,
        "testnet4" => Network::Testnet4,
        "signet" => Network::Signet,
        "regtest" => Network::Regtest,
        other => defmt::panic!("ECSN_NETWORK: unknown network {=str}", other),
    }
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
        config.rcc.sys = Sysclk::PLL1_P;
        config.rcc.ahb_pre = AHBPrescaler::DIV2; // 200 MHz, the FMC's clock: SDRAM at 100 MHz
        config.rcc.apb1_pre = APBPrescaler::DIV2;
        config.rcc.apb2_pre = APBPrescaler::DIV2;
        config.rcc.apb3_pre = APBPrescaler::DIV2;
        config.rcc.apb4_pre = APBPrescaler::DIV2;
        config.rcc.voltage_scale = VoltageScale::Scale1;
    }
    let p = embassy_stm32::init_primary(config, &SHARED_DATA);
    info!("ECSN {=str}", env!("CARGO_PKG_VERSION"));

    let mut core = unwrap!(cortex_m::Peripherals::take());
    core.SCB.enable_icache();

    spawner.spawn(unwrap!(blink(Output::new(p.PI12, Level::High, Speed::Low))));
    let mut connected = Output::new(p.PI13, Level::High, Speed::Low);

    // SDRAM, then the heap in it
    sdram::map(&mut core.MPU, &mut core.SCB);
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

    let mut rng = Rng::new(p.RNG, Irqs);
    let seed = rng.next_u64();
    let nonce = rng.next_u64();

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
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        device,
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.spawn(unwrap!(net_task(runner)));
    info!("Ethernet: MAC {:02x}, waiting for DHCP", mac);
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
    };
    let node = match Node::new(MemChainStore::new(), config, nonce) {
        Ok(node) => node,
        Err(e) => defmt::panic!("chain: {}", Debug2Format(&e)),
    };
    let now: i64 = BUILD_TIME.parse().unwrap_or(0);

    static RX: StaticCell<[u8; 32 * 1024]> = StaticCell::new();
    static TX: StaticCell<[u8; 8 * 1024]> = StaticCell::new();
    let rx = RX.init([0; 32 * 1024]);
    let tx = TX.init([0; 8 * 1024]);
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx[..], &mut tx[..]);
        // the peer pings every few minutes
        socket.set_timeout(Some(Duration::from_secs(600)));
        socket.set_keep_alive(Some(Duration::from_secs(60)));
        info!("peer {}: connecting", Debug2Format(&peer));
        if let Err(e) = socket.connect(peer).await {
            warn!("peer {}: {}", Debug2Format(&peer), e);
        } else {
            connected.set_low();
            let mut conn = Connection::new(socket, network);
            if let Err(e) = node.run(&mut conn, now).await {
                error!("peer {}: {}", Debug2Format(&peer), Debug2Format(&e));
            }
            connected.set_high();
        }
        if let Ok((height, hash)) = node.chain().get_best_block() {
            info!(
                "validated {=u32}, best header {=u32} {}",
                node.chain().get_validation_index().unwrap_or(0),
                height,
                Debug2Format(&hash)
            );
        }
        Timer::after_secs(5).await;
    }
}
