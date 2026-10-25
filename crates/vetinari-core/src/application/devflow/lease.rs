//! The heartbeat schedule, as pure functions of the lease and the clock.

use std::num::NonZeroU32;

use time::{Duration, OffsetDateTime};

use crate::domain::devflow::compile::LeaseSecs;

/// When the next heartbeat is due: a third of the lease after the last
/// extension, which leaves two chances to fail before the lease is in danger.
#[must_use]
pub fn next_heartbeat(lease: LeaseSecs, extended_at: OffsetDateTime) -> OffsetDateTime {
    extended_at + Duration::milliseconds(i64::from(lease.get()) * 1000 / 3)
}

/// How long to wait after the `failures`th failed heartbeat in a row: 2, 4,
/// 8 seconds and so on, capped at a sixth of the lease.
#[must_use]
pub fn retry_after(lease: LeaseSecs, failures: NonZeroU32) -> Duration {
    let cap = i64::from(lease.get()) * 1000 / 6;
    let backoff = 2_i64.saturating_pow(failures.get()).saturating_mul(1000);
    Duration::milliseconds(backoff.min(cap))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn lease(secs: u32) -> LeaseSecs {
        LeaseSecs::new(secs).expect("lease")
    }

    fn failures(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    #[test]
    fn a_heartbeat_is_due_a_third_of_the_lease_after_the_last_extension() {
        let at = datetime!(2026-10-25 10:00:00 UTC);
        assert_eq!(
            next_heartbeat(lease(300), at),
            datetime!(2026-10-25 10:01:40 UTC)
        );
        assert_eq!(
            next_heartbeat(lease(900), at),
            datetime!(2026-10-25 10:05:00 UTC)
        );
    }

    #[test]
    fn a_lease_that_does_not_divide_by_three_keeps_its_fraction() {
        let at = datetime!(2026-10-25 10:00:00 UTC);
        assert_eq!(
            next_heartbeat(lease(31), at),
            at + Duration::milliseconds(10_333)
        );
    }

    #[test]
    fn retries_back_off_from_two_seconds() {
        assert_eq!(retry_after(lease(300), failures(1)), Duration::seconds(2));
        assert_eq!(retry_after(lease(300), failures(2)), Duration::seconds(4));
        assert_eq!(retry_after(lease(300), failures(3)), Duration::seconds(8));
    }

    #[test]
    fn retries_never_wait_longer_than_a_sixth_of_the_lease() {
        assert_eq!(retry_after(lease(300), failures(6)), Duration::seconds(50));
        assert_eq!(retry_after(lease(30), failures(3)), Duration::seconds(5));
        assert_eq!(
            retry_after(lease(300), failures(u32::MAX)),
            Duration::seconds(50)
        );
    }
}
