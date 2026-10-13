//! The wall clock runs while mono stops during sleep, so `wall − mono` jumps forward (A6b).

use crate::Stamp;
use std::time::Duration;

pub const RESUME_THRESHOLD: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default)]
pub struct Sleep {
    last_offset_ms: Option<i128>,
    pub last_resume: Option<(Stamp, Duration)>,
}

impl Sleep {
    pub fn observe(&mut self, stamp: Stamp) -> Option<Duration> {
        let offset =
            i128::from(stamp.wall) - i128::try_from(stamp.mono.as_millis()).unwrap_or(i128::MAX);
        let prev = self.last_offset_ms.replace(offset)?;
        let jump = offset - prev;
        if jump < i128::try_from(RESUME_THRESHOLD.as_millis()).unwrap_or(i128::MAX) {
            return None;
        }
        let gap = Duration::from_millis(u64::try_from(jump).unwrap_or(u64::MAX));
        self.last_resume = Some((stamp, gap));
        Some(gap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_forward_wall_jumps_of_ten_seconds_or_more() {
        let mut s = Sleep::default();
        assert_eq!(
            s.observe(Stamp::at_secs(1)),
            None,
            "first stamp sets the reference"
        );
        assert_eq!(s.observe(Stamp::at_secs(2)), None);
        // Wall advanced 9.5s more than mono: below threshold.
        assert_eq!(
            s.observe(Stamp::at_secs(3).with_wall(Stamp::at_secs(3).wall + 9_500)),
            None
        );
        // Reference now includes the 9.5s; another 12s jump → resume of 12s.
        let slept = Stamp::at_secs(4).with_wall(Stamp::at_secs(4).wall + 9_500 + 12_000);
        assert_eq!(s.observe(slept), Some(Duration::from_secs(12)));
        assert_eq!(s.last_resume, Some((slept, Duration::from_secs(12))));
        // Clock set backwards: ignored but becomes the new reference.
        assert_eq!(s.observe(Stamp::at_secs(5).with_wall(1)), None);
        assert_eq!(
            s.observe(Stamp::at_secs(6).with_wall(1 + 1000 + 10_000)),
            Some(Duration::from_secs(10))
        );
    }
}
