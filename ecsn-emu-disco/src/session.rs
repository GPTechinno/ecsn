//! The board running in its own thread, in real time (its peers are real),
//! and what the window shows of it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use stm32h7_rax::Event;
use stm32h7_rax::devices::rcc::RESET_PIN;

use crate::board::{Disco, Screen};
use crate::pace::Pace;
use crate::panel::Controller;

const PS_PER_S: u64 = 1_000_000_000_000;
/// Virtual time between two looks at the board: 1 ms.
const STEP: u64 = PS_PER_S / 1000;
/// Real time between two snapshots for the window.
const FRAME: Duration = Duration::from_millis(33);
/// Log lines kept for the window.
const LOG_LINES: usize = 5000;
/// Real time between two looks at the display.
const SCREEN_EVERY: Duration = Duration::from_millis(100);

/// What the window shows.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub leds: [bool; 4],
    pub logs: VecDeque<String>,
    /// Lines logged since the start, to follow new ones.
    pub log_count: u64,
    pub virtual_s: f64,
    /// Virtual time over real time, lately.
    pub speed: f64,
    pub mips: f64,
    pub asleep: f64,
    /// The address from DHCP, and the last validated block.
    pub address: Option<String>,
    pub validated: Option<String>,
    /// Why the firmware stopped.
    pub stopped: Option<String>,
    /// What the display shows.
    pub screen: Arc<Screen>,
}

pub enum Command {
    /// Resets the chip: the flash image is kept.
    Reset,
    /// Writes every line logged since the start to a file (see [`log_file`]).
    SaveLogs,
    Quit,
}

