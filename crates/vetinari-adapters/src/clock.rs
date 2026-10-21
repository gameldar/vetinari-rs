//! The system clock.

use time::OffsetDateTime;
use vetinari_core::application::ports::Clock;

/// Reads the real time, in UTC, falling back from the local offset when the
/// platform will not give one.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc())
    }
}
