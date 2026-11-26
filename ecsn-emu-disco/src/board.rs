//! The STM32H747I-DISCO around the chip: the 32 MB SDRAM on FMC bank 2, the
//! two MT25QL512 on the QUADSPI (128 MB in dual-flash mode, kept in a file
//! across runs if asked), the 25 MHz HSE, the LAN8742A PHY and a network
//! behind it (a router with DHCP, DNS and NAT to the host: the gateway
//! 10.0.2.2 is the host's 127.0.0.1), the 4" 800 × 480 display on the DSI
//! link (its reset on PG3, its backlight on PJ12), the four user LEDs, and
//! the firmware's defmt logs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use esp32s3_rax::machine::TICK_HZ;
use esp32s3_rax::net::nat::{Router, RouterConfig};
use stm32h7_rax::devices::ltdc::Frame;
use stm32h7_rax::devices::rcc::RESET_IWDG1;
use stm32h7_rax::rax::isa::arm::common::cpu::ArmCpu;
use stm32h7_rax::rax::isa::arm::cortex_m::CortexMCpu;
use stm32h7_rax::{Config, Event, HostFunction, Stm32h7};

use crate::logs::Logs;
use crate::panel::{Controller, Panel, PanelState};
use crate::phy::Lan8742;

const SDRAM: (u32, usize) = (0xD000_0000, 32 << 20);
const QSPI_BYTES: usize = 128 << 20;
const HSE_HZ: u32 = 25_000_000;
const BOOT: u32 = 0x0800_0000;
/// GPIOI: the user LEDs LD1 (green) to LD4 (blue) on PI12 to PI15, lit low.
const LED_PORT: usize = 8;
const LED_PINS: [u32; 4] = [12, 13, 14, 15];
/// The panel's reset, PG3 (active low, pulled up), and its backlight, PJ12.
const RESET: (usize, u32) = (6, 3);
const BACKLIGHT: (usize, u32) = (9, 12);
/// The panel, in landscape.
pub const SCREEN: (u32, u32) = (800, 480);
const PS_PER_S: u64 = 1_000_000_000_000;

/// What the display shows, and a count that changes with it.
#[derive(Clone, Default)]
pub struct Screen {
    pub frame: Frame,
    pub generation: u64,
}

pub struct Disco {
    pub machine: Stm32h7,
    elf: Vec<u8>,
    controller: Controller,
    /// Whether the firmware's SHA-256 compression runs on the host.
    host_sha: bool,
    /// The virtual time, instructions and sleep of the chips before this one
    /// (each reset starts a new chip).
    before: (u64, u64, u64),
    logs: Logs,
    router: Router,
    pub leds: [bool; 4],
    panel: Arc<Mutex<PanelState>>,
    /// Whether the panel showed the LTDC's frames, at the last look.
    showing: bool,
    screen: Screen,
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
    /// The board with the firmware `elf`, the QSPI flash from `qspi` (an
    /// erased flash if the file does not exist; saved there by
    /// [`Disco::save_qspi`]), and a display with the `controller`; with
    /// `host_sha`, the firmware's SHA-256 compression runs on the host.
    pub fn new(
        elf: &[u8],
        qspi: Option<&Path>,
        controller: Controller,
        host_sha: bool,
    ) -> Result<Self, String> {
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
        let (machine, panel) = Self::chip(elf, flash, controller, host_sha)?;
        Ok(Self {
            machine,
            elf: elf.to_vec(),
            controller,
            host_sha,
            before: (0, 0, 0),
            logs: Logs::new(elf)?,
            router: Router::new(RouterConfig::default(), 0),
            leds: [false; 4],
            panel,
            showing: false,
            screen: Screen::default(),
            qspi_path: qspi.map(Path::to_path_buf),
        })
    }

