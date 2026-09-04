//! The fast-forward-only auto-update: the second and last mutating git operation this
//! program ever performs, alongside the periodic fetch's own fetch-and-prune. Isolated
//! behind the `fetch` cargo feature for the same reason `fetch.rs` is: it can only ever
//! act on what a fetch just learned, so a consumer that never turns fetching on pulls in
//! none of this either. See `docs/spec/config.md`'s "Refresh, fetch and auto-update" and
//! [ADR 0002](https://github.com/paulchiu/repon/blob/main/docs/adr/0002-repon-owns-the-outer-loop-only.md)'s
//! narrowest-safe-operation rule.
//!
//! ADR 0002 boundary: this module moves a branch ref and rewrites the paths a tree diff
//! names, nothing else. No commit, merge, rebase or reset ever runs here, or anywhere in
//! this program (`crates/repon/src/test_support.rs`'s
//! `no_push_commit_merge_rebase_or_reset_operation_exists_in_production_code` is the scan
//! that keeps that true); a Repo that cannot be moved this way is reported, by leaving its
//! true `sync` and `dirty` cells to say so on the next Generation, and never rebased or
//! merged to get it there anyway.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gix::bstr::{BStr, ByteSlice};
use gix::object::tree::diff::{Action, Change};
use gix::objs::tree::EntryKind;

use crate::entity::Head;
use crate::git;

/// Why a Repo was not eligible to move, per `docs/spec/config.md`'s eligibility rule:
/// clean, behind, not ahead and tracking an upstream. Not ahead and fast-forward-only
/// collapse to one reason, [`Ineligible::NotFastForward`]: with ahead/behind counted by
/// reachability (as [`git::ahead_behind`] does), `ahead == 0` and "the local tip is an
/// ancestor of the upstream tip" are the same fact, not two independent checks, so there
/// is no real git history that is "not ahead" yet also "not fast-forward-able".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ineligible {
    /// The working tree or the index carries a change the auto-update did not make.
    NotClean,
    /// No branch, no remote, or a branch with no upstream configured.
    NoUpstream,
    /// Already level with its upstream: nothing to move.
    NotBehind,
    /// The local branch has a commit its upstream does not, so no fast-forward exists.
    NotFastForward,
}

/// One [`attempt`] outcome. `run_fetch_cycle` deliberately discards it (`let _ =`), since
/// nothing there writes a report of its own; `Core::attempt_auto_update` is the one
/// production reader, matching every variant but `Updated`'s own `from`/`to` fields, which
/// stay read only by this module's own tests.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum Outcome {
    /// Nothing was touched, and why.
    Ineligible(Ineligible),
    /// The branch moved from `from` to `to`.
    Updated {
        from: gix::ObjectId,
        to: gix::ObjectId,
    },
    /// A git read or write failed partway through; never a stand-in for ineligibility.
    Failed(String),
}

