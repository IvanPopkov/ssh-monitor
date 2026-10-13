use std::time::Duration;

/// `mono` drives every rule; `wall` is display-only. They drift apart when the machine sleeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Stamp {
    pub mono: Duration,
    /// Unix ms.
    pub wall: u64,
}

/// 2026-01-01T00:00:00Z.
pub const WALL_ORIGIN_MS: u64 = 1_767_225_600_000;

impl Stamp {
    pub const fn new(mono: Duration, wall: u64) -> Self {
        Self { mono, wall }
    }

    pub const fn at_secs(secs: u64) -> Self {
        Self::at_millis(secs * 1000)
    }

    pub const fn at_millis(ms: u64) -> Self {
        Self {
            mono: Duration::from_millis(ms),
            wall: WALL_ORIGIN_MS + ms,
        }
    }

    /// Simulates a clock jump.
    pub const fn with_wall(self, wall: u64) -> Self {
        Self { wall, ..self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_keep_mono_and_wall_in_step() {
        let s = Stamp::at_secs(5);
        assert_eq!(s.mono, Duration::from_secs(5));
        assert_eq!(s.wall, WALL_ORIGIN_MS + 5000);
        assert_eq!(Stamp::at_millis(1500).mono, Duration::from_millis(1500));
        assert_eq!(s.with_wall(7).wall, 7);
        assert_eq!(s.with_wall(7).mono, s.mono);
    }

    #[test]
    fn ordering_is_by_mono_first() {
        assert!(Stamp::new(Duration::from_secs(1), 10) < Stamp::new(Duration::from_secs(2), 0));
    }
}
