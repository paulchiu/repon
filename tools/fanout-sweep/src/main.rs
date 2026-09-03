//! Corpus generator and sweep driver for the probe fan-out's width, built for
//! issue #361: nothing in `repon` ever chose that width, and no prior measurement varied
//! it. Lives outside the workspace (`tools/fanout-sweep/Cargo.toml` opens its own empty
//! `[workspace]`) so `cargo build --workspace`, `cargo clippy --workspace` and
//! `cargo test --workspace` never see it, and running the sweep is a separate `just`
//! recipe rather than a step in `just ci`.
//!
//! Three subcommands:
//!
//! - `synthetic`: builds a corpus in a fresh temp directory (per `corpus::build`'s
//!   shape) and sweeps `(pool_width, gix_thread_limit)` over it.
//! - `real`: the same sweep, read-only, over already-existing repositories the caller
//!   names explicitly. Takes no default path and reads no config, environment variable
//!   or working directory to find one: a root reaches this tool only on the command
//!   line, so the harness itself never depends on anyone's particular checkout. Reports
//!   its own tracked-file total in the same shape `synthetic` and `generate` report
//!   theirs, so the synthetic corpus's resemblance to the real population is something
//!   a reader can check against a number rather than take on the shape's say-so alone.
//! - `generate`: builds a corpus and leaves it on disk, for inspecting the shape by
//!   hand or reusing across several `real`-style runs without rebuilding it.

mod corpus;
mod probe;
mod rng;
mod stats;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

/// One cell of the sweep grid: a pool width paired with the gix thread limit each of its
/// workers is handed, plus how many idle busy-spin threads run alongside it to stand in
/// for a concurrent fetch or Action pool (docs/adr/0013's own observation that "width and
/// gix's own thread limit multiply" extends to a third pool sharing the same cores).
#[derive(Clone, Copy)]
struct Config {
    pool_width: usize,
    thread_limit: Option<usize>,
    contend: usize,
}

impl std::fmt::Display for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let limit = match self.thread_limit {
            Some(n) => n.to_string(),
            None => "none".to_string(),
        };
        write!(
            f,
            "width={} thread_limit={} contend={}",
            self.pool_width, limit, self.contend
        )
    }
}

/// Spins `count` OS threads on busy CPU work until `stop` reads true, standing in for a
/// concurrent Action or fetch pool's own workers competing for the same cores: those pools
/// are dedicated and do not share rayon's global pool, but they still take real cores away
/// from whichever pool the probe fan-out itself runs on.
fn spawn_contention(
    count: usize,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Vec<std::thread::JoinHandle<()>> {
    (0..count)
        .map(|_| {
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut x: u64 = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    std::hint::black_box(x);
                }
            })
        })
        .collect()
}

/// Runs one full pass over `paths` at `config`, on a dedicated pool built and torn down
/// for this call alone. This does not mirror production's own pool lifecycle:
/// `dispatch_probes`'s `rayon::spawn` (`crates/repon-core/src/core.rs`'s
/// probe-fanout-pool region) always targets rayon's single global pool, live for the
/// whole process, never a fresh pool per generation or one built and torn down per call.
/// A dedicated pool here is what lets `pool_width` be swept as a controlled axis at
/// all; it mirrors production's per-entity task (`probe::probe_entity_task`), not its
/// pool's own lifetime. Returns the wall clock for the whole pass and every entity's own
/// elapsed time.
fn run_once(paths: &[PathBuf], config: Config) -> (Duration, Vec<Duration>) {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let contenders = spawn_contention(config.contend, std::sync::Arc::clone(&stop));

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.pool_width)
        .build()
        .expect("build the sweep's own dedicated pool");

    let start = Instant::now();
    let durations: Vec<Duration> = pool.install(|| {
        paths
            .par_iter()
            .map(|path| probe::probe_entity_task(path, config.thread_limit))
            .collect()
    });
    let wall = start.elapsed();

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for handle in contenders {
        handle.join().expect("join a contention thread");
    }

    (wall, durations)
}

