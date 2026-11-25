//! Real-time pacing, for real peers: virtual time never runs faster than the
//! wall clock. When the board is slower than real time (the core busy), it
//! falls behind and does not catch up afterwards: catching up would let the
//! lost time pass in an instant for the firmware, its timeouts firing while
//! its peers take real time to answer.

use std::time::{Duration, Instant};

/// How far behind the wall clock the board may fall before the lag is let go
/// (the overshoot of the sleeps is caught up within it).
const SLACK: Duration = Duration::from_millis(20);

pub struct Pace {
    wall: Instant,
    /// Virtual time at `wall`, in picoseconds.
    virt: u64,
}

impl Pace {
    pub fn new(virt: u64) -> Self {
        Self {
            wall: Instant::now(),
            virt,
        }
    }

    /// How long to wait before running on from `virt` picoseconds.
    pub fn ahead(&mut self, virt: u64) -> Duration {
        let ran = Duration::from_nanos(virt.saturating_sub(self.virt) / 1000);
        let wall = self.wall.elapsed();
        if wall > ran + SLACK {
            *self = Self::new(virt);
            return Duration::ZERO;
        }
        ran.saturating_sub(wall)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000_000;

    #[test]
    fn a_board_ahead_waits() {
        let mut pace = Pace::new(0);
        let wait = pace.ahead(100 * MS);
        assert!(wait > Duration::from_millis(90) && wait <= Duration::from_millis(100));
    }

    #[test]
    fn a_board_behind_does_not_catch_up() {
        let mut pace = Pace::new(0);
        pace.wall -= Duration::from_secs(5);
        // 5 s of wall clock for 1 ms of virtual time: the lag is let go
        assert_eq!(pace.ahead(MS), Duration::ZERO);
        // and the next virtual 100 ms wait for 100 ms of wall clock
        assert!(pace.ahead(101 * MS) > Duration::from_millis(90));
    }
}
