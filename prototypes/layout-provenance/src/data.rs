//! Fake population. No git, no filesystem, no network: the shapes and the timings only.

/// The five states from ADR 0001. `Stale` and `Fresh` carry the age the UI is allowed to show.
#[derive(Clone, Debug)]
pub enum Prov<T> {
    Unknown,
    Loading,
    Fresh(T, u64),
    Stale(T, u64),
    Failed(&'static str),
}

impl<T> Prov<T> {
    pub fn age_secs(&self) -> Option<u64> {
        match self {
            Prov::Fresh(_, a) | Prov::Stale(_, a) => Some(*a),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Repo,
    Worktree,
    Submodule,
}

#[derive(Clone, Copy, PartialEq)]
pub enum WtState {
    Merged,
    Gone,
    LocalOnly,
    Active,
}

impl WtState {
    pub fn label(self) -> &'static str {
        match self {
            WtState::Merged => "merged",
            WtState::Gone => "gone",
            WtState::LocalOnly => "local only",
            WtState::Active => "active",
        }
    }
}

/// What a probe eventually settles on. Separate from `Prov` so a refresh can replay the
/// same arrival twice and the row can hold a previous value while the new one lands.
#[derive(Clone)]
pub struct Settled {
    pub branch: Option<&'static str>,
    pub sync: Option<(u32, u32)>,
    pub dirty: Option<u32>,
    pub state: Option<WtState>,
    /// Set when the whole entity fails to open, e.g. a broken vendored checkout.
    pub fails: Option<&'static str>,
    /// Set when the probe succeeds but there is genuinely nothing to report, e.g. no upstream.
    pub no_upstream: bool,
}

pub struct Entity {
    pub name: &'static str,
    pub kind: Kind,
    pub parent: Option<&'static str>,
    pub settled: Settled,
    /// Milliseconds after a refresh starts at which the slow probe (status, ahead/behind) lands.
    /// Branch lands at a tenth of this, matching the 10ms-open against 94ms-status split
    /// measured in the gix benchmark.
    pub slow_ms: u64,
    pub commits: &'static [(&'static str, &'static str)],
}

impl Entity {
    pub fn fast_ms(&self) -> u64 {
        self.slow_ms / 10
    }
}

const LOG_A: &[(&str, &str)] = &[
    ("4f1a09c", "Rename the settlement worker's retry window"),
    ("b7c2e10", "Drop the unused ledger index"),
    (
        "18de334",
        "Take the currency code from the account, not the request",
    ),
];
const LOG_B: &[(&str, &str)] = &[
    ("9ab7712", "Split the checkout reducer per step"),
    ("2c40f8e", "Stop double-firing the analytics event"),
];
const LOG_C: &[(&str, &str)] = &[("0d5591a", "Initial import")];

fn repo(
    name: &'static str,
    branch: &'static str,
    sync: (u32, u32),
    dirty: u32,
    slow_ms: u64,
    commits: &'static [(&'static str, &'static str)],
) -> Entity {
    Entity {
        name,
        kind: Kind::Repo,
        parent: None,
        settled: Settled {
            branch: Some(branch),
            sync: Some(sync),
            dirty: Some(dirty),
            state: None,
            fails: None,
            no_upstream: false,
        },
        slow_ms,
        commits,
    }
}

fn wt(
    name: &'static str,
    parent: &'static str,
    branch: &'static str,
    sync: (u32, u32),
    dirty: u32,
    state: WtState,
    slow_ms: u64,
) -> Entity {
    Entity {
        name,
        kind: Kind::Worktree,
        parent: Some(parent),
        settled: Settled {
            branch: Some(branch),
            sync: Some(sync),
            dirty: Some(dirty),
            state: Some(state),
            fails: None,
            no_upstream: false,
        },
        slow_ms,
        commits: LOG_B,
    }
}

/// Roughly the shape of one Set at the measured scale, with the awkward cases deliberately
/// clustered near the top so they are on screen without scrolling.
pub fn population() -> Vec<Entity> {
    let mut v = vec![
        repo("acquiring-gateway", "main", (0, 0), 0, 180, LOG_A),
        wt(
            "fix/settlement-retry",
            "acquiring-gateway",
            "fix/settlement-retry",
            (3, 0),
            4,
            WtState::Active,
            2100,
        ),
        wt(
            "chore/bump-tonic",
            "acquiring-gateway",
            "chore/bump-tonic",
            (0, 0),
            0,
            WtState::Merged,
            240,
        ),
        wt(
            "spike/idempotency",
            "acquiring-gateway",
            "spike/idempotency",
            (0, 0),
            11,
            WtState::LocalOnly,
            310,
        ),
    ];

    // Opens, but the status probe never returns: the row that must not show a zero.
    v.push(Entity {
        name: "vendor/legacy-terminal-sdk",
        kind: Kind::Repo,
        parent: None,
        settled: Settled {
            branch: Some("master"),
            sync: None,
            dirty: None,
            state: None,
            fails: None,
            no_upstream: false,
        },
        slow_ms: 9_000,
        commits: LOG_C,
    });

    // Does not open at all. One of these existed in the real population.
    v.push(Entity {
        name: "vendor/broken-checkout",
        kind: Kind::Repo,
        parent: None,
        settled: Settled {
            branch: None,
            sync: None,
            dirty: None,
            state: None,
            fails: Some("could not read HEAD"),
            no_upstream: false,
        },
        slow_ms: 90,
        commits: &[],
    });

    // Opens fine, has no upstream. 229 of 441 measured entities looked like this.
    v.push(Entity {
        name: "scratch/perf-notes",
        kind: Kind::Repo,
        parent: None,
        settled: Settled {
            branch: Some("main"),
            sync: None,
            dirty: Some(2),
            state: None,
            fails: None,
            no_upstream: true,
        },
        slow_ms: 130,
        commits: LOG_C,
    });

    v.push(Entity {
        name: "acquiring-gateway/protos",
        kind: Kind::Submodule,
        parent: Some("acquiring-gateway"),
        settled: Settled {
            branch: Some("v3"),
            sync: Some((0, 12)),
            dirty: Some(0),
            state: None,
            fails: None,
            no_upstream: false,
        },
        slow_ms: 400,
        commits: LOG_C,
    });

    let bulk: &[(&str, &str, (u32, u32), u32, u64)] = &[
        ("checkout-web", "main", (0, 2), 0, 220),
        ("checkout-web-e2e", "main", (0, 0), 1, 260),
        ("ledger-core", "main", (1, 0), 0, 640),
        ("ledger-projections", "main", (0, 0), 0, 300),
        ("merchant-portal", "develop", (0, 41), 7, 880),
        ("merchant-portal-design", "main", (0, 0), 0, 150),
        ("payouts-scheduler", "main", (0, 0), 0, 200),
        ("payouts-rules", "main", (2, 2), 0, 210),
        ("risk-scoring", "main", (0, 5), 3, 1_400),
        ("risk-features", "main", (0, 0), 0, 190),
        ("terminal-firmware", "trunk", (0, 0), 0, 2_400),
        ("terminal-provisioning", "main", (0, 1), 0, 230),
        ("infra-terraform", "main", (0, 9), 22, 760),
        ("infra-runbooks", "main", (0, 0), 0, 120),
        ("shared-proto", "main", (0, 0), 0, 140),
        ("shared-eslint", "main", (0, 3), 0, 160),
        ("docs-platform", "main", (0, 0), 0, 170),
        ("onboarding-api", "main", (5, 0), 1, 520),
        ("onboarding-web", "main", (0, 0), 0, 280),
        ("recon-batch", "main", (0, 0), 0, 340),
        ("recon-reports", "main", (0, 7), 0, 360),
        ("fraud-rules-dsl", "main", (0, 0), 0, 250),
        ("notifications", "main", (0, 0), 2, 290),
        ("webhooks-relay", "main", (0, 0), 0, 210),
    ];
    for (name, branch, sync, dirty, ms) in bulk {
        v.push(repo(name, branch, *sync, *dirty, *ms, LOG_A));
    }

    v.push(wt(
        "release/2026-08",
        "ledger-core",
        "release/2026-08",
        (0, 0),
        0,
        WtState::Gone,
        420,
    ));
    v.push(wt(
        "fix/rounding",
        "ledger-core",
        "fix/rounding",
        (1, 3),
        2,
        WtState::Active,
        480,
    ));
    v.push(wt(
        "old/pos-discount",
        "merchant-portal",
        "old/pos-discount",
        (0, 0),
        0,
        WtState::Merged,
        390,
    ));
    v.push(wt(
        "wip/darkmode",
        "merchant-portal",
        "wip/darkmode",
        (0, 0),
        14,
        WtState::LocalOnly,
        410,
    ));
    v.push(wt(
        "hotfix/tls-pin",
        "terminal-firmware",
        "hotfix/tls-pin",
        (0, 0),
        0,
        WtState::Gone,
        1_900,
    ));
    v
}
