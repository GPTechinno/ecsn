//! Runs the ECSN firmware on an emulated STM32H747I-DISCO, in a window (the
//! board's display, LEDs and state, the firmware's logs), or headless with
//! `--headless`: the firmware's logs, the emulator's notes and the network
//! events go to stdout, then the virtual and real time taken (and with
//! `--screen-png`, what the display shows goes to a PNG).

mod board;
mod gui;
mod logs;
mod pace;
mod panel;
mod phy;
mod profile;
mod session;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use clap::Parser;
use stm32h7_rax::Event;

use board::Disco;
use pace::Pace;
use panel::Controller;
use profile::Profile;

const PS_PER_S: u64 = 1_000_000_000_000;
/// Virtual time between two looks at the board: 1 ms.
const STEP: u64 = PS_PER_S / 1000;

#[derive(Parser)]
struct Args {
    /// The firmware ELF
    #[arg(
        long,
        default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../firmware/target/thumbv7em-none-eabihf/release/ecsn-firmware")
    )]
    elf: PathBuf,
    /// Run without a window, printing the logs
    #[arg(long)]
    headless: bool,
    /// Seconds of virtual time to run, headless
    #[arg(long, default_value_t = 10.0)]
    seconds: f64,
    /// The QSPI flash image, read at the start (an erased flash if missing)
    /// and written at the end, and at each RESET from the window, so the
    /// firmware finds its chain store again at the next run [default: one per
    /// network, the firmware's (read from its ELF): <network>.bin in
    /// $XDG_DATA_HOME/ecsn-emu-disco or ~/.local/share/ecsn-emu-disco;
    /// qspi.bin there for a firmware that does not say]
    #[arg(long)]
    qspi: Option<PathBuf>,
    /// Start from an erased QSPI flash and save it nowhere, whatever
    /// `--qspi` says
    #[arg(long)]
    volatile: bool,
    /// Keep virtual time no faster than the wall clock (for real peers)
    #[arg(long)]
    realtime: bool,
    /// Print the PC and the core clock after every millisecond
    #[arg(long)]
    trace: bool,
    /// Sample the PC every millisecond, and print where the core spent its
    /// time every this many seconds of virtual time and at the end
    #[arg(long, value_name = "SECONDS")]
    profile: Option<f64>,
    /// Log every peripheral access in this range of addresses, as
    /// start:end in hexadecimal (for example 58024400:58024800, the RCC)
    #[arg(long, value_parser = parse_range)]
    mmio_trace: Option<(u32, u32)>,
    /// The display's controller: the OTM8009A of the first boards, or the
    /// NT35510 of later ones; `none` for no panel on the DSI link
    #[arg(long, value_enum, default_value = "otm8009a")]
    panel: Controller,
    /// Run the firmware's SHA-256 compression on the emulated core too; by
    /// default the host runs it, charged the cycles the core took for its
    /// first calls, so virtual time is the same
    #[arg(long)]
    no_host_sha: bool,
    /// Save what the display shows as a PNG at the end of a headless run
    #[arg(long, value_name = "PNG")]
    screen_png: Option<PathBuf>,
}

fn parse_range(text: &str) -> Result<(u32, u32), String> {
    let (start, end) = text.split_once(':').ok_or("start:end")?;
    let parse =
        |t: &str| u32::from_str_radix(t.trim_start_matches("0x"), 16).map_err(|e| e.to_string());
    Ok((parse(start)?, parse(end)?))
}

/// The network a firmware is built for, from the magic at its `ECSN_NETWORK_MAGIC` symbol; `None`
/// for a firmware without it.
fn firmware_network(elf: &[u8]) -> Option<&'static str> {
    let image = goblin::elf::Elf::parse(elf).ok()?;
    let symbol = image
        .syms
        .iter()
        .find(|s| image.strtab.get_at(s.st_name) == Some("ECSN_NETWORK_MAGIC"))?;
    let address = symbol.st_value;
    let section = image.section_headers.iter().find(|h| {
        h.sh_type != goblin::elf::section_header::SHT_NOBITS
            && h.sh_addr <= address
            && address + 4 <= h.sh_addr + h.sh_size
    })?;
    let offset = (section.sh_offset + address - section.sh_addr) as usize;
    Some(match elf.get(offset..offset + 4)? {
        [0xF9, 0xBE, 0xB4, 0xD9] => "mainnet",
        [0x0B, 0x11, 0x09, 0x07] => "testnet",
        [0x1C, 0x16, 0x3F, 0x28] => "testnet4",
        [0x0A, 0x03, 0xCF, 0x40] => "signet",
        [0xFA, 0xBF, 0xB5, 0xDA] => "regtest",
        _ => return None,
    })
}

