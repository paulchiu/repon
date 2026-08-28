//! PROTOTYPE, throwaway. Answers one question: does gix hold up across a real repo
//! population, and does it cover the four bulk reads Repon lives on?
//!
//! Not a UI prototype and not a logic prototype (the two shapes the prototype skill
//! names); the question here is "will this library carry the load", so it is a spike
//! that measures. No polish, no error handling beyond what keeps it running.

use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Default, Debug, Clone)]
struct Probe {
    path: PathBuf,
    is_worktree: bool,
    open_us: u128,
    branch: Option<String>,
    branch_err: Option<String>,
    dirty: Option<(usize, usize, usize)>, // modified, untracked, deleted
    dirty_err: Option<String>,
    dirty_us: u128,
    ahead_behind: Option<(usize, usize)>,
    ab_err: Option<String>,
    ab_us: u128,
    total_us: u128,
}

fn discover(roots: &[PathBuf]) -> Vec<(PathBuf, bool)> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = roots.to_vec();
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name == "node_modules" || name == "target" {
                continue;
            }
            if name == ".git" {
                let is_wt = p.is_file();
                out.push((dir.clone(), is_wt));
                if std::env::var_os("STOP_AT_REPO").is_some() {
                    // Stop at the repo boundary: do not walk its working tree.
                    // Cheap, but blind to nested worktrees and submodules.
                    stack.retain(|q| !q.starts_with(&dir));
                    break;
                }
                continue;
            }
            if p.is_dir() && !p.is_symlink() {
                stack.push(p);
            }
        }
    }
    out
}

fn probe(path: &Path, is_worktree: bool) -> Probe {
    let t0 = Instant::now();
    let mut p = Probe {
        path: path.to_path_buf(),
        is_worktree,
        ..Default::default()
    };

    let t_open = Instant::now();
    let repo = match gix::open(path) {
        Ok(r) => r,
        Err(e) => {
            p.branch_err = Some(format!("open: {e}"));
            p.total_us = t0.elapsed().as_micros();
            return p;
        }
    };
    p.open_us = t_open.elapsed().as_micros();

    // (1) branch name
    match repo.head_name() {
        Ok(Some(n)) => p.branch = Some(n.shorten().to_string()),
        Ok(None) => p.branch = Some("(detached)".into()),
        Err(e) => p.branch_err = Some(format!("head_name: {e}")),
    }

    // (2) working-tree status, typed counts
    let t_dirty = Instant::now();
    match status_counts(&repo) {
        Ok(c) => p.dirty = Some(c),
        Err(e) => p.dirty_err = Some(e),
    }
    p.dirty_us = t_dirty.elapsed().as_micros();

    // (3) ahead/behind against the upstream tracking branch
    let t_ab = Instant::now();
    match ahead_behind(&repo) {
        Ok(Some(ab)) => p.ahead_behind = Some(ab),
        Ok(None) => {} // no upstream: legitimately Unknown, not an error
        Err(e) => p.ab_err = Some(e),
    }
    p.ab_us = t_ab.elapsed().as_micros();

    p.total_us = t0.elapsed().as_micros();
    p
}

fn status_counts(repo: &gix::Repository) -> Result<(usize, usize, usize), String> {
    let status = repo
        .status(gix::progress::Discard)
        .map_err(|e| format!("status platform: {e}"))?;
    let iter = status
        .into_iter(None)
        .map_err(|e| format!("status iter: {e}"))?;
    let (mut modified, mut untracked, mut deleted) = (0, 0, 0);
    for item in iter {
        let item = match item {
            Ok(i) => i,
            Err(e) => return Err(format!("status item: {e}")),
        };
        use gix::status::Item;
        match &item {
            Item::IndexWorktree(iw) => {
                use gix::status::index_worktree::Item as IwItem;
                match iw {
                    IwItem::DirectoryContents { .. } => untracked += 1,
                    IwItem::Modification { status, .. } => {
                        use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
                        match status {
                            EntryStatus::Change(Change::Removed) => deleted += 1,
                            EntryStatus::NeedsUpdate(_) => {}
                            _ => modified += 1,
                        }
                    }
                    IwItem::Rewrite { .. } => modified += 1,
                }
            }
            Item::TreeIndex(_) => modified += 1,
        }
    }
    Ok((modified, untracked, deleted))
}