    /// The chip running `elf` with the QSPI flash holding `flash`, and the
    /// panel's state.
    fn chip(
        elf: &[u8],
        flash: Vec<u8>,
        controller: Controller,
        host_sha: bool,
    ) -> Result<(Stm32h7, Arc<Mutex<PanelState>>), String> {
        let config = Config {
            hse_hz: HSE_HZ,
            uid: *b"ECSN-DISCO-1",
            external_ram: vec![SDRAM],
            qspi_flash: flash,
            qspi_dual: true,
            boot_address: BOOT,
            seed: 0x5eed_ec5e,
        };
        let mut machine = Stm32h7::new(config, elf)?;
        if host_sha && let Some(entry) = symbol(elf, SHA256_COMPRESS) {
            machine.host_function(entry, SHA256_COMPRESS, Box::new(Sha256Compress::default()));
        }
        let panel = Panel::new(controller);
        let state = panel.state.clone();
        {
            let mut p = machine.periph();
            p.eth.phy = Some(Box::new(Lan8742::new()));
            if controller != Controller::None {
                p.dsi.device = Some(Box::new(panel));
            }
        }
        Ok((machine, state))
    }

    /// Resets the chip as a reset with the RCC's reset `flags` does (see
    /// `stm32h7_rax::devices::rcc`): the firmware starts again with the QSPI
    /// flash as it is, the reset flags it had not cleared kept, the display
    /// reset; the network around the board stays.
    pub fn restart(&mut self, flags: u32) -> Result<(), String> {
        let (flash, kept) = {
            let p = self.machine.periph();
            (p.quadspi.flash.bytes.clone(), p.rcc.reset_flags())
        };
        let (machine, panel) = Self::chip(&self.elf, flash, self.controller, self.host_sha)?;
        machine.periph().rcc.set_reset_flags(kept | flags);
        let trace = self.machine.periph().trace;
        machine.periph().trace = trace;
        self.before = (self.now(), self.instructions(), self.asleep());
        self.machine = machine;
        self.panel = panel;
        self.logs = Logs::new(&self.elf)?;
        self.leds = [false; 4];
        self.showing = false;
        self.screen = Screen::default();
        Ok(())
    }

    /// A watchdog reset, as [`Disco::restart`] does it.
    pub fn watchdog_reset(&mut self) -> Result<(), String> {
        self.restart(RESET_IWDG1)
    }

    /// The board's virtual time, picoseconds, across resets.
    pub fn now(&self) -> u64 {
        self.before.0 + self.machine.now()
    }

    /// Instructions run, across resets.
    pub fn instructions(&self) -> u64 {
        self.before.1 + self.machine.instructions
    }

    /// Virtual time the core slept, picoseconds, across resets.
    pub fn asleep(&self) -> u64 {
        self.before.2 + self.machine.asleep
    }

