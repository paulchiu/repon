//! Percentiles and spread, not just a mean: a single wall-clock figure from one run is
//! exactly the kind of evidence this tool exists to stop producing.

use std::time::Duration;

pub struct EntityStats {
    pub p50: Duration,
    pub p90: Duration,
    pub max: Duration,
    pub min: Duration,
    pub mean: Duration,
    pub samples: usize,
}

/// Percentiles over every per-entity duration pooled across all repeats of one
/// configuration, so a config run three times reports on 3x the samples rather than
/// throwing two thirds of them away.
pub fn entity_stats(mut durations: Vec<Duration>) -> EntityStats {
    assert!(
        !durations.is_empty(),
        "no per-entity durations to summarise"
    );
    durations.sort_unstable();
    let n = durations.len();
    let percentile = |p: f64| durations[((n - 1) as f64 * p).round() as usize];
    let total: Duration = durations.iter().sum();
    EntityStats {
        p50: percentile(0.50),
        p90: percentile(0.90),
        max: durations[n - 1],
        min: durations[0],
        mean: total / n as u32,
        samples: n,
    }
}

pub struct WallStats {
    pub median: Duration,
    pub min: Duration,
    pub max: Duration,
    pub runs: usize,
}

/// Spread across repeats of the same configuration's own wall clock, which is the
/// number a sweep grid actually compares row to row.
pub fn wall_stats(mut walls: Vec<Duration>) -> WallStats {
    assert!(!walls.is_empty(), "no wall-clock samples to summarise");
    walls.sort_unstable();
    let n = walls.len();
    WallStats {
        median: walls[n / 2],
        min: walls[0],
        max: walls[n - 1],
        runs: n,
    }
}
