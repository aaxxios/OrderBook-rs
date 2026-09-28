#[cfg(test)]
// tests may panic: rules/global_rules.md § Testing
mod tests {
    use crate::current_time_millis;
    use crate::utils::time::{
        TimeError, duration_to_millis, fallback_millis, system_time_to_millis,
        try_current_time_millis,
    };
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tracing::info;

    #[test]
    fn test_current_time_millis_increases() {
        let time1 = current_time_millis();
        // Sleep for a bit to ensure time passes
        thread::sleep(Duration::from_millis(5));
        let time2 = current_time_millis();

        // The second time should be greater than the first
        assert!(time2 > time1, "Time should increase between calls");
    }

    #[test]
    fn test_current_time_millis_is_reasonably_current() {
        // Get current time using both methods
        let time_from_function = current_time_millis();
        let time_direct = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_millis(),
        )
        .expect("millis fit in u64");

        // The times should be very close to each other
        // Allow a small difference due to execution time between the two calls
        let difference = time_direct.abs_diff(time_from_function);

        // The difference should be no more than 10ms (this is generous)
        assert!(
            difference <= 10,
            "Time difference should be small, but got {difference}ms"
        );
    }

    #[test]
    fn test_current_time_millis_precision() {
        // Call the function twice in quick succession
        let time1 = current_time_millis();
        let time2 = current_time_millis();

        // Check if we have at least millisecond precision
        // This test might be flaky if both calls happen within the same millisecond,
        // but it's unlikely on most modern systems
        // If it fails, it doesn't necessarily indicate a problem
        if time1 == time2 {
            info!("Note: consecutive calls returned same time, which might happen occasionally");
        }

        // Ensure with a sleep that we get different values
        thread::sleep(Duration::from_millis(5));
        let time3 = current_time_millis();
        assert!(time3 > time1, "Time should increase after sleep");
    }

    #[test]
    fn test_try_current_time_millis_matches_wrapper() {
        let checked = try_current_time_millis().expect("clock is after the epoch");
        let wrapped = current_time_millis();
        assert!(wrapped >= checked);
        assert!(wrapped - checked <= 10);
    }

    #[test]
    fn test_system_time_to_millis_epoch_is_zero() {
        assert_eq!(system_time_to_millis(UNIX_EPOCH), Ok(0));
    }

    #[test]
    fn test_system_time_to_millis_exact() {
        let t = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(system_time_to_millis(t), Ok(1_700_000_000_123));
    }

    #[test]
    fn test_system_time_to_millis_before_epoch() {
        let t = UNIX_EPOCH
            .checked_sub(Duration::from_secs(5))
            .expect("platform supports pre-epoch SystemTime");
        assert_eq!(
            system_time_to_millis(t),
            Err(TimeError::ClockBeforeEpoch {
                behind: Duration::from_secs(5)
            })
        );
    }

    #[test]
    fn test_duration_to_millis_max_fits() {
        assert_eq!(
            duration_to_millis(Duration::from_millis(u64::MAX)),
            Ok(u64::MAX)
        );
    }

    #[test]
    fn test_duration_to_millis_overflow() {
        // One millisecond past u64::MAX ms.
        let d = Duration::from_millis(u64::MAX) + Duration::from_millis(1);
        let expected = u128::from(u64::MAX) + 1;
        assert_eq!(
            duration_to_millis(d),
            Err(TimeError::MillisOverflow { millis: expected })
        );
        assert!(matches!(
            duration_to_millis(Duration::MAX),
            Err(TimeError::MillisOverflow { .. })
        ));
    }

    #[test]
    fn test_fallback_values_are_explicit() {
        let before = TimeError::ClockBeforeEpoch {
            behind: Duration::from_millis(1),
        };
        let overflow = TimeError::MillisOverflow {
            millis: u128::from(u64::MAX) + 1,
        };
        // Repeated calls keep returning the same sentinel (the warn is latched).
        assert_eq!(fallback_millis(before), 0);
        assert_eq!(fallback_millis(before), 0);
        assert_eq!(fallback_millis(overflow), u64::MAX);
        assert_eq!(fallback_millis(overflow), u64::MAX);
    }

    #[test]
    fn test_time_error_display() {
        let e = TimeError::ClockBeforeEpoch {
            behind: Duration::from_secs(2),
        };
        assert!(e.to_string().contains("before the UNIX epoch"));
        let e = TimeError::MillisOverflow { millis: 7 };
        assert!(e.to_string().contains("does not fit in u64"));
    }
}
