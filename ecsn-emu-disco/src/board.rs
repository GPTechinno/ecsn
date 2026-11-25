//! The STM32H747I-DISCO around the chip: the 32 MB SDRAM on FMC bank 2, the
//! two MT25QL512 on the QUADSPI (128 MB in dual-flash mode, kept in a file
//! across runs if asked), the 25 MHz HSE, the LAN8742A PHY and a network
//! behind it (a router with DHCP, DNS and NAT to the host: the gateway
//! 10.0.2.2 is the host's 127.0.0.1), the four user LEDs, and the
//! firmware's defmt logs.

use std::path::{Path, PathBuf};

use esp32s3_rax::machine::TICK_HZ;
use esp32s3_rax::net::nat::{Router, RouterConfig};
use stm32h7_rax::{Config, Event, Stm32h7};

use crate::logs::Logs;
use crate::phy::Lan8742;

const SDRAM: (u32, usize) = (0xD000_0000, 32 << 20);
const QSPI_BYTES: usize = 128 << 20;
const HSE_HZ: u32 = 25_000_000;
const BOOT: u32 = 0x0800_0000;
/// GPIOI: the user LEDs LD1 (green) to LD4 (blue) on PI12 to PI15, lit low.
const LED_PORT: usize = 8;
const LED_PINS: [u32; 4] = [12, 13, 14, 15];
const PS_PER_S: u64 = 1_000_000_000_000;

pub struct Disco {
    pub machine: Stm32h7,
    logs: Logs,
    router: Router,
    pub leds: [bool; 4],
    qspi_path: Option<PathBuf>,
}

/// What came out of a run.
#[derive(Default)]
pub struct Output {
    /// Firmware logs, emulator notes and network events, in order.
    pub lines: Vec<String>,
    pub event: Option<Event>,
}

impl Disco {
    /// The board with the firmware `elf`, and the QSPI flash from `qspi` (an
    /// erased flash if the file does not exist; saved there by
    /// [`Disco::save_qspi`]).
    pub fn new(elf: &[u8], qspi: Option<&Path>) -> Result<Self, String> {
        let flash = match qspi {
            Some(path) if path.exists() => {
                let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
                if bytes.len() != QSPI_BYTES {
                    return Err(format!("{}: not a 128 MiB flash image", path.display()));
                }
                bytes
            }
            _ => vec![0xFF; QSPI_BYTES],
        };
        let config = Config {
            hse_hz: HSE_HZ,
            uid: *b"ECSN-DISCO-1",
            external_ram: vec![SDRAM],
            qspi_flash: flash,
            qspi_dual: true,
            boot_address: BOOT,
            seed: 0x5eed_ec5e,
        };
        let machine = Stm32h7::new(config, elf)?;
        machine.periph().eth.phy = Some(Box::new(Lan8742::new()));
        Ok(Self {
            machine,
            logs: Logs::new(elf)?,
            router: Router::new(RouterConfig::default(), 0),
            leds: [false; 4],
            qspi_path: qspi.map(Path::to_path_buf),
        })
    }

    /// The router's time: ticks of `TICK_HZ`.
    fn ticks(&self) -> u64 {
        (u128::from(self.machine.now()) * u128::from(TICK_HZ) / u128::from(PS_PER_S)) as u64
    }

    /// Runs `duration` picoseconds of virtual time, then the board.
    pub fn run(&mut self, duration: u64) -> Output {
        let mut out = Output {
            event: self.machine.run_for(duration),
            ..Output::default()
        };
        if let Some(rtt) = self.logs.rtt() {
            let bytes = self.machine.rtt_read(rtt, 0);
            out.lines.extend(self.logs.decode(&bytes));
        }
        let now = self.ticks();
        let sent: Vec<Vec<u8>> = {
            let mut p = self.machine.periph();
            out.lines
                .extend(p.log.drain(..).map(|l| format!("emu: {l}")));
            out.lines.extend(
                p.quadspi
                    .flash
                    .warnings
                    .drain(..)
                    .map(|w| format!("emu: QSPI flash: {w}")),
            );
            let gpio = &p.gpio[LED_PORT];
            for (led, &pin) in self.leds.iter_mut().zip(LED_PINS.iter()) {
                *led = gpio.mode(pin) == 1 && !gpio.level(pin);
            }
            p.eth.outgoing.drain(..).collect()
        };
        for frame in &sent {
            self.router.input(frame, now);
        }
        self.router.poll(now);
        out.lines
            .extend(self.router.events.drain(..).map(|e| format!("net: {e}")));
        let frames: Vec<Vec<u8>> = self.router.out.drain(..).collect();
        if !frames.is_empty() {
            self.machine.periph().eth.incoming.extend(frames);
        }
        out
    }

    /// Saves the QSPI flash where it was read from.
    pub fn save_qspi(&self) -> std::io::Result<()> {
        if let Some(path) = &self.qspi_path {
            std::fs::write(path, &self.machine.periph().quadspi.flash.bytes)?;
        }
        Ok(())
    }
}
