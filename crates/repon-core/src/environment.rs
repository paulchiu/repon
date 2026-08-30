//! The environment contract: an Entity's already-computed git facts, turned into
//! the set-or-unset variable pairs a child (a Launcher or an Action step)
//! receives. Returns data only: no argv, no shell mode, no terminal content, and
//! nothing here spawns anything.
//!
//! See `docs/spec/core-api.md`'s "The environment contract", `docs/spec/config.md`'s
//! table of the same name, and [ADR 0018](https://github.com/paulchiu/repon/blob/main/docs/adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md)
//! and [ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)
//! for why an unset name must never carry an empty string and why a detached
//! HEAD must never leak an object id into the branch slot.

use crate::cell::Settled;
use crate::entity::{DefaultBranch, EntityState, Head, Kind};

const REPON_REPO_PATH: &str = "REPON_REPO_PATH";
const REPON_REPO_NAME: &str = "REPON_REPO_NAME";
const REPON_COMMON_DIR: &str = "REPON_COMMON_DIR";
const REPON_KIND: &str = "REPON_KIND";
const REPON_BRANCH: &str = "REPON_BRANCH";
const REPON_HEAD: &str = "REPON_HEAD";
const REPON_DEFAULT_BRANCH: &str = "REPON_DEFAULT_BRANCH";
const REPON_ACTION: &str = "REPON_ACTION";

/// The eight `REPON_` variable names, in the order `docs/spec/config.md`'s table
/// lists them. Read by [`environment`] itself for nothing but this array's own
/// length; the value each name carries still takes its own match against the
/// Entity, since a name alone cannot say how to derive one. Also read by this
/// module's own tests, so a name dropped from [`environment`]'s construction and
/// a name dropped from this array are the same edit rather than two that could
/// drift apart.
const REPON_ENV_VAR_NAMES: [&str; 8] = [
    REPON_REPO_PATH,
    REPON_REPO_NAME,
    REPON_COMMON_DIR,
    REPON_KIND,
    REPON_BRANCH,
    REPON_HEAD,
    REPON_DEFAULT_BRANCH,
    REPON_ACTION,
];

/// The terminal-prompt suppression variable, force-set for every child
/// regardless of shape: a step that would otherwise block on a credential
/// prompt behind the alternate screen is a hang with no visible cause
/// (ADR 0018).
const GIT_TERMINAL_PROMPT: &str = "GIT_TERMINAL_PROMPT";

/// The fifteen git local environment variables Repon unsets from every child,
/// exactly `git rev-parse --local-env-vars` on git 2.50.1 (`docs/spec/config.md`).
/// One array read by both [`environment`] and this module's own tests, so a
/// variable dropped from one cannot silently drop from the other, and the count
/// of fifteen is asserted against this array's own length rather than written
/// twice.
const GIT_LOCAL_ENV_VARS: [&str; 15] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// `entity`'s environment contract: the set-or-unset pairs a Launcher or an
/// Action step's child receives. `Some` sets a variable, `None` unsets it, so an
/// absent name can never be misread as one set to the empty string.
///
/// Covers exactly the eight `REPON_` variables and all fifteen of git's local
/// environment variables (`docs/spec/config.md`), plus `GIT_TERMINAL_PROMPT`,
/// force-set on every call regardless of shape or `action`. `action` names the
/// running Action; `None` is `REPON_ACTION`'s own unset case, for a Launcher.
///
/// Destructures `entity` exhaustively so a Cell added to [`EntityState`] later
/// fails to compile here rather than silently never reaching the environment.
pub fn environment(entity: &EntityState, action: Option<&str>) -> Vec<(String, Option<String>)> {
    let EntityState {
        key,
        name,
        common_dir,
        kind,
        branch,
        sync: _,
        base: _,
        dirty: _,
        state: _,
        default_branch,
        diagnostics: _,
        last_action: _,
        presence: _,
        excluded: _,
        in_progress_operation: _,
        recent_commits: _,
    } = entity;

    let (repon_branch, repon_head) = branch_and_head(branch.settled());

    let repon_pairs: [(&str, Option<String>); 8] = [
        (
            REPON_REPO_PATH,
            Some(key.path().to_string_lossy().into_owned()),
        ),
        (REPON_REPO_NAME, Some(name.to_string())),
        (
            REPON_COMMON_DIR,
            Some(common_dir.to_string_lossy().into_owned()),
        ),
        (REPON_KIND, Some(kind_name(*kind).to_string())),
        (REPON_BRANCH, repon_branch),
        (REPON_HEAD, repon_head),
        (
            REPON_DEFAULT_BRANCH,
            default_branch_name(default_branch.settled()),
        ),
        (REPON_ACTION, action.map(str::to_string)),
    ];
    // Ties this construction to `REPON_ENV_VAR_NAMES`, the same array the count
    // and presence tests read, so the two cannot drift apart unnoticed.
    debug_assert_eq!(
        repon_pairs.each_ref().map(|(name, _)| *name),
        REPON_ENV_VAR_NAMES,
        "the environment contract's Repon variable names drifted from REPON_ENV_VAR_NAMES"
    );

    let mut pairs: Vec<(String, Option<String>)> = repon_pairs
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    pairs.push((GIT_TERMINAL_PROMPT.to_string(), Some("0".to_string())));
    pairs.extend(
        GIT_LOCAL_ENV_VARS
            .iter()
            .map(|name| (name.to_string(), None)),
    );
    pairs
}