/// Runs `config` `repeats` times, pooling every repeat's per-entity durations for the
/// percentile report and keeping each repeat's own wall clock for the variance report,
/// per the standing rule that a single run is not evidence.
fn run_config(
    paths: &[PathBuf],
    config: Config,
    repeats: usize,
) -> (stats::WallStats, stats::EntityStats) {
    let mut walls = Vec::with_capacity(repeats);
    let mut all_durations = Vec::new();
    for _ in 0..repeats {
        let (wall, durations) = run_once(paths, config);
        walls.push(wall);
        all_durations.extend(durations);
    }
    (stats::wall_stats(walls), stats::entity_stats(all_durations))
}

fn print_header() {
    println!(
        "{:<8} {:<12} {:<8} | {:>10} {:>10} {:>10} | {:>9} {:>9} {:>9} {:>9}",
        "width",
        "thread_lim",
        "contend",
        "wall_med",
        "wall_min",
        "wall_max",
        "p50",
        "p90",
        "max",
        "min"
    );
}

fn print_row(config: Config, wall: &stats::WallStats, entity: &stats::EntityStats) {
    let limit = match config.thread_limit {
        Some(n) => n.to_string(),
        None => "none".to_string(),
    };
    println!(
        "{:<8} {:<12} {:<8} | {:>10?} {:>10?} {:>10?} | {:>9?} {:>9?} {:>9?} {:>9?}",
        config.pool_width,
        limit,
        config.contend,
        wall.median,
        wall.min,
        wall.max,
        entity.p50,
        entity.p90,
        entity.max,
        entity.min
    );
}

fn write_csv(out: &Path, rows: &[(Config, stats::WallStats, stats::EntityStats)]) {
    let mut body = String::from(
        "pool_width,thread_limit,contend,repeats,wall_median_ms,wall_min_ms,wall_max_ms,entity_samples,entity_p50_ms,entity_p90_ms,entity_max_ms,entity_min_ms,entity_mean_ms\n",
    );
    for (config, wall, entity) in rows {
        let limit = match config.thread_limit {
            Some(n) => n.to_string(),
            None => "none".to_string(),
        };
        body.push_str(&format!(
            "{},{},{},{},{:.3},{:.3},{:.3},{},{:.3},{:.3},{:.3},{:.3},{:.3}\n",
            config.pool_width,
            limit,
            config.contend,
            wall.runs,
            wall.median.as_secs_f64() * 1000.0,
            wall.min.as_secs_f64() * 1000.0,
            wall.max.as_secs_f64() * 1000.0,
            entity.samples,
            entity.p50.as_secs_f64() * 1000.0,
            entity.p90.as_secs_f64() * 1000.0,
            entity.max.as_secs_f64() * 1000.0,
            entity.min.as_secs_f64() * 1000.0,
            entity.mean.as_secs_f64() * 1000.0,
        ));
    }
    std::fs::write(out, body).unwrap_or_else(|error| panic!("write {}: {error}", out.display()));
}

fn parse_widths(s: &str) -> Vec<usize> {
    s.split(',')
        .map(|p| p.trim().parse().expect("integer pool width"))
        .collect()
}

fn parse_thread_limits(s: &str) -> Vec<Option<usize>> {
    s.split(',')
        .map(|p| {
            let p = p.trim();
            if p.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(p.parse().expect("integer thread limit or 'none'"))
            }
        })
        .collect()
}

struct SweepArgs {
    widths: Vec<usize>,
    thread_limits: Vec<Option<usize>>,
    repeats: usize,
    contend: Vec<usize>,
    out: Option<PathBuf>,
}

fn parse_sweep_args(args: &[String]) -> SweepArgs {
    let mut widths = vec![1, 2, 4, 8, 12, 18, 36];
    let mut thread_limits = vec![Some(1), Some(2), Some(4), None];
    let mut repeats = 3;
    let mut contend = vec![0];
    let mut out = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--widths" => {
                widths = parse_widths(&args[i + 1]);
                i += 2;
            }
            "--thread-limits" => {
                thread_limits = parse_thread_limits(&args[i + 1]);
                i += 2;
            }
            "--repeats" => {
                repeats = args[i + 1].parse().expect("integer repeat count");
                i += 2;
            }
            "--contend" => {
                contend = parse_widths(&args[i + 1]);
                i += 2;
            }
            "--out" => {
                out = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            other => panic!("unrecognised sweep argument: {other}"),
        }
    }
    SweepArgs {
        widths,
        thread_limits,
        repeats,
        contend,
        out,
    }
}

