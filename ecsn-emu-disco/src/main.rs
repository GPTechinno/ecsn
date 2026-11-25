//! Runs the ECSN firmware on an emulated STM32H747I-DISCO, in a window (the
//! board's LEDs and state, the firmware's logs), or headless with
//! `--headless`: the firmware's logs, the emulator's notes and the network
//! events go to stdout, then the virtual and real time taken.

mod board;
mod gui;
mod logs;
mod pace;
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
    /// and written at the end
    #[arg(long)]
    qspi: Option<PathBuf>,
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
}

fn parse_range(text: &str) -> Result<(u32, u32), String> {
    let (start, end) = text.split_once(':').ok_or("start:end")?;
    let parse =
        |t: &str| u32::from_str_radix(t.trim_start_matches("0x"), 16).map_err(|e| e.to_string());
    Ok((parse(start)?, parse(end)?))
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
    if !args.headless {
        gui::run(elf, args.qspi);
        return ExitCode::SUCCESS;
    }
    let mut disco = match Disco::new(&elf, args.qspi.as_deref()) {
        Ok(disco) => disco,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    disco.machine.periph().trace = args.mmio_trace;
    let total = (args.seconds * PS_PER_S as f64) as u64;
    let started = Instant::now();
    let mut pace = Pace::new(disco.machine.now());
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
    while disco.machine.now() < total && !interrupted.load(Ordering::Relaxed) {
        let out = disco.run(STEP);
        let t = disco.machine.now() as f64 / PS_PER_S as f64;
        for line in out.lines {
            println!("{t:10.6} {line}");
        }
        if args.trace {
            let hz = disco.machine.periph().cpu_clock();
            println!(
                "{t:10.6} pc {:#010x} core {} Hz, {} instructions",
                disco.machine.pc(),
                hz,
                disco.machine.instructions
            );
        }
        if disco.leds != leds {
            leds = disco.leds;
            println!("{t:10.6} LEDs LD1-LD4: {leds:?}");
        }
        if let Some(event) = out.event {
            let regs = disco.machine.registers();
            match event {
                Event::Breakpoint { pc, imm } => println!("{t:10.6} BKPT {imm:#x} at {pc:#010x}"),
                Event::Fault { pc, message } => {
                    println!("{t:10.6} stopped at {pc:#010x}: {message}")
                }
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
            if disco.machine.now() >= next_report {
                next_report += every.unwrap_or(u64::MAX);
                print!("{t:10.6} {}", profile.interval_report(12));
            }
        }
        if args.realtime {
            std::thread::sleep(pace.ahead(disco.machine.now()));
        }
    }
    if let Some(profile) = &profile {
        print!("{}", profile.whole_report(30));
    }
    let virtual_s = disco.machine.now() as f64 / PS_PER_S as f64;
    let wall = started.elapsed().as_secs_f64();
    println!(
        "{virtual_s:.3} s virtual in {wall:.3} s, {} instructions ({:.1} MIPS), asleep {:.1} %",
        disco.machine.instructions,
        disco.machine.instructions as f64 / wall / 1e6,
        100.0 * disco.machine.asleep as f64 / disco.machine.now().max(1) as f64
    );
    println!(
        "Ethernet: {} frames dropped",
        disco.machine.periph().eth.dropped
    );
    if let Err(e) = disco.save_qspi() {
        eprintln!("QSPI image: {e}");
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