fn ahead_behind(repo: &gix::Repository) -> Result<Option<(usize, usize)>, String> {
    let head_id = match repo.head_id() {
        Ok(id) => id.detach(),
        Err(_) => return Ok(None),
    };
    let name = match repo.head_name() {
        Ok(Some(n)) => n,
        _ => return Ok(None),
    };
    let upstream_name = match repo
        .branch_remote_tracking_ref_name(name.as_ref(), gix::remote::Direction::Fetch)
    {
        Some(Ok(n)) => n.to_owned(),
        _ => return Ok(None),
    };
    let upstream_id = match repo.find_reference(upstream_name.as_ref()) {
        Ok(mut r) => match r.peel_to_id() {
            Ok(id) => id.detach(),
            Err(e) => return Err(format!("peel upstream: {e}")),
        },
        Err(_) => return Ok(None),
    };
    if head_id == upstream_id {
        return Ok(Some((0, 0)));
    }
    let ahead = count_to_boundary(repo, head_id, upstream_id)?;
    let behind = count_to_boundary(repo, upstream_id, head_id)?;
    Ok(Some((ahead, behind)))
}

fn count_to_boundary(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    boundary: gix::ObjectId,
) -> Result<usize, String> {
    let walk = repo
        .rev_walk(Some(tip))
        .with_hidden(Some(boundary))
        .all()
        .map_err(|e| format!("rev_walk: {e}"))?;
    let mut n = 0usize;
    for c in walk {
        match c {
            Ok(_) => n += 1,
            Err(e) => return Err(format!("walk item: {e}")),
        }
    }
    Ok(n)
}

/// Same three reads via the git binary, for a like-for-like baseline.
fn probe_git(path: &Path) -> u128 {
    let t = Instant::now();
    let _ = std::process::Command::new("git")
        .args(["-C", &path.to_string_lossy(), "status", "--porcelain=v1", "--branch"])
        .output();
    t.elapsed().as_micros()
}

fn pct(mut v: Vec<u128>, p: f64) -> u128 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i]
}

fn ms(us: u128) -> String {
    format!("{:.1}ms", us as f64 / 1000.0)
}

