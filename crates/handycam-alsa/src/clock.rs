//! Pure timestamp helpers, kept free of any ALSA handle so they can be unit
//! tested without a real capture device.

use std::time::Duration;

/// Converts an ALSA `timespec` (as returned by `Status::get_htstamp`) into a
/// `Duration`, or `None` if it is negative (which `libc::timespec` allows in
/// general but a valid monotonic-clock reading never produces) or the
/// all-zero value ALSA reports when the driver does not support hardware
/// timestamps for this device.
pub(crate) fn timespec_duration(ts: libc::timespec) -> Option<Duration> {
    if ts.tv_sec == 0 && ts.tv_nsec == 0 {
        return None;
    }
    if ts.tv_sec < 0 || ts.tv_nsec < 0 {
        return None;
    }
    Some(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

/// Estimates how many sample-frames were lost during `elapsed` wall-clock
/// time. This is a wall-clock estimate, not an exact hardware count: ALSA
/// does not expose the true lost-frame count generically across drivers on
/// an overrun (xrun), so this is the best a portable capture backend can do.
pub(crate) fn estimate_frames_lost(elapsed: Duration, sample_rate_hz: u32) -> u32 {
    let frames = elapsed.as_secs_f64() * f64::from(sample_rate_hz);
    if frames.is_finite() && frames > 0.0 {
        frames as u32
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timespec_duration_rejects_the_all_zero_unsupported_marker() {
        assert!(
            timespec_duration(libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            })
            .is_none()
        );
    }

    #[test]
    fn timespec_duration_rejects_negative_fields() {
        assert!(
            timespec_duration(libc::timespec {
                tv_sec: -1,
                tv_nsec: 0,
            })
            .is_none()
        );
    }

    #[test]
    fn timespec_duration_converts_seconds_and_nanos() {
        let duration = timespec_duration(libc::timespec {
            tv_sec: 2,
            tv_nsec: 500_000_000,
        })
        .unwrap();
        assert_eq!(duration, Duration::new(2, 500_000_000));
    }

    #[test]
    fn estimate_frames_lost_scales_by_sample_rate() {
        assert_eq!(estimate_frames_lost(Duration::from_millis(10), 16_000), 160);
        assert_eq!(estimate_frames_lost(Duration::from_millis(1), 16_000), 16);
    }

    #[test]
    fn estimate_frames_lost_is_zero_for_no_elapsed_time() {
        assert_eq!(estimate_frames_lost(Duration::ZERO, 16_000), 0);
    }
}
