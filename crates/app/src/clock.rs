//! The only place the shell reads clocks; core never asks the time.

use ssh_monitor_core::Stamp;
use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    fn now(&self) -> Stamp;
}

/// `mono`: tokio `Instant` (paused in tests, stops during macOS sleep); `wall`: system clock.
pub struct RealClock {
    origin: tokio::time::Instant,
}

impl RealClock {
    pub fn new() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
        }
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn now(&self) -> Stamp {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Stamp::new(self.origin.elapsed(), wall)
    }
}

/// Must run before any thread exists (`time` crate on Unix); DST changes are not tracked.
pub fn local_utc_offset_secs() -> i32 {
    time::UtcOffset::current_local_offset()
        .map(time::UtcOffset::whole_seconds)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn real_clock_advances_both_axes() {
        let c = RealClock::new();
        let a = c.now();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let b = c.now();
        assert!(b.mono > a.mono);
        assert!(b.wall >= a.wall);
        assert!(a.wall > 1_700_000_000_000, "wall is unix ms");
    }

    #[test]
    fn offset_is_a_sane_number_of_seconds() {
        let off = local_utc_offset_secs();
        assert!((-14 * 3600..=14 * 3600).contains(&off));
    }
}