fn main() {
    let roots: Vec<PathBuf> = std::env::args()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let roots = if roots.is_empty() {
        let home = std::env::var("HOME").unwrap();
        vec![
            PathBuf::from(format!("{home}/dev")),
            PathBuf::from(format!("{home}/dev-misc")),
        ]
    } else {
        roots
    };

    println!("PROTOTYPE gix benchmark. Roots: {roots:?}\n");

    let t = Instant::now();
    let found = discover(&roots);
    let discovery = t.elapsed();
    let repos = found.iter().filter(|(_, w)| !*w).count();
    let wts = found.iter().filter(|(_, w)| *w).count();
    println!(
        "discovery: {} entities ({} repo dirs, {} .git files) in {:?}\n",
        found.len(),
        repos,
        wts,
        discovery
    );

    // Parallel probe, the shape Repon would actually use.
    let t = Instant::now();
    let results: Vec<Probe> = found
        .par_iter()
        .map(|(p, w)| probe(p, *w))
        .collect();
    let wall_parallel = t.elapsed();

    // Serial, to show what the parallelism buys.
    let t = Instant::now();
    let serial_sample: Vec<Probe> = found
        .iter()
        .take(40)
        .map(|(p, w)| probe(p, *w))
        .collect();
    let wall_serial_40 = t.elapsed();

    let totals: Vec<u128> = results.iter().map(|r| r.total_us).collect();
    let dirties: Vec<u128> = results.iter().map(|r| r.dirty_us).collect();
    let abs: Vec<u128> = results.iter().map(|r| r.ab_us).collect();
    let opens: Vec<u128> = results.iter().map(|r| r.open_us).collect();

    println!("=== wall clock ===");
    println!("  parallel probe of all {}: {:?}", results.len(), wall_parallel);
    println!(
        "  serial probe of 40:          {:?}  (implies ~{:?} serial for all)",
        wall_serial_40,
        Duration::from_micros(
            (wall_serial_40.as_micros() as u64 / 40) * results.len() as u64
        )
    );
    let _ = serial_sample;

    println!("\n=== per-repo cost (parallel run) ===");
    println!(
        "  open      p50 {}  p95 {}  max {}",
        ms(pct(opens.clone(), 0.5)),
        ms(pct(opens.clone(), 0.95)),
        ms(pct(opens, 1.0))
    );
    println!(
        "  status    p50 {}  p95 {}  max {}",
        ms(pct(dirties.clone(), 0.5)),
        ms(pct(dirties.clone(), 0.95)),
        ms(pct(dirties, 1.0))
    );
    println!(
        "  ahead/beh p50 {}  p95 {}  max {}",
        ms(pct(abs.clone(), 0.5)),
        ms(pct(abs.clone(), 0.95)),
        ms(pct(abs, 1.0))
    );
    println!(
        "  total     p50 {}  p95 {}  max {}",
        ms(pct(totals.clone(), 0.5)),
        ms(pct(totals.clone(), 0.95)),
        ms(pct(totals.clone(), 1.0))
    );

    // "Feels instant" proxy: how long until the fastest 30 rows could be painted.
    let mut sorted = totals.clone();
    sorted.sort_unstable();
    println!(
        "\n  30th-fastest repo completes at {} of single-threaded work",
        ms(sorted.iter().take(30).sum::<u128>())
    );

    println!("\n=== coverage ===");
    let n = results.len();
    let branch_ok = results.iter().filter(|r| r.branch.is_some()).count();
    let dirty_ok = results.iter().filter(|r| r.dirty.is_some()).count();
    let ab_ok = results.iter().filter(|r| r.ahead_behind.is_some()).count();
    let ab_none = results
        .iter()
        .filter(|r| r.ahead_behind.is_none() && r.ab_err.is_none())
        .count();
    println!("  branch name resolved: {branch_ok}/{n}");
    println!("  status computed:      {dirty_ok}/{n}");
    println!("  ahead/behind computed: {ab_ok}/{n}  (no upstream, legitimately Unknown: {ab_none})");

    println!("\n=== failures (first 15 of each) ===");
    for (label, errs) in [
        ("branch", results.iter().filter_map(|r| r.branch_err.as_ref().map(|e| (&r.path, e))).collect::<Vec<_>>()),
        ("status", results.iter().filter_map(|r| r.dirty_err.as_ref().map(|e| (&r.path, e))).collect::<Vec<_>>()),
        ("ahead/behind", results.iter().filter_map(|r| r.ab_err.as_ref().map(|e| (&r.path, e))).collect::<Vec<_>>()),
    ] {
        println!("  {label}: {} failures", errs.len());
        for (p, e) in errs.iter().take(15) {
            println!("    {} :: {}", p.display(), e);
        }
    }

    // Slowest repos: the ones that would hold a table without a timeout.
    if std::env::var_os("GIT_BASELINE").is_some() {
        let t = Instant::now();
        let git_us: Vec<u128> = found.par_iter().map(|(p, _)| probe_git(p)).collect();
        let git_wall = t.elapsed();
        println!("\n=== git binary baseline (status --porcelain --branch) ===");
        println!("  parallel wall clock: {git_wall:?}   (gix: {wall_parallel:?})");
        println!(
            "  per-repo p50 {}  p95 {}  max {}",
            ms(pct(git_us.clone(), 0.5)),
            ms(pct(git_us.clone(), 0.95)),
            ms(pct(git_us, 1.0))
        );
    }

    println!("\n=== 10 slowest entities ===");
    let mut by_slow: Vec<&Probe> = results.iter().collect();
    by_slow.sort_by_key(|r| std::cmp::Reverse(r.total_us));
    for r in by_slow.iter().take(10) {
        println!(
            "  {:>9}  {} {}",
            ms(r.total_us),
            if r.is_worktree { "[wt]" } else { "    " },
            r.path.display()
        );
    }
}
