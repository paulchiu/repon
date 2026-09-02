//! The seconds-and-minutes elapsed-time ladder, shared by [`crate::header`]'s run timer and
//! [`crate::components::detail`]'s per-step timings so the two surfaces cannot drift onto
//! their own separate ladders.

use std::time::Duration;

/// Renders `elapsed` at one decimal place of seconds under a minute, then minutes and
/// seconds beyond that, and the same ladder onward however long the elapsed time runs.
/// [`crate::components::detail`]'s own steps call this directly: a step's first tenth of a
/// second is never interesting enough to need finer resolution than this already gives it.
/// [`crate::header`]'s run timer wraps this in [`format_elapsed`] instead, for the one case
/// this ladder does not cover: a run's own first second, before it has anything to round.
pub(crate) fn format_seconds_elapsed(elapsed: Duration) -> String {
    let whole_secs = elapsed.as_secs();
    if whole_secs < 60 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        format!("{}m{:02}s", whole_secs / 60, whole_secs % 60)
    }
}

/// [`format_seconds_elapsed`]'s own ladder, extended one rung lower: plain milliseconds under
/// a second, for the header's run timer, which starts counting from zero and would otherwise
/// round its first second down to `0.0s` to `0.9s` with nothing to distinguish them.
pub(crate) fn format_elapsed(elapsed: Duration) -> String {
    if elapsed.as_secs() == 0 {
        format!("{}ms", elapsed.as_millis())
    } else {
        format_seconds_elapsed(elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_a_second_renders_as_plain_milliseconds() {
        assert_eq!(format_elapsed(Duration::from_millis(168)), "168ms");
        assert_eq!(format_elapsed(Duration::from_millis(0)), "0ms");
    }

    #[test]
    fn a_second_or_beyond_matches_the_shared_seconds_ladder() {
        assert_eq!(format_elapsed(Duration::from_millis(1000)), "1.0s");
        assert_eq!(format_elapsed(Duration::from_millis(12000)), "12.0s");
        assert_eq!(format_elapsed(Duration::from_secs(75)), "1m15s");
    }

    #[test]
    fn the_seconds_ladder_alone_renders_a_sub_second_duration_as_a_fraction_of_a_second() {
        // detail.rs's own contract: a step never needs the millisecond branch, since its
        // elapsed time is never observed before it has rounded to a tenth of a second.
        assert_eq!(format_seconds_elapsed(Duration::from_millis(300)), "0.3s");
    }

    #[test]
    fn under_a_minute_renders_seconds_to_one_decimal_place() {
        assert_eq!(
            format_seconds_elapsed(Duration::from_millis(12000)),
            "12.0s"
        );
    }

    #[test]
    fn a_minute_or_beyond_renders_minutes_and_seconds() {
        assert_eq!(format_seconds_elapsed(Duration::from_secs(75)), "1m15s");
        assert_eq!(format_seconds_elapsed(Duration::from_secs(168)), "2m48s");
    }
}