pub struct Session {
    pub snapshot: Arc<Mutex<Snapshot>>,
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Runs `elf` on the board with the QSPI flash image `qspi` and a panel
    /// with `controller` (with `host_sha`, the firmware's SHA-256 compression
    /// on the host); `wake` is called after each snapshot.
    pub fn start(
        elf: Vec<u8>,
        qspi: Option<PathBuf>,
        controller: Controller,
        host_sha: bool,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let (commands, inbox) = channel();
        let shared = snapshot.clone();
        let thread =
            std::thread::spawn(move || run(elf, qspi, controller, host_sha, inbox, shared, wake));
        Self {
            snapshot,
            commands,
            thread: Some(thread),
        }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// Stops the board, which saves its flash image.
    pub fn stop(mut self) {
        self.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn log(snapshot: &mut Snapshot, line: String) {
    if let Some(address) = line.split("Ethernet: ").nth(1).filter(|a| a.contains('/')) {
        snapshot.address = Some(address.trim().to_string());
    }
    // Each block validated (floresta's "New tip! hash=… height=N …"), and the
    // node's progress ("validated up to N of M")
    let height = if line.contains("New tip!") {
        line.split("height=").nth(1)
    } else {
        line.split("validated up to ").nth(1)
    };
    if let Some(height) = height {
        let digits: String = height.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            snapshot.validated = Some(digits);
        }
    }
    snapshot.logs.push_back(line);
    snapshot.log_count += 1;
    while snapshot.logs.len() > LOG_LINES {
        snapshot.logs.pop_front();
    }
}

/// Where the logs of a run with the flash image `qspi` are saved at `now`: `logs/` beside the
/// image (in the data directory for a run without one), named after the image and the UTC time,
/// `mainnet-20261127-093512.log`.
fn log_file(qspi: Option<&Path>, now: SystemTime) -> PathBuf {
    let dir = qspi
        .and_then(Path::parent)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(crate::data_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = qspi
        .and_then(Path::file_stem)
        .map_or("volatile".into(), |s| s.to_string_lossy());
    dir.join("logs")
        .join(format!("{stem}-{}.log", utc_stamp(now)))
}

/// `now` as `YYYYMMDD-HHMMSS` in UTC.
fn utc_stamp(now: SystemTime) -> String {
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, time) = ((secs / 86_400) as i64, secs % 86_400);
    // the civil date of a day count since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

/// Writes `lines` to [`log_file`]; returns the file.
fn save_logs<'a>(
    qspi: Option<&Path>,
    lines: impl Iterator<Item = &'a String>,
) -> std::io::Result<PathBuf> {
    let path = log_file(qspi, SystemTime::now());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text)?;
    Ok(path)
}

fn run(
    elf: Vec<u8>,
    qspi: Option<PathBuf>,
    controller: Controller,
    host_sha: bool,
    inbox: Receiver<Command>,
    shared: Arc<Mutex<Snapshot>>,
    wake: impl Fn(),
) {
    let mut disco = match Disco::new(&elf, qspi.as_deref(), controller, host_sha) {
        Ok(disco) => disco,
        Err(e) => {
            shared.lock().unwrap().stopped = Some(e);
            wake();
            return;
        }
    };
    let mut pace = Pace::new(disco.now());
    let mut last_frame = Instant::now();
    let mut last_screen = Instant::now();
    // the board's screen generation last taken, and the window's
    let mut seen_screen = u64::MAX;
    let mut shown = 0;
    let mut window = (Instant::now(), disco.now(), disco.instructions());
    let mut pending: Vec<String> = Vec::new();
    // every line since the start, for SAVE LOGS (the window keeps the last LOG_LINES)
    let mut history: Vec<String> = Vec::new();
    let mut stopped = None;
    loop {
        match inbox.try_recv() {
            Ok(Command::Quit) | Err(TryRecvError::Disconnected) => break,
            Ok(Command::SaveLogs) => {
                let line = match save_logs(qspi.as_deref(), history.iter().chain(&pending)) {
                    Ok(path) => format!("emu: logs saved to {}", path.display()),
                    Err(e) => format!("emu: logs not saved: {e}"),
                };
                pending.push(line);
            }
            Ok(Command::Reset) => {
                if let Err(e) = disco.save_qspi() {
                    eprintln!("ecsn-emu-disco: QSPI image: {e}");
                }
                stopped = disco.restart(RESET_PIN).err();
                pending.push("emu: reset".into());
                seen_screen = u64::MAX;
            }
            Err(TryRecvError::Empty) => {}
        }
        if stopped.is_none() {
            let out = disco.run(STEP);
            let t = disco.now() as f64 / PS_PER_S as f64;
            pending.extend(out.lines.into_iter().map(|l| format!("{t:10.6} {l}")));
            match out.event {
                Some(Event::Breakpoint { pc, imm }) => {
                    stopped = Some(format!("BKPT {imm:#x} at {pc:#010x}"))
                }
                Some(Event::Fault { pc, message }) => {
                    stopped = Some(format!("stopped at {pc:#010x}: {message}"))
                }
                Some(Event::WatchdogReset { pc }) => {
                    pending.push(format!("{t:10.6} emu: watchdog reset (pc {pc:#010x})"));
                    stopped = disco.watchdog_reset().err();
                    seen_screen = u64::MAX;
                }
                None => {}
            }
            // real time: the board's peers are real
            std::thread::sleep(pace.ahead(disco.now()).min(FRAME));
        } else {
            std::thread::sleep(FRAME);
        }
        if last_frame.elapsed() >= FRAME || stopped.is_some() {
            last_frame = Instant::now();
            let mut s = shared.lock().unwrap();
            for line in pending.drain(..) {
                history.push(line.clone());
                log(&mut s, line);
            }
            let (wall, virt, instructions) = window;
            let real = wall.elapsed().as_secs_f64();
            if real >= 1.0 {
                s.speed = (disco.now() - virt) as f64 / PS_PER_S as f64 / real;
                s.mips = (disco.instructions() - instructions) as f64 / real / 1e6;
                window = (Instant::now(), disco.now(), disco.instructions());
            }
            s.leds = disco.leds;
            s.virtual_s = disco.now() as f64 / PS_PER_S as f64;
            s.asleep = disco.asleep() as f64 / disco.now().max(1) as f64;
            s.stopped = stopped.clone();
            drop(s);
            if last_screen.elapsed() >= SCREEN_EVERY {
                last_screen = Instant::now();
                let screen = disco.screen();
                if screen.generation != seen_screen {
                    seen_screen = screen.generation;
                    shown += 1;
                    let screen = Arc::new(Screen {
                        frame: screen.frame.clone(),
                        generation: shown,
                    });
                    shared.lock().unwrap().screen = screen;
                }
            }
            wake();
        }
    }
    if let Err(e) = disco.save_qspi() {
        eprintln!("ecsn-emu-disco: QSPI image: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_file_is_named_after_its_flash_image_and_the_utc_time() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_772_112);
        assert_eq!(utc_stamp(at), "20261127-093512");
        let leap = SystemTime::UNIX_EPOCH + Duration::from_secs(951_868_799);
        assert_eq!(utc_stamp(leap), "20000229-235959");
        assert_eq!(utc_stamp(SystemTime::UNIX_EPOCH), "19700101-000000");
        assert_eq!(
            log_file(Some(Path::new("/data/ecsn-emu-disco/mainnet.bin")), at),
            PathBuf::from("/data/ecsn-emu-disco/logs/mainnet-20261127-093512.log")
        );
    }

    #[test]
    fn the_validated_height_follows_each_new_tip_and_the_progress() {
        let mut snapshot = Snapshot::default();
        log(
            &mut snapshot,
            "99.577000 99.575717 INFO  floresta_chain::pruned_utreexo::chain_state: New tip! \
             hash=0000000c823ba5a1 height=331882 tx_count=289"
                .into(),
        );
        assert_eq!(snapshot.validated.as_deref(), Some("331882"));
        log(
            &mut snapshot,
            "INFO  ecsn::node: validated up to 332000 of 332441".into(),
        );
        assert_eq!(snapshot.validated.as_deref(), Some("332000"));
        log(
            &mut snapshot,
            "INFO  ecsn::node: headers synced: best 332441".into(),
        );
        assert_eq!(snapshot.validated.as_deref(), Some("332000"));
    }
}