fn run_sweep(paths: &[PathBuf], args: &SweepArgs) {
    let mut rows = Vec::new();
    print_header();
    for &contend in &args.contend {
        for &pool_width in &args.widths {
            for &thread_limit in &args.thread_limits {
                let config = Config {
                    pool_width,
                    thread_limit,
                    contend,
                };
                let (wall, entity) = run_config(paths, config, args.repeats);
                print_row(config, &wall, &entity);
                rows.push((config, wall, entity));
            }
        }
    }
    if let Some(out) = &args.out {
        write_csv(out, &rows);
        println!("wrote {}", out.display());
    }
}

/// A directory name this walk refuses to enter, ever, regardless of `--roots`: ADR 0003 is
/// a clean-room decision about the repository named `mrx`, and a read-only sweep is not an
/// exception to it. Matched case-insensitively against every path component, not only a
/// root's immediate children, since a clean-room boundary is not something a nested
/// checkout gets to sit inside of either.
fn is_forbidden_dir_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|s| s.eq_ignore_ascii_case("mrx"))
}

/// Walks `root` for repository boundaries, matching `crates/repon-core/src/discovery.rs`'s
/// own `is_boundary` rule exactly: "a directory is a boundary when it holds a `.git`
/// entry, file or directory form alike" (`dir.join(".git").exists()`), which is what
/// also counts a linked worktree or a submodule's own checkout, both of which point at
/// their common dir through a `.git` *file* rather than a directory. A recursive
/// subdirectory this process cannot read is skipped silently and the walk continues
/// past it, the same as a real corpus walk tolerating one unreadable nested directory
/// (a permissions-locked cache dir, say) without aborting the whole scan; only a
/// top-level `--roots` entry gets the loud treatment, in `main`, since a typo or an
/// unexpanded `~` there should never be swallowed into a silently smaller corpus.
fn find_git_repos(root: &Path, max_depth: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= 100_000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        if is_forbidden_dir_name(&name) {
            continue;
        }
        if name == ".git" {
            // A `.git` entry directly inside `root`, file or directory alike, means
            // `root` itself is a repository boundary: reached when a `--roots` entry
            // is itself a repository, or a linked worktree's own `.git` file is seen
            // while iterating its parent's children.
            out.push(root.to_path_buf());
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        if name == "node_modules" {
            continue;
        }
        if path.join(".git").exists() {
            out.push(path.clone());
            // A repository boundary stops the walk, matching discovery's own
            // boundary-stop rule: this tool never descends into a repo it already found.
            continue;
        }
        if max_depth > 0 {
            find_git_repos(&path, max_depth - 1, out);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        eprintln!("usage: fanout-sweep <synthetic|real|generate> [args]");
        std::process::exit(2);
    };
    let rest = &args[1..];

    match command.as_str() {
        "generate" => {
            let mut entities = 150usize;
            let mut seed = 1u64;
            let mut out = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--entities" => {
                        entities = rest[i + 1].parse().expect("integer entity count");
                        i += 2;
                    }
                    "--seed" => {
                        seed = rest[i + 1].parse().expect("integer seed");
                        i += 2;
                    }
                    "--out" => {
                        out = Some(PathBuf::from(&rest[i + 1]));
                        i += 2;
                    }
                    other => panic!("unrecognised generate argument: {other}"),
                }
            }
            let root = out.unwrap_or_else(|| {
                std::env::temp_dir().join(format!("fanout-sweep-corpus-{seed}-{entities}"))
            });
            std::fs::create_dir_all(&root).expect("create corpus root");
            let started = Instant::now();
            let built = corpus::build(&root, entities, seed);
            println!(
                "built {} entities, {} files total, {} dirty, in {:?} at {}",
                built.paths.len(),
                built.total_files,
                built.dirty_count,
                started.elapsed(),
                root.display()
            );
        }
        "synthetic" => {
            let mut entities = 150usize;
            let mut seed = 1u64;
            let mut sweep_args_raw = Vec::new();
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--entities" => {
                        entities = rest[i + 1].parse().expect("integer entity count");
                        i += 2;
                    }
                    "--seed" => {
                        seed = rest[i + 1].parse().expect("integer seed");
                        i += 2;
                    }
                    other => {
                        sweep_args_raw.push(other.to_string());
                        sweep_args_raw.push(rest[i + 1].clone());
                        i += 2;
                    }
                }
            }
            let sweep_args = parse_sweep_args(&sweep_args_raw);
            let root = std::env::temp_dir().join(format!("fanout-sweep-corpus-{seed}-{entities}"));
            std::fs::create_dir_all(&root).expect("create corpus root");
            let started = Instant::now();
            let built = corpus::build(&root, entities, seed);
            println!(
                "built {} entities, {} files total, {} dirty, in {:?} at {}",
                built.paths.len(),
                built.total_files,
                built.dirty_count,
                started.elapsed(),
                root.display()
            );
            run_sweep(&built.paths, &sweep_args);
            std::fs::remove_dir_all(&root).ok();
        }
        "real" => {
            let mut roots: Vec<PathBuf> = Vec::new();
            let mut limit = usize::MAX;
            let mut max_depth = 6usize;
            let mut sweep_args_raw = Vec::new();
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--roots" => {
                        roots = rest[i + 1].split(',').map(PathBuf::from).collect();
                        i += 2;
                    }
                    "--limit" => {
                        limit = rest[i + 1].parse().expect("integer limit");
                        i += 2;
                    }
                    "--max-depth" => {
                        max_depth = rest[i + 1].parse().expect("integer depth");
                        i += 2;
                    }
                    other => {
                        sweep_args_raw.push(other.to_string());
                        sweep_args_raw.push(rest[i + 1].clone());
                        i += 2;
                    }
                }
            }
            assert!(!roots.is_empty(), "real needs at least one --roots entry");
            for root in &roots {
                assert!(
                    !root
                        .components()
                        .any(|c| is_forbidden_dir_name(c.as_os_str())),
                    "refusing a --roots entry that names or contains `mrx`: {}",
                    root.display()
                );
                // Loud rather than silently dropped: `find_git_repos`'s own recursive
                // `read_dir` failures are tolerated (a locked-down subdirectory is a
                // normal thing to meet deep in a real tree), but a `--roots` entry
                // itself failing to read is never that: it is either a typo or, as
                // happened in practice, a shell that only tilde-expanded the first of
                // several comma-joined roots and passed the rest through as a literal
                // `~`. A truncated `--roots` list must never look like a smaller but
                // otherwise valid real-corpus run.
                assert!(
                    root.is_dir(),
                    "--roots entry is not a readable directory: {} (an unexpanded `~` \
                     past the first comma-joined root is a common cause; pass roots as \
                     separate shell words, e.g. `just sweep-fanout-real ~/dev ~/dev-misc`)",
                    root.display()
                );
            }
            let sweep_args = parse_sweep_args(&sweep_args_raw);
            let mut paths = Vec::new();
            for root in &roots {
                let before = paths.len();
                find_git_repos(root, max_depth, &mut paths);
                println!(
                    "found {} repositories under {}",
                    paths.len() - before,
                    root.display()
                );
            }
            paths.truncate(limit);
            println!(
                "found {} repositories under {} root(s)",
                paths.len(),
                roots.len()
            );
            // Tracked-file count only, read from each repo's own index rather than a
            // full status walk: enough to compare the real population's shape against
            // `synthetic`'s own reported "files total" figure without paying for a
            // second status pass this sweep already runs per config. Working-tree
            // dirtiness is not summarised here for the same reason: it is exactly what
            // the sweep below measures, on every repo, at every cell.
            let total_tracked: usize = paths
                .iter()
                .filter_map(|path| gix::open(path).ok())
                .filter_map(|repo| repo.index().ok())
                .map(|index| index.entries().len())
                .sum();
            println!("{total_tracked} tracked files total across the real population");
            // Printed because the sweep does not measure these repositories the way
            // production would: `probe`'s module doc explains that phases A and B are
            // always timed on the "no remote" path, so this count is the size of that
            // omission for this population, on the run's own output.
            let with_remote = paths
                .iter()
                .filter_map(|path| gix::open(path).ok())
                .filter(|repo| !repo.remote_names().is_empty())
                .count();
            println!(
                "{with_remote} of them carry a remote, whose sync and default-branch \
                 phases this sweep times on the no-remote path (see probe.rs)"
            );
            run_sweep(&paths, &sweep_args);
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            std::process::exit(2);
        }
    }
}
