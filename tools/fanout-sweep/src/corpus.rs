//! A synthetic corpus of throwaway git repositories, shaped after
//! `docs/spec/refresh.md`'s own population: mostly small and clean, a long tail of
//! heavier working trees, one or two outliers at 15,000-25,000 files (37-61% of
//! `vial-qmk`'s real 40,871, scaled down so building a corpus stays fast; a real
//! `vial-qmk` checkout is cross-checked separately, in isolation, by the single-repo
//! measurement in `docs/spec/refresh.md`), and a small dirty minority. Deterministic
//! from a seed, so two runs against the same `(entities, seed)` build the same shape
//! and are comparable.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::rng::Rng;

/// One entity's own shape before it is built: how many files its working tree holds, how
/// many extra commits follow the seed commit, and whether it is left dirty afterwards.
struct EntityPlan {
    name: String,
    file_count: usize,
    extra_commits: usize,
    dirty: bool,
}

/// A built corpus: the paths of every repository this call created, in the order they
/// were planned (outliers first), which is also the order a sweep dispatches in unless
/// the caller shuffles it.
pub struct Corpus {
    pub paths: Vec<PathBuf>,
    pub total_files: usize,
    pub dirty_count: usize,
}

/// Plans `entities` repositories across four tiers whose sizes are fractions of the
/// population rather than fixed counts, so the shape holds at any corpus size:
///
/// - outliers (roughly 1 in 150, at least one): 15,000-25,000 files, standing in for
///   `vial-qmk`.
/// - heavy (roughly 1 in 25): 1,500-6,000 files.
/// - medium (roughly 1 in 8): 150-800 files.
/// - the rest, small: 5-150 files, which is also where the 96%-clean ratio is spent,
///   since `refresh.md` measured the whole population that clean rather than only its
///   small end.
///
/// Roughly 4% of entities overall are left dirty (a mix of modified, untracked and
/// deleted paths against their last commit), matching the same figure read the other
/// way around.
fn plan(entities: usize, seed: u64) -> Vec<EntityPlan> {
    let mut rng = Rng::new(seed);
    let outlier_count = (entities / 150).max(1);
    let heavy_count = (entities / 25).max(if entities >= 25 { 1 } else { 0 });
    let medium_count = (entities / 8).max(if entities >= 8 { 1 } else { 0 });
    let small_count = entities.saturating_sub(outlier_count + heavy_count + medium_count);

    let mut plans = Vec::with_capacity(entities);
    let push_tier = |plans: &mut Vec<EntityPlan>,
                          rng: &mut Rng,
                          count: usize,
                          prefix: &str,
                          file_range: (usize, usize),
                          commit_range: (usize, usize)| {
        for i in 0..count {
            let dirty = rng.chance(0.04);
            plans.push(EntityPlan {
                name: format!("{prefix}-{i:04}"),
                file_count: rng.range(file_range.0, file_range.1),
                extra_commits: rng.range(commit_range.0, commit_range.1),
                dirty,
            });
        }
    };

    push_tier(
        &mut plans,
        &mut rng,
        outlier_count,
        "outlier",
        (15_000, 25_001),
        (2, 6),
    );
    push_tier(
        &mut plans,
        &mut rng,
        heavy_count,
        "heavy",
        (1_500, 6_001),
        (3, 12),
    );
    push_tier(
        &mut plans,
        &mut rng,
        medium_count,
        "medium",
        (150, 801),
        (3, 15),
    );
    push_tier(
        &mut plans,
        &mut rng,
        small_count,
        "small",
        (5, 151),
        (1, 8),
    );
    plans
}

/// Runs `git` against `path` with a fixed identity, so a commit never depends on
/// whatever identity (if any) is configured on the machine running the sweep.
fn git(path: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["-c", "user.email=fanout-sweep@example.com", "-c", "user.name=fanout-sweep"])
        .args(args)
        .status()
        .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
    assert!(status.success(), "git {args:?} failed in {}", path.display());
}

