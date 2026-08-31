//! The one document `Core::settle` feeds standard output: a schema integer at its root plus
//! the settled [`Snapshot`] itself, behind the `serde` feature and off by default.
//!
//! See `docs/spec/core-api.md`'s "The wire format" and
//! [ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md).

use serde::Serialize;

use crate::snapshot::Snapshot;

/// This wire format's own version. A schema-less shell script parsing the settled document
/// has no compiler to catch an enum variant it predates, so this integer is the one thing it
/// can check itself: bump it whenever [`crate::Settled`] or [`crate::Unknown`] gains or loses
/// a variant, in the same change that updates
/// `wire_schema_bump_discipline_matches_the_closed_variant_counts_below`'s own exhaustive
/// counts, which fails to compile on a variant this schema has not accounted for.
pub(crate) const SCHEMA: u32 = 1;

/// The whole settled table, tagged with the schema version that closes
/// [`crate::Settled`] and [`crate::Unknown`]'s variant sets. `settle`, then serialise this
/// once: `docs/spec/core-api.md`'s "The machine-readable consumer emits one settled document
/// rather than a stream".
///
/// Wraps [`Snapshot`] rather than restating its fields, so this crate's own `Serialize` derive
/// on `Snapshot` is the one definition of the settled table's shape on the wire, per ADR
/// 0015's "no separate consumer-side wire structs".
#[derive(Debug, Clone, Serialize)]
pub struct SettledDocument {
    pub schema: u32,
    #[serde(flatten)]
    pub snapshot: Snapshot,
}

impl SettledDocument {
    /// Tags `snapshot` with this crate's current `SCHEMA`.
    pub fn new(snapshot: Snapshot) -> Self {
        SettledDocument {
            schema: SCHEMA,
            snapshot,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::SCHEMA;
    use crate::cell::{Settled, Timestamp, Unknown};
    use crate::git::ProbeError;

    /// One value per [`Settled`] variant and one per [`Unknown`] reason: the two closed sets
    /// `docs/spec/core-api.md`'s wire format section calls "versioned by" [`SCHEMA`]. Each
    /// index function below is an exhaustive match with no wildcard arm, so a variant added to
    /// either enum fails to compile here until it is named; the constants are this file's own
    /// record of what `SCHEMA` was last bumped to cover, so add a matching example and bump
    /// both together.
    #[test]
    fn wire_schema_bump_discipline_matches_the_closed_variant_counts_below() {
        const SETTLED_VARIANT_COUNT: usize = 4;
        const UNKNOWN_REASON_COUNT: usize = 3;

        fn settled_variant_index(settled: &Settled<u32>) -> usize {
            match settled {
                Settled::Unknown(_) => 0,
                Settled::Known {
                    value: _,
                    at: _,
                    stale: _,
                } => 1,
                Settled::Failed(_) => 2,
                Settled::NotApplicable => 3,
            }
        }
        fn unknown_reason_index(reason: &Unknown) -> usize {
            match reason {
                Unknown::TimedOut => 0,
                Unknown::NoDefaultBranch => 1,
                Unknown::SubmoduleUninitialized => 2,
            }
        }

        let settled_examples: [Settled<u32>; SETTLED_VARIANT_COUNT] = [
            Settled::Unknown(Unknown::TimedOut),
            Settled::Known {
                value: 0,
                at: Timestamp::now(),
                stale: false,
            },
            Settled::Failed(ProbeError::Open(Arc::from("boom"))),
            Settled::NotApplicable,
        ];
        let mut settled_indices: Vec<usize> =
            settled_examples.iter().map(settled_variant_index).collect();
        settled_indices.sort_unstable();
        assert_eq!(
            settled_indices,
            (0..SETTLED_VARIANT_COUNT).collect::<Vec<_>>(),
            "a `Settled` variant is missing its own example above, or `SETTLED_VARIANT_COUNT` \
             is stale; bump `SCHEMA` in the same change that fixes this"
        );

        let unknown_examples: [Unknown; UNKNOWN_REASON_COUNT] = [
            Unknown::TimedOut,
            Unknown::NoDefaultBranch,
            Unknown::SubmoduleUninitialized,
        ];
        let mut unknown_indices: Vec<usize> =
            unknown_examples.iter().map(unknown_reason_index).collect();
        unknown_indices.sort_unstable();
        assert_eq!(
            unknown_indices,
            (0..UNKNOWN_REASON_COUNT).collect::<Vec<_>>(),
            "an `Unknown` reason is missing its own example above, or `UNKNOWN_REASON_COUNT` \
             is stale; bump `SCHEMA` in the same change that fixes this"
        );

        assert_eq!(
            SCHEMA, 1,
            "`SCHEMA` moved without this test's own counts being reviewed; update \
             `SETTLED_VARIANT_COUNT` and `UNKNOWN_REASON_COUNT` above for whatever changed, \
             then move this literal to match"
        );
    }
}