/// `kind`'s lower-case name, `docs/spec/config.md`'s `REPON_KIND` values. No
/// wildcard arm, so a fourth `Kind` fails to compile here rather than silently
/// falling through unnamed.
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Repo => "repo",
        Kind::Worktree => "worktree",
        Kind::Submodule => "submodule",
    }
}

/// `branch`'s contribution to `REPON_BRANCH` and `REPON_HEAD`. `Head::Branch`
/// sets both, its name and its own resolved commit; `Head::Detached` sets only
/// `REPON_HEAD`, since a detached row's branch slot must never carry an object
/// id ([ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md));
/// `Head::Unborn` sets only `REPON_BRANCH`, since there is no commit yet.
/// Anything short of `Known` (`Unknown`, `Failed`, `NotApplicable`, or never yet
/// probed) unsets both, per `docs/spec/config.md`'s rule that an unresolved
/// value is unset rather than empty.
fn branch_and_head(settled: Option<&Settled<Head>>) -> (Option<String>, Option<String>) {
    let Some(settled) = settled else {
        return (None, None);
    };
    match settled {
        Settled::Known { value, .. } => match value {
            Head::Branch { name, commit } => (Some(name.to_string()), Some(commit.to_string())),
            Head::Detached(commit) => (None, Some(commit.to_string())),
            Head::Unborn(name) => (Some(name.to_string()), None),
        },
        Settled::Unknown(_) | Settled::Failed(_) | Settled::NotApplicable => (None, None),
    }
}