/// The emulator's directory for what it keeps between runs: `$XDG_DATA_HOME/ecsn-emu-disco`, or
/// `~/.local/share/ecsn-emu-disco`.
pub fn data_dir() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(data.join("ecsn-emu-disco"))
}

/// Where the QSPI flash image is kept between runs, when `--qspi` names none: one per network, so
/// that a firmware for one network does not format the store of another.
fn default_qspi(network: Option<&str>) -> Option<PathBuf> {
    let file = format!("{}.bin", network.unwrap_or("qspi"));
    Some(data_dir()?.join(file))
}

fn main() -> ExitCode {
    let args = Args::parse();
    let elf = match std::fs::read(&args.elf) {
        Ok(elf) => elf,
        Err(e) => {
            eprintln!("{}: {e}", args.elf.display());
            return ExitCode::FAILURE;
        }
    };
    let network = firmware_network(&elf);
    let qspi = if args.volatile {
        None
    } else {
        args.qspi.clone().or_else(|| default_qspi(network))
    };
    println!(
        "firmware for {}, flash image {}",
        network.unwrap_or("an unknown network"),
        qspi.as_ref()
            .map_or("none (erased, saved nowhere)".into(), |p| p
                .display()
                .to_string())
    );
    if !args.headless {
        gui::run(elf, qspi, args.panel, !args.no_host_sha);
        return ExitCode::SUCCESS;
    }
    let mut disco = match Disco::new(&elf, qspi.as_deref(), args.panel, !args.no_host_sha) {
        Ok(disco) => disco,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    disco.machine.periph().trace = args.mmio_trace;
    let total = (args.seconds * PS_PER_S as f64) as u64;
    let started = Instant::now();
    let mut pace = Pace::new(disco.now());
    // Ctrl-C or SIGTERM end the run as its end does: statistics, QSPI image.
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed));
    let mut profile = args
        .profile
        .map(|_| Profile::new(&elf).expect("ELF symbols"));
    let every = args.profile.map(|s| (s * PS_PER_S as f64) as u64);
    let mut next_report = every.unwrap_or(u64::MAX);
    let mut leds = disco.leds;
    let mut failed = false;
    while disco.now() < total && !interrupted.load(Ordering::Relaxed) {
        let out = disco.run(STEP);
        let t = disco.now() as f64 / PS_PER_S as f64;
        for line in out.lines {
            println!("{t:10.6} {line}");
        }
        if args.trace {
            let hz = disco.machine.periph().cpu_clock();
            println!(
                "{t:10.6} pc {:#010x} core {} Hz, {} instructions",
                disco.machine.pc(),
                hz,
                disco.instructions()
            );
        }
        if disco.leds != leds {
            leds = disco.leds;
            println!("{t:10.6} LEDs LD1-LD4: {leds:?}");
        }
        if let Some(Event::WatchdogReset { pc }) = out.event {
            println!("{t:10.6} emu: watchdog reset (pc {pc:#010x})");
            if let Err(e) = disco.watchdog_reset() {
                eprintln!("{e}");
                failed = true;
                break;
            }
        } else if let Some(event) = out.event {
            let regs = disco.machine.registers();
            match event {
                Event::Breakpoint { pc, imm } => println!("{t:10.6} BKPT {imm:#x} at {pc:#010x}"),
                Event::Fault { pc, message } => {
                    println!("{t:10.6} stopped at {pc:#010x}: {message}")
                }
                Event::WatchdogReset { .. } => {}
            }
            println!(
                "registers: {}",
                regs.iter()
                    .enumerate()
                    .map(|(i, r)| format!("r{i}={r:#010x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            failed = true;
            break;
        }
        if let Some(profile) = &mut profile {
            let regs = disco.machine.registers();
            profile.sample(regs[15], regs[14]);
            if disco.now() >= next_report {
                next_report += every.unwrap_or(u64::MAX);
                print!("{t:10.6} {}", profile.interval_report(12));
            }
        }
        if args.realtime {
            std::thread::sleep(pace.ahead(disco.now()));
        }
    }
    if let Some(profile) = &profile {
        print!("{}", profile.whole_report(30));
    }
    let virtual_s = disco.now() as f64 / PS_PER_S as f64;
    let wall = started.elapsed().as_secs_f64();
    println!(
        "{virtual_s:.3} s virtual in {wall:.3} s, {} instructions ({:.1} MIPS), asleep {:.1} %",
        disco.instructions(),
        disco.instructions() as f64 / wall / 1e6,
        100.0 * disco.asleep() as f64 / disco.now().max(1) as f64
    );
    println!(
        "Ethernet: {} frames dropped",
        disco.machine.periph().eth.dropped
    );
    if let Err(e) = disco.save_qspi() {
        eprintln!("QSPI image: {e}");
    }
    if let Some(path) = &args.screen_png
        && let Err(e) = disco.save_screen(path)
    {
        eprintln!("screen: {e}");
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