    /// The router's time: ticks of `TICK_HZ`.
    fn ticks(&self) -> u64 {
        (u128::from(self.now()) * u128::from(TICK_HZ) / u128::from(PS_PER_S)) as u64
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
        let (sent, streaming, backlight) = {
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
            out.lines
                .extend(p.dsi.notes.drain(..).map(|n| format!("emu: {n}")));
            let gpio = &p.gpio[LED_PORT];
            for (led, &pin) in self.leds.iter_mut().zip(LED_PINS.iter()) {
                *led = gpio.mode(pin) == 1 && !gpio.level(pin);
            }
            let reset = &p.gpio[RESET.0];
            if reset.mode(RESET.1) == 1 && !reset.level(RESET.1) {
                Panel::reset(&self.panel);
            }
            let backlight = &p.gpio[BACKLIGHT.0];
            let lit = backlight.mode(BACKLIGHT.1) == 1 && backlight.level(BACKLIGHT.1);
            (
                p.eth.outgoing.drain(..).collect::<Vec<_>>(),
                p.dsi.streaming(),
                lit,
            )
        };
        let panel = self.panel.lock().unwrap().clone();
        let showing = panel.showing() && streaming && backlight;
        if showing != self.showing {
            self.showing = showing;
            out.lines.push(if showing {
                format!(
                    "emu: display on: {:?}, {}, COLMOD {:#04x}, brightness {}",
                    panel.controller,
                    if panel.address_mode & 0x20 != 0 {
                        "landscape"
                    } else {
                        "portrait"
                    },
                    panel.pixel_format,
                    panel.brightness
                )
            } else {
                "emu: display off".into()
            });
        }
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

    /// What the display shows now: the LTDC's picture while the panel shows
    /// it, else black.
    pub fn screen(&mut self) -> &Screen {
        let frame = self
            .showing
            .then(|| self.machine.ltdc_frame())
            .flatten()
            .unwrap_or_else(|| Frame {
                width: SCREEN.0,
                height: SCREEN.1,
                rgba: [0, 0, 0, 0xFF].repeat((SCREEN.0 * SCREEN.1) as usize),
            });
        if frame.width != self.screen.frame.width
            || frame.height != self.screen.frame.height
            || frame.rgba != self.screen.frame.rgba
        {
            self.screen = Screen {
                frame,
                generation: self.screen.generation + 1,
            };
        }
        &self.screen
    }

    /// Saves what the display shows as a PNG.
    pub fn save_screen(&mut self, path: &Path) -> Result<(), String> {
        let frame = &self.screen().frame;
        let rgb: Vec<u8> = frame
            .rgba
            .chunks(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let error = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
        let file = std::fs::File::create(path).map_err(|e| error(&e))?;
        let mut encoder =
            png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .and_then(|mut writer| writer.write_image_data(&rgb))
            .map_err(|e| error(&e))
    }

    /// Saves the QSPI flash where it was read from, creating its directory,
    /// through a temporary file renamed over the image, so an interrupted save
    /// leaves the previous image whole.
    pub fn save_qspi(&self) -> std::io::Result<()> {
        if let Some(path) = &self.qspi_path {
            if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            let mut temporary = path.clone().into_os_string();
            temporary.push(".saving");
            std::fs::write(&temporary, &self.machine.periph().quadspi.flash.bytes)?;
            std::fs::rename(&temporary, path)?;
        }
        Ok(())
    }
}

/// The firmware's SHA-256 block compression (`bitcoin_hashes`' `sha256-extern` feature).
const SHA256_COMPRESS: &str = "bitcoin_hashes_sha256_compress";

/// The address of the ELF's symbol `name`.
fn symbol(elf: &[u8], name: &str) -> Option<u32> {
    let image = goblin::elf::Elf::parse(elf).ok()?;
    image
        .syms
        .iter()
        .find(|s| image.strtab.get_at(s.st_name) == Some(name))
        .map(|s| s.st_value as u32)
}

/// `bitcoin_hashes_sha256_compress(state: *mut [u32; 8], block: *const [u8; 64])` on the host.
#[derive(Default)]
struct Sha256Compress {
    /// Where the state of the call the core runs is, and what it should become.
    at: u32,
    expected: [u32; 8],
}

impl Sha256Compress {
    /// The state at `at`, eight words in the core's byte order.
    fn state_at(cpu: &CortexMCpu, at: u32) -> [u32; 8] {
        let mut bytes = [0; 32];
        let _ = cpu.memory().read(u64::from(at), &mut bytes);
        std::array::from_fn(|i| {
            u32::from_le_bytes([
                bytes[4 * i],
                bytes[4 * i + 1],
                bytes[4 * i + 2],
                bytes[4 * i + 3],
            ])
        })
    }

    /// The state at r0 once the block at r1 is compressed into it.
    fn compute(cpu: &CortexMCpu) -> [u32; 8] {
        let mut state = Self::state_at(cpu, cpu.get_gpr(0) as u32);
        let mut block = [0; 64];
        let _ = cpu
            .memory()
            .read(u64::from(cpu.get_gpr(1) as u32), &mut block);
        sha2::compress256(&mut state, &[block.into()]);
        state
    }
}

impl HostFunction for Sha256Compress {
    fn run(&mut self, cpu: &mut CortexMCpu) {
        let state = Self::compute(cpu);
        let bytes: Vec<u8> = state.iter().flat_map(|word| word.to_le_bytes()).collect();
        let at = cpu.get_gpr(0) as u32;
        let _ = cpu.memory_mut().write(u64::from(at), &bytes);
    }

    fn expect(&mut self, cpu: &CortexMCpu) {
        self.at = cpu.get_gpr(0) as u32;
        self.expected = Self::compute(cpu);
    }

    fn matches(&mut self, cpu: &CortexMCpu) -> bool {
        // r0 is free in the callee: the state's address is taken at the entry
        Self::state_at(cpu, self.at) == self.expected
    }
}