/// Attempts the fast-forward-only update on the Repo at `path`: checks every eligibility
/// rule fresh against the repository as it stands right now (never a cached `Cell`, which
/// may predate the fetch that just ran), and moves the branch only when all of them hold.
/// An ineligible or failed Repo is left exactly as it was; the next Generation's own probe
/// is what reports it, since this module writes nothing to explain itself.
pub(crate) fn attempt(path: &Path) -> Outcome {
    let repo = match gix::open(path) {
        Ok(repo) => repo,
        Err(error) => return Outcome::Failed(describe_error_chain(error)),
    };

    let head = match git::head_shape(&repo) {
        Ok(head) => head,
        Err(error) => return Outcome::Failed(describe_error_chain(error)),
    };
    let Head::Branch {
        name,
        commit: local_commit,
    } = head
    else {
        return Outcome::Ineligible(Ineligible::NoUpstream);
    };

    if !git::has_any_remote(&repo) {
        return Outcome::Ineligible(Ineligible::NoUpstream);
    }
    let Some(upstream_commit) = git::upstream_commit(&repo, &name) else {
        return Outcome::Ineligible(Ineligible::NoUpstream);
    };

    let ahead_behind = match git::ahead_behind(&repo, local_commit, upstream_commit) {
        Ok(counts) => counts,
        Err(error) => return Outcome::Failed(error),
    };
    if ahead_behind.ahead > 0 {
        return Outcome::Ineligible(Ineligible::NotFastForward);
    }
    if ahead_behind.behind == 0 {
        return Outcome::Ineligible(Ineligible::NotBehind);
    }

    match is_repo_clean_for_auto_update(&repo, local_commit) {
        Ok(true) => {}
        Ok(false) => return Outcome::Ineligible(Ineligible::NotClean),
        Err(error) => return Outcome::Failed(error),
    }

    // Defence in depth, not a sixth eligibility rule: `ahead_behind.ahead == 0` above
    // already proves `local_commit` is an ancestor of `upstream_commit` by reachability
    // (see this module's own doc comment on [`Ineligible::NotFastForward`]). This check
    // can never fail on a path that reached here; it stands so the mutation below is
    // never reached by a future edit that changes what "ahead" means without changing
    // this guard to match.
    match is_ancestor(&repo, local_commit, upstream_commit) {
        Ok(true) => {}
        Ok(false) => return Outcome::Ineligible(Ineligible::NotFastForward),
        Err(error) => return Outcome::Failed(error),
    }

    match fast_forward(&repo, path, &name, local_commit, upstream_commit) {
        Ok(()) => Outcome::Updated {
            from: local_commit,
            to: upstream_commit,
        },
        Err(error) => Outcome::Failed(error),
    }
}

/// Whether `from` is `to` itself or one of its ancestors, per [`git::checked_merge_base`].
fn is_ancestor(
    repo: &gix::Repository,
    from: gix::ObjectId,
    to: gix::ObjectId,
) -> Result<bool, String> {
    Ok(git::checked_merge_base(repo, from, to)? == Some(from))
}

/// "Clean" for the auto-update's own purposes: the working tree matches the index
/// ([`git::dirty_counts`], the same typed counts the UI's own `dirty` cell reads) *and*
/// the index matches `head_commit`'s own tree. The second half is deliberately not part
/// of [`git::dirty_counts`] (its own doc comment: "never the head-to-index half"), which
/// is exactly why this module cannot reuse it alone: overwriting the index below would
/// silently discard a staged-but-uncommitted change that a worktree-vs-index count alone
/// cannot see.
fn is_repo_clean_for_auto_update(
    repo: &gix::Repository,
    head_commit: gix::ObjectId,
) -> Result<bool, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let counts = git::dirty_counts(repo, cancel).map_err(describe_error_chain)?;
    if counts.total() > 0 {
        return Ok(false);
    }

    let head_tree = repo
        .find_commit(head_commit)
        .map_err(describe_error_chain)?
        .tree_id()
        .map_err(describe_error_chain)?
        .detach();
    let expected_index = repo
        .index_from_tree(&head_tree)
        .map_err(describe_error_chain)?;
    let actual_index = repo.open_index().map_err(describe_error_chain)?;
    Ok(index_snapshot(&expected_index) == index_snapshot(&actual_index))
}

/// Every index entry's path, mode and blob id, keyed so two indexes built by different
/// paths (a fresh read off disk, a fresh build from a tree) compare equal exactly when
/// they carry the same entries, independent of on-disk ordering or extension data
/// neither construction path carries the same way.
fn index_snapshot(
    index: &gix::index::File,
) -> BTreeMap<Vec<u8>, (gix::index::entry::Mode, gix::ObjectId)> {
    let backing = index.path_backing();
    index
        .entries()
        .iter()
        .map(|entry| (entry.path_in(backing).to_vec(), (entry.mode, entry.id)))
        .collect()
}

