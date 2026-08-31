//! The Cell vocabulary: every displayed value carries its whole provenance, and the
//! only way to it is a match, so an absent value can never be read as a default.
//!
//! See [ADR 0001](https://github.com/paulchiu/repon/blob/main/docs/adr/0001-per-cell-provenance.md),
//! [ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md)
//! and `docs/spec/core-api.md`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::git::ProbeError;

/// One refresh, so a newer one can be recognised over an older one still draining.
///
/// Ordered so a [`Cell`] can tell a superseded write from a current one. Minting one
/// is `Core::refresh`'s job elsewhere; this crate only compares them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Generation(u64);

impl Generation {
    /// Wraps a raw counter value. `Core::refresh` is the one minter of a real one.
    pub(crate) fn new(value: u64) -> Self {
        Generation(value)
    }
}

/// A wall-clock moment, RFC 3339 on request via [`std::fmt::Display`].
///
/// Never a monotonic instant and never a raw `SystemTime` on the surface.
/// Supersession arbitrates entirely on [`Generation`], never on this, so a clock
/// that jumps backwards is not guarded against here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp(SystemTime);

impl Timestamp {
    /// The current wall-clock time.
    pub fn now() -> Self {
        Timestamp(SystemTime::now())
    }

    /// Wraps an arbitrary point in time. `now` is every real caller's constructor;
    /// this exists so a consumer's test can build a `Timestamp` in the future, the
    /// only way to exercise a backward clock jump deterministically.
    ///
    /// Gated behind `test-util` (on by default under `cfg(test)` for this crate's own
    /// tests) so a test-only affordance never ships on the default published surface,
    /// per [ADR 0021](https://github.com/paulchiu/repon/blob/main/docs/adr/0021-a-release-is-what-the-tag-pipeline-publishes.md).
    #[cfg(any(test, feature = "test-util"))]
    pub fn at(instant: SystemTime) -> Self {
        Timestamp(instant)
    }

    /// How long ago this timestamp was, against the wall clock right now.
    ///
    /// A future `self`, the shape a backward clock jump leaves behind, has no
    /// negative `Duration` to report, so this reads zero rather than erring: a
    /// reader sees "just now" with no defensive clamp layered on top.
    pub fn elapsed(&self) -> Duration {
        SystemTime::now()
            .duration_since(self.0)
            .unwrap_or(Duration::ZERO)
    }
}

impl std::fmt::Display for Timestamp {
    /// Formats as RFC 3339 (`2026-08-30T12:34:56Z`), computed by hand: the
    /// dependency allowlist has no time crate to reach for.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secs = self
            .0
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs() as i64;
        let days = secs.div_euclid(86_400);
        let secs_of_day = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let hour = secs_of_day / 3_600;
        let minute = (secs_of_day % 3_600) / 60;
        let second = secs_of_day % 60;
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
        )
    }
}

/// Days since the Unix epoch to a proleptic Gregorian (year, month, day).
///
/// Howard Hinnant's `civil_from_days` (<https://howardhinnant.github.io/date_algorithms.html>),
/// chosen so this crate never needs a calendar dependency for one field's display.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let day_of_era = z - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let month_prime = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32; // [1, 31]
    let month = (if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    }) as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// Why a [`Cell`] is [`Settled::Unknown`]. Closed at exactly these three: every other
/// absence this design once modelled as Unknown turned out to be a settled value
/// rendered elsewhere (a branch with no upstream renders `-`, a Repo with no remote
/// renders `∅`), so no `NoUpstream` or `NoRemote` reason exists here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unknown {
    /// The Generation hit its deadline while this cell was still being probed.
    TimedOut,
    /// The default branch resolution chain reached its last rung with no answer.
    NoDefaultBranch,
    /// A Submodule has never been `git submodule update --init`-ed, so opening it
    /// found nothing there rather than something broken
    /// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
    /// "The Submodule row").
    SubmoduleUninitialized,
}

/// What a [`Cell`] has settled to. Never a bare `Option<T>`, so an absent value can
/// never be read as a default.
#[derive(Debug, Clone)]
pub enum Settled<T> {
    /// Asked and got nothing back, for one of [`Unknown`]'s closed reasons.
    Unknown(Unknown),
    /// A value as of `at`; `stale` means known to be old with nothing currently
    /// fixing it.
    Known {
        value: T,
        at: Timestamp,
        stale: bool,
    },
    /// The probe itself failed.
    Failed(ProbeError),
    /// A settled fact rather than a missing value: this column has no meaning for
    /// this row. Its three producers are Worktree state on a Repo row, `base` on a
    /// row whose branch is itself the default branch, and `base` on a Repo with no
    /// remote.
    NotApplicable,
}

