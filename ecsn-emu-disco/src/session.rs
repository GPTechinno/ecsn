//! The board running in its own thread, in real time (its peers are real),
//! and what the window shows of it.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use stm32h7_rax::Event;

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
    Quit,
}

pub struct Session {
    pub snapshot: Arc<Mutex<Snapshot>>,
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Runs `elf` on the board with the QSPI flash image `qspi` and a panel
    /// with `controller`; `wake` is called after each snapshot.
    pub fn start(
        elf: Vec<u8>,
        qspi: Option<PathBuf>,
        controller: Controller,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let (commands, inbox) = channel();
        let shared = snapshot.clone();
        let thread = std::thread::spawn(move || run(elf, qspi, controller, inbox, shared, wake));
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

fn run(
    elf: Vec<u8>,
    qspi: Option<PathBuf>,
    controller: Controller,
    inbox: Receiver<Command>,
    shared: Arc<Mutex<Snapshot>>,
    wake: impl Fn(),
) {
    let mut disco = match Disco::new(&elf, qspi.as_deref(), controller) {
        Ok(disco) => disco,
        Err(e) => {
            shared.lock().unwrap().stopped = Some(e);
            wake();
            return;
        }
    };
    let mut pace = Pace::new(disco.machine.now());
    let mut last_frame = Instant::now();
    let mut last_screen = Instant::now();
    // the board's screen generation last taken, and the window's
    let mut seen_screen = u64::MAX;
    let mut shown = 0;
    let mut window = (
        Instant::now(),
        disco.machine.now(),
        disco.machine.instructions,
    );
    let mut pending: Vec<String> = Vec::new();
    let mut stopped = None;
    loop {
        match inbox.try_recv() {
            Ok(Command::Quit) | Err(TryRecvError::Disconnected) => break,
            Ok(Command::Reset) => {
                let _ = disco.save_qspi();
                stopped = match Disco::new(&elf, qspi.as_deref(), controller) {
                    Ok(new) => {
                        disco = new;
                        None
                    }
                    Err(e) => Some(e),
                };
                pending.push("emu: reset".into());
                seen_screen = u64::MAX;
                pace = Pace::new(disco.machine.now());
                window = (Instant::now(), 0, 0);
            }
            Err(TryRecvError::Empty) => {}
        }
        if stopped.is_none() {
            let out = disco.run(STEP);
            let t = disco.machine.now() as f64 / PS_PER_S as f64;
            pending.extend(out.lines.into_iter().map(|l| format!("{t:10.6} {l}")));
            if let Some(event) = out.event {
                stopped = Some(match event {
                    Event::Breakpoint { pc, imm } => format!("BKPT {imm:#x} at {pc:#010x}"),
                    Event::Fault { pc, message } => format!("stopped at {pc:#010x}: {message}"),
                });
            }
            // real time: the board's peers are real
            std::thread::sleep(pace.ahead(disco.machine.now()).min(FRAME));
        } else {
            std::thread::sleep(FRAME);
        }
        if last_frame.elapsed() >= FRAME || stopped.is_some() {
            last_frame = Instant::now();
            let mut s = shared.lock().unwrap();
            for line in pending.drain(..) {
                log(&mut s, line);
            }
            let (wall, virt, instructions) = window;
            let real = wall.elapsed().as_secs_f64();
            if real >= 1.0 {
                s.speed = (disco.machine.now() - virt) as f64 / PS_PER_S as f64 / real;
                s.mips = (disco.machine.instructions - instructions) as f64 / real / 1e6;
                window = (
                    Instant::now(),
                    disco.machine.now(),
                    disco.machine.instructions,
                );
            }
            s.leds = disco.leds;
            s.virtual_s = disco.machine.now() as f64 / PS_PER_S as f64;
            s.asleep = disco.machine.asleep as f64 / disco.machine.now().max(1) as f64;
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
    let _ = disco.save_qspi();
}

#[cfg(test)]
mod tests {
    use super::*;

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