/// Moves `branch_name`'s ref from `from` to `to`: applies the tree diff between the two
/// commits to the working tree, rewrites the index to `to`'s own tree exactly (safe only
/// because [`is_repo_clean_for_auto_update`] already proved the two agreed beforehand),
/// then moves the ref itself with a reflog entry. No step here is a commit, a merge, a
/// rebase or a reset ([`gix::Repository::commit_as`], `.merge(`, `.rebase(` and `.reset(`
/// never appear in this module): a ref edit and a set of plain file writes are the whole
/// mechanism, which is what keeps this the narrowest safe operation ADR 0002 asks for.
fn fast_forward(
    repo: &gix::Repository,
    work_dir: &Path,
    branch_name: &str,
    from: gix::ObjectId,
    to: gix::ObjectId,
) -> Result<(), String> {
    let from_tree = repo
        .find_commit(from)
        .map_err(describe_error_chain)?
        .tree()
        .map_err(describe_error_chain)?;
    let to_commit = repo.find_commit(to).map_err(describe_error_chain)?;
    let to_tree = to_commit.tree().map_err(describe_error_chain)?;
    let to_tree_id = to_commit.tree_id().map_err(describe_error_chain)?.detach();

    let mut changes = from_tree.changes().map_err(describe_error_chain)?;
    changes.options(|options| {
        options.track_path();
        options.track_rewrites(None);
    });
    changes
        .for_each_to_obtain_tree(&to_tree, |change| -> Result<Action, String> {
            apply_change(change, work_dir)?;
            Ok(Action::Continue(()))
        })
        .map_err(describe_error_chain)?;

    let mut new_index = repo
        .index_from_tree(&to_tree_id)
        .map_err(describe_error_chain)?;
    new_index
        .write(Default::default())
        .map_err(describe_error_chain)?;

    let full_ref = format!("refs/heads/{branch_name}");
    let mut reference = repo
        .find_reference(full_ref.as_str())
        .map_err(describe_error_chain)?;
    reference
        .set_target_id(to, "repon: fast-forward auto-update")
        .map_err(describe_error_chain)?;
    Ok(())
}

/// Applies one tree-diff change to the working tree at `work_dir`. A rewrite (a rename or
/// copy the diff detected) is disabled at the call site (`track_rewrites(None)`), but the
/// match still names every [`Change`] variant, per this crate's own no-catch-all
/// convention: a variant this module cannot yet reach still has to be named here rather
/// than silently matched by a wildcard if diff options ever change under it.
fn apply_change(change: Change<'_, '_, '_>, work_dir: &Path) -> Result<(), String> {
    match change {
        Change::Addition {
            location,
            entry_mode,
            id,
            ..
        }
        | Change::Modification {
            location,
            entry_mode,
            id,
            ..
        } => write_entry(work_dir, location, entry_mode, id),
        Change::Deletion { location, .. } => remove_entry(work_dir, location),
        Change::Rewrite {
            source_location,
            location,
            entry_mode,
            id,
            ..
        } => {
            if source_location != location {
                remove_entry(work_dir, source_location)?;
            }
            write_entry(work_dir, location, entry_mode, id)
        }
    }
}

/// Describes `error` and its full `source()` chain, outermost first, on one line: every
/// `map_err` in this module reports through this rather than `to_string()` alone, which
/// keeps only a wrapper's own message and drops the cause underneath it.
fn describe_error_chain<E: std::error::Error>(error: E) -> String {
    let mut message = format!("{error}").replace('\n', " ");
    let mut source = error.source();
    while let Some(current) = source {
        message.push_str(": ");
        message.push_str(&format!("{current}").replace('\n', " "));
        source = current.source();
    }
    message
}

fn relative_path(location: &BStr) -> Result<&Path, String> {
    location.to_str().map(Path::new).map_err(|error| {
        format!(
            "non-UTF-8 path in a tree diff: {}",
            describe_error_chain(error)
        )
    })
}