/// A displayed value together with its whole provenance.
///
/// Every field is private; the only way out is [`Cell::settled`] plus a match, so an
/// absent value can never be read as a default. `in_flight` is orthogonal to
/// `settled` rather than a fifth [`Settled`] arm, which is what lets a re-probing
/// cell keep its previous value instead of blanking. `settled` being `None` while
/// `in_flight` is `false` is a cell nothing has looked at yet, only reachable before
/// the first Generation covers the entity.
#[derive(Debug, Clone)]
pub struct Cell<T> {
    settled: Option<Settled<T>>,
    in_flight: bool,
    #[allow(dead_code)] // read only by settle's own supersession check for now
    generation: Generation,
}

impl<T> Default for Cell<T> {
    fn default() -> Self {
        Cell {
            settled: None,
            in_flight: false,
            generation: Generation::default(),
        }
    }
}

impl<T> Cell<T> {
    /// The settled state, or `None` while absent (loading, or never yet probed).
    /// The only way to a `T`.
    pub fn settled(&self) -> Option<&Settled<T>> {
        self.settled.as_ref()
    }

    /// Whether a probe is running against this cell right now. A row's summary
    /// treats in-flight as a property that outranks its least-settled Cell.
    pub fn is_in_flight(&self) -> bool {
        self.in_flight
    }

    /// Marks a probe as started, leaving any previous settled value untouched.
    pub(crate) fn begin_probe(&mut self) {
        self.in_flight = true;
    }

    /// Records one probe's result for `generation`; dropped without effect if a
    /// later Generation has already written this cell, which is the write-time half
    /// of supersession. Returns whether the write was applied, which is what lets a
    /// caller update entity-level diagnostics (not themselves a Cell, so not
    /// self-superseding) only on the write that actually won.
    pub(crate) fn settle(&mut self, generation: Generation, settled: Settled<T>) -> bool {
        if generation < self.generation {
            return false;
        }
        self.generation = generation;
        self.settled = Some(settled);
        self.in_flight = false;
        true
    }