/// Spreads `count` files under `root` in a shallow tree rather than one flat directory,
/// since a real working tree is not one directory of forty thousand siblings: leaf
/// directories hold at most 200 files each, and the tree gains another directory level
/// once a single level would need more than 200 leaves.
fn write_files(root: &Path, count: usize, rng: &mut Rng) {
    const FILES_PER_DIR: usize = 200;
    let mut written = 0usize;
    let mut dir_index = 0usize;
    while written < count {
        let this_dir_count = FILES_PER_DIR.min(count - written);
        let dir = if count <= FILES_PER_DIR {
            root.to_path_buf()
        } else {
            let group = dir_index / FILES_PER_DIR;
            let leaf = dir_index % FILES_PER_DIR;
            root.join(format!("d{group:03}")).join(format!("d{leaf:03}"))
        };
        std::fs::create_dir_all(&dir).expect("create working-tree directory");
        for i in 0..this_dir_count {
            let name = format!("f{:06}.txt", written + i);
            let body = format!("seed {}\n", rng.range(0, 1_000_000));
            std::fs::write(dir.join(name), body).expect("write working-tree file");
        }
        written += this_dir_count;
        dir_index += 1;
    }
}

/// Touches a handful of already-written files under `root` (path recorded by
/// [`write_files`]'s own naming scheme) so an extra commit costs a few writes rather than
/// rewriting the whole tree, which is what keeps a heavy entity's commit depth cheap to
/// build.
fn touch_a_few_files(root: &Path, total_files: usize, rng: &mut Rng) {
    const FILES_PER_DIR: usize = 200;
    for _ in 0..5.min(total_files) {
        let index = rng.range(0, total_files);
        let path = if total_files <= FILES_PER_DIR {
            root.join(format!("f{index:06}.txt"))
        } else {
            let group = (index / FILES_PER_DIR) / FILES_PER_DIR;
            let leaf = (index / FILES_PER_DIR) % FILES_PER_DIR;
            root.join(format!("d{group:03}"))
                .join(format!("d{leaf:03}"))
                .join(format!("f{index:06}.txt"))
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, format!("touched {}\n", rng.range(0, 1_000_000)))
            .expect("touch working-tree file");
    }
}

/// Builds one repository at `root` per `plan`: an initial commit over its whole working
/// tree, `plan.extra_commits` small follow-up commits, then, if `plan.dirty`, a modified
/// file, an untracked file and a deleted file left uncommitted, so `DirtyCounts`'s three
/// fields are all non-zero on a dirty entity rather than only one of them.
fn build_repo(root: &Path, plan: &EntityPlan, rng: &mut Rng) {
    std::fs::create_dir_all(root).expect("create repo root");
    git(root, &["init", "--quiet", "--initial-branch=main"]);
    write_files(root, plan.file_count, rng);
    git(root, &["add", "-A"]);
    git(root, &["commit", "--quiet", "-m", "seed"]);

    for _ in 0..plan.extra_commits {
        touch_a_few_files(root, plan.file_count, rng);
        git(root, &["add", "-A"]);
        git(root, &["commit", "--quiet", "-m", "follow-up"]);
    }

    if plan.dirty {
        // Modified: rewrite an existing tracked file without committing it.
        touch_a_few_files(root, plan.file_count, rng);
        // Untracked: a new file `git add` never saw.
        std::fs::write(root.join("untracked.txt"), "never staged\n")
            .expect("write untracked file");
        // Deleted: remove a tracked file from the working tree only.
        let victim = root.join("f000000.txt");
        if victim.exists() {
            std::fs::remove_file(victim).expect("delete tracked file");
        }
    }
}

/// Builds a full corpus of `entities` repositories under `root` (which must already
/// exist and be empty), deterministic from `seed`. `root` is the caller's own
/// responsibility to place under a temp directory: this module never chooses one itself,
/// so a caller can never be tricked into pointing it at a real checkout.
pub fn build(root: &Path, entities: usize, seed: u64) -> Corpus {
    let plans = plan(entities, seed);
    let mut paths = Vec::with_capacity(plans.len());
    let mut total_files = 0;
    let mut dirty_count = 0;
    // Each repo gets its own RNG derived from the corpus seed and its own index, so
    // building repos in parallel (not done today, but a future change might) would still
    // reproduce byte-for-byte, and so that one entity's own file count does not perturb
    // another's file contents.
    for (index, entity_plan) in plans.iter().enumerate() {
        let mut repo_rng = Rng::new(seed ^ (index as u64).wrapping_mul(0x2545F4914F6CDD1D));
        let repo_root = root.join(&entity_plan.name);
        build_repo(&repo_root, entity_plan, &mut repo_rng);
        total_files += entity_plan.file_count;
        if entity_plan.dirty {
            dirty_count += 1;
        }
        paths.push(repo_root);
    }
    Corpus {
        paths,
        total_files,
        dirty_count,
    }
}