/// Writes or overwrites `location` under `work_dir` with `id`'s own object content,
/// per `entry_mode`'s kind: a plain file, an executable file, or a symlink whose target
/// is the blob's content.  A tree or a commit (submodule) entry is refused outright
/// rather than guessed at: this module has no working-tree-mutation story for either.
fn write_entry(
    work_dir: &Path,
    location: &BStr,
    entry_mode: gix::objs::tree::EntryMode,
    id: gix::Id<'_>,
) -> Result<(), String> {
    let full_path = work_dir.join(relative_path(location)?);
    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent).map_err(describe_error_chain)?;
    }
    let object = id.object().map_err(describe_error_chain)?;
    match entry_mode.kind() {
        EntryKind::Blob => {
            fs::write(&full_path, &object.data).map_err(describe_error_chain)?;
            set_permissions(&full_path, 0o644)?;
        }
        EntryKind::BlobExecutable => {
            fs::write(&full_path, &object.data).map_err(describe_error_chain)?;
            set_permissions(&full_path, 0o755)?;
        }
        EntryKind::Link => {
            let target = object
                .data
                .to_path()
                .map_err(describe_error_chain)?
                .to_path_buf();
            match fs::remove_file(&full_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(describe_error_chain(error)),
            }
            std::os::unix::fs::symlink(target, &full_path).map_err(describe_error_chain)?;
        }
        EntryKind::Tree => {
            return Err(format!(
                "unexpected tree-shaped leaf change at {}",
                full_path.display()
            ));
        }
        EntryKind::Commit => {
            return Err(format!(
                "a submodule pointer changed at {}; the auto-update does not update submodules",
                full_path.display()
            ));
        }
    }
    Ok(())
}

fn set_permissions(path: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(describe_error_chain)
}