/// `default_branch`'s contribution to `REPON_DEFAULT_BRANCH`: the resolved name
/// when `Known`, unset for every other shape, `NotApplicable` included, so
/// `${REPON_DEFAULT_BRANCH:-main}` never substitutes a default branch
/// `docs/spec/discovery.md` already records as known-wrong for a Submodule.
fn default_branch_name(settled: Option<&Settled<DefaultBranch>>) -> Option<String> {
    match settled? {
        Settled::Known { value, .. } => Some(value.name().to_string()),
        Settled::Unknown(_) | Settled::Failed(_) | Settled::NotApplicable => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::cell::{Generation, Timestamp, Unknown};
    use crate::entity::EntityKey;

    /// A hex string long enough to be a real object id, distinguishable per call
    /// so a test can tell two different commits apart.
    fn commit(hex_tail: &str) -> gix::ObjectId {
        let hex = format!("{:0>40}", hex_tail);
        gix::ObjectId::from_hex(hex.as_bytes()).expect("valid hex object id")
    }

    fn entity(
        kind: Kind,
        path: &str,
        name: &str,
        common_dir: &str,
        head: Head,
        default_branch: Settled<DefaultBranch>,
    ) -> EntityState {
        let mut entity = EntityState::new(
            EntityKey::new(Arc::from(Path::new(path))),
            Arc::from(name),
            Arc::from(Path::new(common_dir)),
            kind,
        );
        let generation = Generation::new(1);
        entity.branch.settle(
            generation,
            Settled::Known {
                value: head,
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.default_branch.settle(generation, default_branch);
        entity
    }

    fn known_default_branch(name: &str) -> Settled<DefaultBranch> {
        Settled::Known {
            value: DefaultBranch::new(Arc::from(name)),
            at: Timestamp::now(),
            stale: false,
        }
    }

    fn find<'a>(pairs: &'a [(String, Option<String>)], name: &str) -> Option<&'a Option<String>> {
        pairs
            .iter()
            .find(|(pair_name, _)| pair_name == name)
            .map(|(_, value)| value)
    }

    fn git_unset_pairs() -> Vec<(String, Option<String>)> {
        GIT_LOCAL_ENV_VARS
            .iter()
            .map(|name| (name.to_string(), None))
            .collect()
    }

    /// `docs/spec/config.md`, read at test time from `CARGO_MANIFEST_DIR` rather
    /// than with `include_str!`, matching the pattern `lib.rs`'s own
    /// `public_surface_matches_glossary` test already uses for a file outside
    /// this crate's own directory.
    fn spec_config_md() -> String {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec/config.md"))
            .expect("read docs/spec/config.md")
    }

    /// Every backtick-quoted token on `line`, in order.
    fn backtick_tokens(line: &str) -> Vec<&str> {
        line.split('`').skip(1).step_by(2).collect()
    }

    /// The `REPON_` names `docs/spec/config.md`'s "The environment contract"
    /// table lists, one per table row, read from the row's own first
    /// backtick-quoted cell so a value column's own backtick-quoted text (a
    /// `REPON_KIND` value, say) is never mistaken for a variable name.
    fn spec_repon_variable_names(spec: &str) -> Vec<String> {
        let section = spec
            .split("## The environment contract")
            .nth(1)
            .expect("\"The environment contract\" section is present")
            .split("\n## ")
            .next()
            .expect("a following heading or end of file");
        section
            .lines()
            .filter(|line| line.trim_start().starts_with('|'))
            .filter_map(|line| backtick_tokens(line).first().copied())
            .filter(|token| token.starts_with("REPON_"))
            .map(str::to_string)
            .collect()
    }

    /// The git local environment variable names `docs/spec/config.md` lists in
    /// its own sentence naming them, read from that sentence rather than
    /// transcribed a second time.
    fn spec_git_local_env_var_names(spec: &str) -> Vec<String> {
        let anchor =
            "Repon unsets all fifteen of git's local environment variables from every child:";
        let after = spec
            .split(anchor)
            .nth(1)
            .expect("the git local env vars sentence is present");
        let list = after.split('.').next().expect("a sentence terminator");
        backtick_tokens(list)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// Asserts `spec_names` and `array_names` name exactly the same set,
    /// reporting the specific name either side lacks rather than only a count,
    /// so a misspelling or a dropped entry fails with the offending name.
    fn assert_names_match_the_spec(spec_names: &[String], array_names: &[String]) {
        let missing_from_array: Vec<&String> = spec_names
            .iter()
            .filter(|name| !array_names.contains(name))
            .collect();
        let missing_from_spec: Vec<&String> = array_names
            .iter()
            .filter(|name| !spec_names.contains(name))
            .collect();
        assert!(
            missing_from_array.is_empty(),
            "named in docs/spec/config.md but missing from the array: {missing_from_array:?}"
        );
        assert!(
            missing_from_spec.is_empty(),
            "in the array but not named in docs/spec/config.md: {missing_from_spec:?}"
        );
    }

    // Criterion 2: the counts are the claim, asserted against the one list both
    // the production path and these tests read.

    #[test]
    fn exactly_eight_repon_variable_names_are_declared() {
        assert_eq!(REPON_ENV_VAR_NAMES.len(), 8);
    }

    #[test]
    fn exactly_fifteen_git_local_env_vars_are_declared() {
        assert_eq!(GIT_LOCAL_ENV_VARS.len(), 15);
    }

    // The array-to-array tests above catch REPON_ENV_VAR_NAMES and
    // GIT_LOCAL_ENV_VARS drifting from each other; they do nothing about both
    // drifting together away from the design of record. These two read
    // `docs/spec/config.md` itself as the independent source of truth.

    #[test]
    fn repon_env_var_names_match_the_spec_exactly() {
        let spec = spec_config_md();
        assert_names_match_the_spec(
            &spec_repon_variable_names(&spec),
            &REPON_ENV_VAR_NAMES
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn git_local_env_var_names_match_the_spec_exactly() {
        let spec = spec_config_md();
        assert_names_match_the_spec(
            &spec_git_local_env_var_names(&spec),
            &GIT_LOCAL_ENV_VARS
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn every_declared_repon_variable_is_present_in_the_output() {
        let row = entity(
            Kind::Worktree,
            "/dev/repo",
            "repo",
            "/dev/repo/.git",
            Head::Branch {
                name: Arc::from("main"),
                commit: commit("1"),
            },
            known_default_branch("origin/main"),
        );

        let pairs = environment(&row, None);

        for expected in REPON_ENV_VAR_NAMES {
            assert!(
                find(&pairs, expected).is_some(),
                "missing Repon variable: {expected}"
            );
        }
    }

    #[test]
    fn every_git_local_env_var_is_present_and_unset() {
        let row = entity(
            Kind::Worktree,
            "/dev/repo",
            "repo",
            "/dev/repo/.git",
            Head::Branch {
                name: Arc::from("main"),
                commit: commit("1"),
            },
            known_default_branch("origin/main"),
        );

        let pairs = environment(&row, None);

        for expected in GIT_LOCAL_ENV_VARS {
            match find(&pairs, expected) {
                Some(value) => assert_eq!(*value, None, "git variable {expected} must be unset"),
                None => panic!("missing git variable: {expected}"),
            }
        }
    }

    // Criterion 7: no Repon selection state is exported; the produced REPON_
    // names are exactly the declared eight, so an addition (a Selection, a
    // cursor, REPON_SET) fails this scan even though it would pass a
    // presence-only check.

    #[test]
    fn no_repon_variable_beyond_the_declared_eight_is_ever_produced() {
        let row = entity(
            Kind::Worktree,
            "/dev/repo",
            "repo",
            "/dev/repo/.git",
            Head::Branch {
                name: Arc::from("main"),
                commit: commit("1"),
            },
            known_default_branch("origin/main"),
        );

        let pairs = environment(&row, Some("reinstall"));

        let mut produced: Vec<&str> = pairs
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| name.starts_with("REPON_"))
            .collect();
        produced.sort_unstable();
        let mut expected = REPON_ENV_VAR_NAMES.to_vec();
        expected.sort_unstable();

        assert_eq!(
            produced, expected,
            "Repon must export exactly its own eight variables, no Selection or Set state"
        );
    }

    // Criterion 3: the head variable across HEAD shapes.

    #[test]
    fn repon_head_carries_the_resolved_commit_on_an_attached_branch() {
        let head_commit = commit("aaa");
        let row = entity(
            Kind::Worktree,
            "/dev/repo",
            "repo",
            "/dev/repo/.git",
            Head::Branch {
                name: Arc::from("main"),
                commit: head_commit,
            },
            known_default_branch("origin/main"),
        );

        let pairs = environment(&row, None);

        assert_eq!(
            find(&pairs, REPON_HEAD),
            Some(&Some(head_commit.to_string())),
            "an attached branch must carry its own resolved commit"
        );
    }

    #[test]
    fn repon_head_is_unset_on_an_unborn_head() {
        let row = entity(
            Kind::Worktree,
            "/dev/repo",
            "repo",
            "/dev/repo/.git",
            Head::Unborn(Arc::from("main")),
            known_default_branch("origin/main"),
        );

        let pairs = environment(&row, None);

        assert_eq!(
            find(&pairs, REPON_HEAD),
            Some(&None),
            "an unborn HEAD has no commit, so REPON_HEAD must be unset"
        );
    }

    // Criterion 4: the branch variable never carries an object id.

    #[test]
    fn a_detached_rows_branch_variable_never_carries_the_resolved_commit() {
        let head_commit = commit("bbb");
        let row = entity(
            Kind::Worktree,
            "/dev/repo-pr-1",
            "repo-pr-1",
            "/dev/repo/.git",
            Head::Detached(head_commit),
            known_default_branch("origin/main"),
        );

        let pairs = environment(&row, None);

        assert_eq!(
            find(&pairs, REPON_BRANCH),
            Some(&None),
            "a detached row's branch variable must be unset, never the resolved commit"
        );
        assert_eq!(
            find(&pairs, REPON_HEAD),
            Some(&Some(head_commit.to_string())),
            "REPON_HEAD still carries the commit a detached row's branch slot must not"
        );
    }

    // Criterion 5: Unknown and Not-applicable both unset, never empty.

    #[test]
    fn a_not_applicable_default_branch_unsets_rather_than_emptying_the_variable() {
        let row = entity(
            Kind::Submodule,
            "/repo/vendor/lib",
            "lib",
            "/repo/.git/modules/lib",
            Head::Detached(commit("ccc")),
            Settled::NotApplicable,
        );

        let pairs = environment(&row, None);

        // Distinct from `Some(String::new())`: a shell's bare `${VAR-fallback}`
        // (no colon) only substitutes when the name is unset, so an empty-but-set
        // value would slip a known-wrong default branch through where an unset
        // one cannot.
        assert_eq!(
            find(&pairs, REPON_DEFAULT_BRANCH),
            Some(&None),
            "a Not-applicable default branch must unset the variable, never set it empty"
        );
    }

    #[test]
    fn an_unknown_default_branch_unsets_rather_than_emptying_the_variable() {
        let row = entity(
            Kind::Worktree,
            "/dev/repo",
            "repo",
            "/dev/repo/.git",
            Head::Branch {
                name: Arc::from("main"),
                commit: commit("ddd"),
            },
            Settled::Unknown(Unknown::NoDefaultBranch),
        );

        let pairs = environment(&row, None);

        assert_eq!(
            find(&pairs, REPON_DEFAULT_BRANCH),
            Some(&None),
            "an Unknown default branch must unset the variable, never set it empty"
        );
    }

    // Criterion 6: terminal-prompt suppression is force-set for every child.

    #[test]
    fn git_terminal_prompt_is_force_set_across_more_than_one_row_shape() {
        let attached = entity(
            Kind::Worktree,
            "/dev/repo",
            "repo",
            "/dev/repo/.git",
            Head::Branch {
                name: Arc::from("main"),
                commit: commit("eee"),
            },
            known_default_branch("origin/main"),
        );
        let unborn = entity(
            Kind::Worktree,
            "/dev/fresh",
            "fresh",
            "/dev/fresh/.git",
            Head::Unborn(Arc::from("main")),
            Settled::Unknown(Unknown::NoDefaultBranch),
        );

        for (row, action) in [(&attached, None), (&unborn, Some("reinstall"))] {
            let pairs = environment(row, action);
            assert_eq!(
                find(&pairs, GIT_TERMINAL_PROMPT),
                Some(&Some("0".to_string())),
                "GIT_TERMINAL_PROMPT must be force-set regardless of row shape or action"
            );
        }
    }

    // Criterion 8: the four full-pair-list tests.

    #[test]
    fn the_full_pair_list_for_an_attached_row() {
        let head_commit = commit("1111");
        let row = entity(
            Kind::Worktree,
            "/dev/repo",
            "repo",
            "/dev/parent/.git",
            Head::Branch {
                name: Arc::from("feature"),
                commit: head_commit,
            },
            known_default_branch("origin/main"),
        );

        let mut expected = vec![
            (REPON_REPO_PATH.to_string(), Some("/dev/repo".to_string())),
            (REPON_REPO_NAME.to_string(), Some("repo".to_string())),
            (
                REPON_COMMON_DIR.to_string(),
                Some("/dev/parent/.git".to_string()),
            ),
            (REPON_KIND.to_string(), Some("worktree".to_string())),
            (REPON_BRANCH.to_string(), Some("feature".to_string())),
            (REPON_HEAD.to_string(), Some(head_commit.to_string())),
            (
                REPON_DEFAULT_BRANCH.to_string(),
                Some("origin/main".to_string()),
            ),
            (REPON_ACTION.to_string(), Some("reinstall".to_string())),
            (GIT_TERMINAL_PROMPT.to_string(), Some("0".to_string())),
        ];
        expected.extend(git_unset_pairs());

        assert_eq!(environment(&row, Some("reinstall")), expected);
    }

    #[test]
    fn the_full_pair_list_for_a_detached_row() {
        let head_commit = commit("2222");
        let row = entity(
            Kind::Worktree,
            "/dev/repo-pr-7",
            "repo-pr-7",
            "/dev/repo/.git",
            Head::Detached(head_commit),
            known_default_branch("origin/main"),
        );

        let mut expected = vec![
            (
                REPON_REPO_PATH.to_string(),
                Some("/dev/repo-pr-7".to_string()),
            ),
            (REPON_REPO_NAME.to_string(), Some("repo-pr-7".to_string())),
            (
                REPON_COMMON_DIR.to_string(),
                Some("/dev/repo/.git".to_string()),
            ),
            (REPON_KIND.to_string(), Some("worktree".to_string())),
            (REPON_BRANCH.to_string(), None),
            (REPON_HEAD.to_string(), Some(head_commit.to_string())),
            (
                REPON_DEFAULT_BRANCH.to_string(),
                Some("origin/main".to_string()),
            ),
            (REPON_ACTION.to_string(), None),
            (GIT_TERMINAL_PROMPT.to_string(), Some("0".to_string())),
        ];
        expected.extend(git_unset_pairs());

        assert_eq!(environment(&row, None), expected);
    }

    #[test]
    fn the_full_pair_list_for_an_unborn_row() {
        let row = entity(
            Kind::Worktree,
            "/dev/fresh",
            "fresh",
            "/dev/fresh/.git",
            Head::Unborn(Arc::from("main")),
            known_default_branch("origin/main"),
        );

        let mut expected = vec![
            (REPON_REPO_PATH.to_string(), Some("/dev/fresh".to_string())),
            (REPON_REPO_NAME.to_string(), Some("fresh".to_string())),
            (
                REPON_COMMON_DIR.to_string(),
                Some("/dev/fresh/.git".to_string()),
            ),
            (REPON_KIND.to_string(), Some("worktree".to_string())),
            (REPON_BRANCH.to_string(), Some("main".to_string())),
            (REPON_HEAD.to_string(), None),
            (
                REPON_DEFAULT_BRANCH.to_string(),
                Some("origin/main".to_string()),
            ),
            (REPON_ACTION.to_string(), None),
            (GIT_TERMINAL_PROMPT.to_string(), Some("0".to_string())),
        ];
        expected.extend(git_unset_pairs());

        assert_eq!(environment(&row, None), expected);
    }

    #[test]
    fn the_full_pair_list_for_a_submodule_row() {
        let head_commit = commit("3333");
        let row = entity(
            Kind::Submodule,
            "/repo/vendor/lib",
            "lib",
            "/repo/.git/modules/lib",
            Head::Detached(head_commit),
            Settled::NotApplicable,
        );

        let mut expected = vec![
            (
                REPON_REPO_PATH.to_string(),
                Some("/repo/vendor/lib".to_string()),
            ),
            (REPON_REPO_NAME.to_string(), Some("lib".to_string())),
            (
                REPON_COMMON_DIR.to_string(),
                Some("/repo/.git/modules/lib".to_string()),
            ),
            (REPON_KIND.to_string(), Some("submodule".to_string())),
            (REPON_BRANCH.to_string(), None),
            (REPON_HEAD.to_string(), Some(head_commit.to_string())),
            (REPON_DEFAULT_BRANCH.to_string(), None),
            (REPON_ACTION.to_string(), Some("fetch".to_string())),
            (GIT_TERMINAL_PROMPT.to_string(), Some("0".to_string())),
        ];
        expected.extend(git_unset_pairs());

        assert_eq!(environment(&row, Some("fetch")), expected);
    }
}