    /// Marks a `Known` value stale in place, keeping its value and timestamp. A
    /// no-op on every other shape: `Unknown`, `Failed` and `NotApplicable` carry
    /// no staleness of their own, and a cell nothing has looked at yet has no
    /// value to mark old. This is what a Vanished Entity forces on every cell it
    /// holds, never blanking the last known values.
    pub(crate) fn force_stale(&mut self) {
        if let Some(Settled::Known {
            stale,
            value: _,
            at: _,
        }) = &mut self.settled
        {
            *stale = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Pins [`Unknown`] to the table in `docs/spec/core-api.md`, the document that calls
    /// this set closed. Without it the enum and the document drift apart silently, which is
    /// how the third reason arrived with both the document and ADR 0013 still saying two.
    #[test]
    fn unknown_reasons_match_this_documents_own_table() {
        let spec_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec/core-api.md");
        let spec = std::fs::read_to_string(&spec_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", spec_path.display()));

        let declaration = spec
            .lines()
            .find(|line| line.starts_with("pub enum Unknown {"))
            .unwrap_or_else(|| panic!("no `pub enum Unknown` line in {}", spec_path.display()));
        let documented: Vec<&str> = declaration
            .trim_start_matches("pub enum Unknown {")
            .trim_end_matches('}')
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();

        // Exhaustive rather than a list: a new variant fails to compile here, so this test
        // cannot fall behind the enum the way the document did.
        let in_code: Vec<&str> = [
            Unknown::TimedOut,
            Unknown::NoDefaultBranch,
            Unknown::SubmoduleUninitialized,
        ]
        .iter()
        .map(|reason| match reason {
            Unknown::TimedOut => "TimedOut",
            Unknown::NoDefaultBranch => "NoDefaultBranch",
            Unknown::SubmoduleUninitialized => "SubmoduleUninitialized",
        })
        .collect();

        assert_eq!(
            in_code, documented,
            "`Unknown`'s variants and core-api.md's own enum line disagree; amend the \
             document's table and its closed-set sentence in the same change as the enum"
        );
        for reason in &in_code {
            assert!(
                spec.contains(&format!("| `{reason}` |")),
                "core-api.md's reason table has no row for `{reason}`"
            );
        }
    }

    #[test]
    fn re_probing_keeps_the_previous_value_instead_of_blanking() {
        let mut cell: Cell<u32> = Cell::default();
        cell.settle(
            Generation::new(1),
            Settled::Known {
                value: 7,
                at: Timestamp::now(),
                stale: false,
            },
        );

        cell.begin_probe();

        match cell.settled() {
            Some(Settled::Known {
                value,
                at: _,
                stale: _,
            }) => assert_eq!(*value, 7),
            other => {
                panic!("expected the previous Known value to survive a re-probe, got {other:?}")
            }
        }
    }

    #[test]
    fn absent_before_any_probe_is_distinct_from_absent_while_loading() {
        let never_probed: Cell<u32> = Cell::default();
        assert!(never_probed.settled().is_none());
        assert!(!never_probed.in_flight);

        let mut loading: Cell<u32> = Cell::default();
        loading.begin_probe();
        assert!(loading.settled().is_none());
        assert!(loading.in_flight);
    }

    #[test]
    fn a_lower_generation_write_does_not_overwrite_a_higher_one() {
        let mut cell: Cell<u32> = Cell::default();
        cell.settle(
            Generation::new(2),
            Settled::Known {
                value: 9,
                at: Timestamp::now(),
                stale: false,
            },
        );

        cell.settle(
            Generation::new(1),
            Settled::Known {
                value: 1,
                at: Timestamp::now(),
                stale: false,
            },
        );

        match cell.settled() {
            Some(Settled::Known {
                value,
                at: _,
                stale: _,
            }) => assert_eq!(*value, 9),
            other => panic!("expected the higher Generation's value to survive, got {other:?}"),
        }
    }

    #[test]
    fn every_settled_shape_round_trips_through_settle_and_settled() {
        let mut unknown_cell: Cell<u32> = Cell::default();
        unknown_cell.settle(Generation::new(1), Settled::Unknown(Unknown::TimedOut));
        assert!(matches!(
            unknown_cell.settled(),
            Some(Settled::Unknown(Unknown::TimedOut))
        ));

        let mut failed_cell: Cell<u32> = Cell::default();
        failed_cell.settle(
            Generation::new(1),
            Settled::Failed(ProbeError::Open(Arc::from("boom"))),
        );
        assert!(matches!(
            failed_cell.settled(),
            Some(Settled::Failed(ProbeError::Open(_)))
        ));

        let mut not_applicable_cell: Cell<u32> = Cell::default();
        not_applicable_cell.settle(Generation::new(1), Settled::NotApplicable);
        assert!(matches!(
            not_applicable_cell.settled(),
            Some(Settled::NotApplicable)
        ));
    }

    #[test]
    fn force_stale_marks_a_known_value_stale_without_changing_it() {
        let mut cell: Cell<u32> = Cell::default();
        cell.settle(
            Generation::new(1),
            Settled::Known {
                value: 42,
                at: Timestamp::now(),
                stale: false,
            },
        );

        cell.force_stale();

        match cell.settled() {
            Some(Settled::Known {
                value,
                stale,
                at: _,
            }) => {
                assert_eq!(*value, 42, "the value must survive being forced stale");
                assert!(*stale, "the cell must be marked stale");
            }
            other => panic!("expected the Known value to survive, got {other:?}"),
        }
    }

    #[test]
    fn force_stale_on_a_cell_with_no_known_value_is_a_no_op() {
        let mut unknown_cell: Cell<u32> = Cell::default();
        unknown_cell.settle(Generation::new(1), Settled::Unknown(Unknown::TimedOut));
        unknown_cell.force_stale();
        assert!(matches!(
            unknown_cell.settled(),
            Some(Settled::Unknown(Unknown::TimedOut))
        ));

        let mut never_probed: Cell<u32> = Cell::default();
        never_probed.force_stale();
        assert!(never_probed.settled().is_none());
    }

    #[test]
    fn a_settled_cell_clones() {
        let mut cell: Cell<u32> = Cell::default();
        cell.settle(
            Generation::new(1),
            Settled::Known {
                value: 3,
                at: Timestamp::now(),
                stale: false,
            },
        );

        let cloned = cell.clone();

        match cloned.settled() {
            Some(Settled::Known {
                value,
                at: _,
                stale: _,
            }) => assert_eq!(*value, 3),
            other => panic!("expected the clone to carry the same Known value, got {other:?}"),
        }
    }

    #[test]
    fn elapsed_reads_zero_for_a_timestamp_in_the_future_rather_than_a_negative_duration() {
        let future = Timestamp::at(SystemTime::now() + Duration::from_secs(3600));

        assert_eq!(future.elapsed(), Duration::ZERO);
    }

    #[test]
    fn elapsed_reads_a_positive_duration_for_a_timestamp_in_the_past() {
        let past = Timestamp::at(SystemTime::now() - Duration::from_secs(90));

        assert!(past.elapsed() >= Duration::from_secs(90));
    }

    #[test]
    fn timestamp_formats_as_rfc3339() {
        let cases: [(u64, &str); 6] = [
            (0, "1970-01-01T00:00:00Z"),
            (1, "1970-01-01T00:00:01Z"),
            (86_399, "1970-01-01T23:59:59Z"),
            (86_400, "1970-01-02T00:00:00Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_700_000_000, "2023-11-14T22:13:20Z"),
        ];

        for (epoch_secs, expected) in cases {
            let timestamp = Timestamp(UNIX_EPOCH + Duration::from_secs(epoch_secs));
            assert_eq!(timestamp.to_string(), expected);
        }
    }
}