/// Removes `location` under `work_dir`, then prunes now-empty parent directories up to
/// (excluding) `work_dir` itself, best-effort: a directory `fs::remove_dir` refuses
/// because something else still lives there is left alone rather than treated as a
/// failure.
fn remove_entry(work_dir: &Path, location: &BStr) -> Result<(), String> {
    let full_path = work_dir.join(relative_path(location)?);
    match fs::remove_file(&full_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(describe_error_chain(error)),
    }
    let mut dir = full_path.parent();
    while let Some(candidate) = dir {
        if candidate == work_dir || fs::remove_dir(candidate).is_err() {
            break;
        }
        dir = candidate.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        commit_file, current_branch, git, head_sha, push_new_commit, remote_and_clone,
    };

    /// The text between the `(` that opens at `open` (the index just past it) and its
    /// matching `)`, tracking nesting depth so a call full of its own nested parens
    /// (`format!("...", inner(..))`) does not fool the scan into stopping early.
    fn balanced_parens(source: &str, open: usize) -> &str {
        let mut depth = 1usize;
        for (offset, character) in source[open..].char_indices() {
            match character {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[open..open + offset];
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced parentheses in module source starting at index {open}");
    }

    /// Every `.map_err(` call in this module, where a `dyn std::error::Error` is turned
    /// into the `String` this module returns (directly, as `.map_err(describe_error_chain)`,
    /// or through a closure that adds its own formatting around the call), renders
    /// through [`describe_error_chain`]: a call site added by hand could otherwise
    /// collapse its own chain the way `fast_forward` did before this ticket, and compile
    /// just as cleanly. Checks for the *presence* of the helper's call in each site's own
    /// argument, not the *absence* of one banned spelling of the bug (`.to_string()`), so
    /// a differently-spelled bypass (`format!("{error}")`, say) cannot silently slip past
    /// this test the way it slipped past an earlier version that only banned that one
    /// spelling. [`write_entry`]'s two `fs::remove_file` match arms take the same shape
    /// by hand rather than through `map_err` (matching on the `io::Error` first, to skip
    /// a not-found error); `two_bare_err_arms_in_write_entry_and_remove_entry_also_render_through_describe_error_chain`
    /// covers those two separately, by exact count rather than this scan's generic parsing.
    #[test]
    fn every_map_err_closure_in_this_module_renders_through_describe_error_chain() {
        let source = include_str!("auto_update.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("splitting on a literal always yields at least the part before it");

        let mut checked = 0;
        let mut search_from = 0;
        while let Some(relative) = production[search_from..].find(".map_err(") {
            let open = search_from + relative + ".map_err(".len();
            let argument = balanced_parens(production, open);
            assert!(
                argument.contains("describe_error_chain"),
                "a .map_err( call does not render through describe_error_chain: {argument:?}"
            );
            checked += 1;
            search_from = open;
        }

        assert!(
            checked >= 20,
            "expected at least 20 .map_err( sites in this module, found {checked}; \
             the scan's own marker may no longer match this file's shape"
        );
    }

    /// The two places this module turns an `io::Error` into the `String` it returns by
    /// hand, outside a `.map_err(` closure ([`write_entry`]'s symlink overwrite and
    /// [`remove_entry`], each matching the error first to let a not-found error through
    /// as a no-op), by an exact count of the one literal arm shape both use: a rewrite to
    /// any other spelling, `.to_string()` or `format!("{error}")` alike, changes the
    /// count and fails this test.
    #[test]
    fn two_bare_err_arms_in_write_entry_and_remove_entry_also_render_through_describe_error_chain()
    {
        let source = include_str!("auto_update.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("splitting on a literal always yields at least the part before it");

        let occurrences = production
            .matches("Err(error) => return Err(describe_error_chain(error)),")
            .count();

        assert_eq!(
            occurrences, 2,
            "expected exactly 2 bare `Err(error) => return Err(describe_error_chain(error)),` \
             arms (write_entry's symlink overwrite and remove_entry), found {occurrences}"
        );
    }

    fn read_file(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path:?}: {error}"))
    }

    /// A purpose-built error with no `source()`, standing in for a leaf failure
    /// ([`describe_error_chain`]'s tests never reuse a real gix or io error, so the chain
    /// length is exactly what the test author put there, not whatever a library happens
    /// to produce today).
    #[derive(Debug)]
    struct LeafError(&'static str);

    impl std::fmt::Display for LeafError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for LeafError {}

    /// An error whose `source()` is another error, for building a chain deeper than one
    /// hop in [`describe_error_chain`]'s own tests.
    #[derive(Debug)]
    struct WrapperError {
        message: &'static str,
        source: Box<dyn std::error::Error + 'static>,
    }

    impl std::fmt::Display for WrapperError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.message)
        }
    }

    impl std::error::Error for WrapperError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.source.as_ref())
        }
    }

    /// A single-layer error with no source renders exactly as it does today: its own
    /// message, no trailing separator, no empty segment appended for the absent source.
    #[test]
    fn describe_error_chain_of_a_sourceless_error_is_exactly_its_own_message() {
        let error = LeafError("disk is full");

        let rendered = describe_error_chain(error);

        assert_eq!(rendered, "disk is full");
    }

    /// A cause nested two levels deep (a wrapper whose source is itself sourced) is still
    /// visible in the rendered text, outermost first, on one line.
    #[test]
    fn describe_error_chain_of_a_two_deep_source_includes_every_level_outermost_first() {
        let root_cause = LeafError("permission denied");
        let middle = WrapperError {
            message: "the user-provided callback failed",
            source: Box::new(root_cause),
        };
        let outer = WrapperError {
            message: "fast-forward failed",
            source: Box::new(middle),
        };

        let rendered = describe_error_chain(&outer);

        assert_eq!(
            rendered,
            "fast-forward failed: the user-provided callback failed: permission denied"
        );
        assert!(
            !rendered.contains('\n'),
            "the message must stay on one line"
        );
    }

    /// A `source()` whose own `Display` text spans multiple lines (some real errors read
    /// that way) still renders onto one line, its embedded newline flattened, not left to
    /// break the one-line guarantee every other test here only ever checks against
    /// newline-free literals.
    #[test]
    fn describe_error_chain_flattens_a_multi_line_source_onto_one_line() {
        let error = WrapperError {
            message: "fast-forward failed",
            source: Box::new(LeafError("disk is full\nretry later")),
        };

        let rendered = describe_error_chain(error);

        assert_eq!(rendered, "fast-forward failed: disk is full retry later");
        assert!(
            !rendered.contains('\n'),
            "the message must stay on one line"
        );
    }

    /// The happy path the risk brief warns a weak test looks like on its own: an eligible
    /// Repo (clean, behind, not ahead, tracking an upstream) moves forward. Paired below
    /// with one ineligible fixture per condition, each proving the branch did *not* move.
    #[test]
    fn an_eligible_repo_fast_forwards_to_its_upstream_and_updates_the_working_tree() {
        let (remote, clone) = remote_and_clone();
        push_new_commit(remote.path(), "second.txt", "second\n");
        git(clone.path(), &["fetch", "origin"]);
        let upstream_sha = git_rev_parse(clone.path(), "refs/remotes/origin/main");

        let outcome = attempt(clone.path());

        match outcome {
            Outcome::Updated { to, .. } => assert_eq!(to.to_string(), upstream_sha),
            other => panic!("expected an eligible repo to update, got {other:?}"),
        }
        assert_eq!(
            git_rev_parse(clone.path(), "refs/heads/main"),
            upstream_sha,
            "the local branch ref must now equal the upstream it fast-forwarded to"
        );
        assert_eq!(
            read_file(&clone.path().join("second.txt")),
            "second\n",
            "the new commit's file must be checked out into the working tree"
        );
    }

    /// A deleted file is removed from the working tree by a fast-forward, not merely
    /// left behind because only additions were applied.
    #[test]
    fn a_fast_forward_removes_a_file_the_new_commit_deleted() {
        let (remote, clone) = remote_and_clone();
        push_new_commit(remote.path(), "doomed.txt", "will be removed\n");
        git(clone.path(), &["fetch", "origin"]);
        // The clone never checked this commit out; fast-forward it there first so the
        // file that gets removed next actually exists in the working tree.
        git(clone.path(), &["merge", "--ff-only", "origin/main"]);
        assert!(clone.path().join("doomed.txt").exists());

        // `remote` is bare, so the deletion goes through the same clone-commit-push
        // dance every other collaborator's own change in this module does.
        push_removed_file(remote.path(), "doomed.txt");
        git(clone.path(), &["fetch", "origin"]);

        let outcome = attempt(clone.path());

        assert!(
            matches!(outcome, Outcome::Updated { .. }),
            "expected the deletion to still be a clean fast-forward, got {outcome:?}"
        );
        assert!(
            !clone.path().join("doomed.txt").exists(),
            "the fast-forward must remove a file the new commit no longer has"
        );
    }

    /// A callback failure whose cause is [`write_entry`]'s own `fs::write` (the new
    /// commit's file lands where the working tree already has a same-named directory)
    /// must surface that io error's own message, not just gix's wrapper text, `The
    /// user-provided callback failed`, the exact string this ticket was filed against.
    /// `blocked` sits at the tree root, a plain [`Change::Addition`] of `EntryKind::Blob`
    /// reaching `write_entry` directly: a nested path instead would hit gix's own
    /// directory-level change notice first (`EntryKind::Tree`, handled by the untouched,
    /// pre-existing arm below), never reaching a fs call this ticket touched at all.
    #[test]
    fn a_fast_forward_failure_from_a_callback_error_reports_the_callback_message() {
        let (remote, clone) = remote_and_clone();
        let from_sha = git_rev_parse(clone.path(), "refs/heads/main");
        push_new_commit(remote.path(), "blocked", "new content\n");
        git(clone.path(), &["fetch", "origin"]);
        let to_sha = git_rev_parse(clone.path(), "refs/remotes/origin/main");
        // `blocked` already exists in the working tree as a directory, so writing the new
        // commit's `blocked` file there fails, an OS-level type mismatch that holds
        // regardless of which user or permissions the test runs under.
        std::fs::create_dir(clone.path().join("blocked")).expect("create a blocking directory");
        let repo = gix::open(clone.path()).expect("open the clone");
        let from = gix::ObjectId::from_hex(from_sha.as_bytes()).expect("parse from sha");
        let to = gix::ObjectId::from_hex(to_sha.as_bytes()).expect("parse to sha");

        let error = fast_forward(&repo, clone.path(), "main", from, to)
            .expect_err("a directory blocking the new file must fail the fast-forward");

        assert!(
            error.starts_with("The user-provided callback failed: "),
            "expected the wrapper's own message followed by its cause, got {error:?}"
        );
        assert_ne!(
            error, "The user-provided callback failed",
            "the callback's own message must not be dropped"
        );
    }

    fn push_removed_file(remote: &Path, name: &str) {
        let contributor = tempfile::tempdir().expect("temp dir");
        let status = std::process::Command::new("git")
            .arg("clone")
            .arg(remote)
            .arg(contributor.path())
            .status()
            .expect("run git clone");
        assert!(status.success());
        git(contributor.path(), &["rm", name]);
        git(contributor.path(), &["commit", "-m", "remove a file"]);
        git(contributor.path(), &["push", "origin", "main"]);
    }

    fn git_rev_parse(path: &Path, rev: &str) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", rev])
            .output()
            .expect("run git rev-parse");
        assert!(output.status.success(), "git rev-parse {rev} failed");
        String::from_utf8(output.stdout)
            .expect("utf8 output")
            .trim()
            .to_string()
    }

    /// Condition 1: a dirty working tree is ineligible even though it is otherwise
    /// exactly the eligible-fixture shape (clean-but-for-this, behind, not ahead,
    /// tracking an upstream): asserts the branch did not move, and that the true dirty
    /// count is still reported (not hidden) on the next read.
    #[test]
    fn a_dirty_repo_is_ineligible_and_left_untouched() {
        let (remote, clone) = remote_and_clone();
        push_new_commit(remote.path(), "second.txt", "second\n");
        git(clone.path(), &["fetch", "origin"]);
        let before = git_rev_parse(clone.path(), "refs/heads/main");
        std::fs::write(clone.path().join("untracked.txt"), "uncommitted\n")
            .expect("write an untracked file");

        let outcome = attempt(clone.path());

        assert!(
            matches!(outcome, Outcome::Ineligible(Ineligible::NotClean)),
            "expected NotClean, got {outcome:?}"
        );
        assert_eq!(
            git_rev_parse(clone.path(), "refs/heads/main"),
            before,
            "a dirty repo's branch must not move"
        );
        let repo = gix::open(clone.path()).expect("open the clone");
        let counts =
            git::dirty_counts(&repo, Arc::new(AtomicBool::new(false))).expect("dirty counts");
        assert_eq!(
            counts.untracked, 1,
            "the dirty count must still truthfully report the untracked file, not hide it"
        );
    }

    /// Condition 2: nothing to do (already level with the upstream) is ineligible, not
    /// merely a silent no-op indistinguishable from a bug.
    #[test]
    fn an_up_to_date_repo_is_ineligible_and_left_untouched() {
        let (_remote, clone) = remote_and_clone();
        git(clone.path(), &["fetch", "origin"]);
        let before = git_rev_parse(clone.path(), "refs/heads/main");

        let outcome = attempt(clone.path());

        assert!(
            matches!(outcome, Outcome::Ineligible(Ineligible::NotBehind)),
            "expected NotBehind, got {outcome:?}"
        );
        assert_eq!(git_rev_parse(clone.path(), "refs/heads/main"), before);
        let repo = gix::open(clone.path()).expect("open the clone");
        let head = git::head_shape(&repo).expect("head shape");
        let Head::Branch { commit, .. } = head else {
            panic!("expected a branch head")
        };
        let sync = git::resolve_sync(&repo, Some(&head)).expect("resolve sync");
        assert!(
            matches!(
                sync,
                crate::entity::SyncState::Tracking(crate::entity::AheadBehind {
                    ahead: 0,
                    behind: 0
                })
            ),
            "the true, level sync state must still be what a fresh read reports, got \
             {sync:?} for commit {commit}"
        );
    }

    /// Condition 3 (and, by the mathematical equivalence this module's own doc comment
    /// records, condition 5's "fast-forward only"): a local commit the upstream does not
    /// have makes the repo ineligible, even though it is also behind and tracks an
    /// upstream.
    #[test]
    fn a_repo_with_an_unpublished_local_commit_is_ineligible_and_left_untouched() {
        let (remote, clone) = remote_and_clone();
        push_new_commit(remote.path(), "second.txt", "second\n");
        git(clone.path(), &["fetch", "origin"]);
        commit_file(clone.path(), "local-only.txt", "never pushed\n");
        let before = git_rev_parse(clone.path(), "refs/heads/main");

        let outcome = attempt(clone.path());

        assert!(
            matches!(outcome, Outcome::Ineligible(Ineligible::NotFastForward)),
            "expected NotFastForward, got {outcome:?}"
        );
        assert_eq!(
            git_rev_parse(clone.path(), "refs/heads/main"),
            before,
            "a repo with an unpublished commit must not move"
        );
        let repo = gix::open(clone.path()).expect("open the clone");
        let head = git::head_shape(&repo).expect("head shape");
        let sync = git::resolve_sync(&repo, Some(&head)).expect("resolve sync");
        assert!(
            matches!(
                sync,
                crate::entity::SyncState::Tracking(crate::entity::AheadBehind { ahead: 1, .. })
            ),
            "the true ahead count must still be reported, got {sync:?}"
        );
    }

    /// Condition 4: a branch with no upstream configured at all is ineligible.
    #[test]
    fn a_branch_with_no_upstream_is_ineligible_and_left_untouched() {
        let (_remote, clone) = remote_and_clone();
        git(clone.path(), &["checkout", "-b", "untracked-branch"]);
        let before = git_rev_parse(clone.path(), "refs/heads/untracked-branch");

        let outcome = attempt(clone.path());

        assert!(
            matches!(outcome, Outcome::Ineligible(Ineligible::NoUpstream)),
            "expected NoUpstream, got {outcome:?}"
        );
        assert_eq!(
            git_rev_parse(clone.path(), "refs/heads/untracked-branch"),
            before
        );
        let repo = gix::open(clone.path()).expect("open the clone");
        let head = git::head_shape(&repo).expect("head shape");
        let sync = git::resolve_sync(&repo, Some(&head)).expect("resolve sync");
        assert_eq!(
            sync,
            crate::entity::SyncState::NoUpstream,
            "the true absence of an upstream must still be what a fresh read reports"
        );
        assert_eq!(current_branch(clone.path()), "untracked-branch");
    }

    /// Condition 5's own dedicated proof, at the level it can actually be independent of
    /// condition 3 (see this module's own doc comment on [`Ineligible::NotFastForward`]):
    /// [`is_ancestor`] itself correctly refuses two commits from unrelated histories, the
    /// defensive check [`attempt`] runs right before it would otherwise mutate anything.
    #[test]
    fn is_ancestor_refuses_two_commits_with_unrelated_histories() {
        let dir = tempfile::tempdir().expect("temp dir");
        git(dir.path(), &["init", "-q", "--initial-branch=main"]);
        commit_file(dir.path(), "a.txt", "a\n");
        let first = head_sha(dir.path());
        git(dir.path(), &["checkout", "--orphan", "unrelated"]);
        git(dir.path(), &["rm", "-rf", "."]);
        commit_file(dir.path(), "b.txt", "b\n");
        let second = head_sha(dir.path());

        let repo = gix::open(dir.path()).expect("open");
        let from = gix::ObjectId::from_hex(first.as_bytes()).expect("parse sha");
        let to = gix::ObjectId::from_hex(second.as_bytes()).expect("parse sha");

        assert_eq!(is_ancestor(&repo, from, to), Ok(false));
    }
}
