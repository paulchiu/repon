//! The rendering-agnostic core: it computes git state and knows nothing about terminals.
//!
//! Its public surface is flat, re-exported from the crate root rather than through
//! `fanout`, `git`, `entity` or `snapshot`, which stay private: a generic scatter
//! primitive and a single branch read are not vocabulary a second consumer needs,
//! and neither is which file happens to define `EntityState` or `Snapshot`. The one
//! exception is `liveness`, a `test-util`-gated module of test waits that names
//! nothing git-shaped and so belongs under a namespace rather than at the root. The
//! entry points on `Core` itself (`start`, `refresh`, `snapshot`, `try_settle`, ...)
//! land in later work and get re-exported the same way once they exist.
//!
//! ## Reviewing an addition to this surface
//!
//! Every addition to what this crate exports should hold each of these, from
//! `docs/spec/core-api.md`'s ownership table and
//! [ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md):
//!
//! - It does a git-shaped thing (discovery, a probe phase, the metadata poll, a Set
//!   boundary, an override, Generation supersession, the row fold, the display
//!   name, the default branch rung, the environment contract as data, Action
//!   fan-out), never a terminal-shaped one (rendering, the cursor, glyphs, theme,
//!   keybindings, the Launcher, config file discovery, `$HOME`, a user-specific
//!   environment variable).
//! - An empty Selection carries no meaning here: this crate never defaults it to
//!   "the row under the cursor" or anything else that only makes sense on a screen.
//! - `refresh` takes an already-ordered `&[EntityKey]`. This crate never computes
//!   or second-guesses that order; cursor-row-first is the consumer's ordering to
//!   make, not this crate's to infer.
//! - A Filter is a pure predicate over these public types. Deciding when to apply
//!   one, if ever, stays with the consumer.
//! - It has no notification channel, update stream or callback: a consumer reads a
//!   [`Snapshot`] when it decides to, it is never pushed one.
//!
//! Four refusals hold for every type this crate ever makes public, reasoned in
//! [ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md):
//!
//! - No `#[non_exhaustive]`: it forces a consumer to add a wildcard match arm even
//!   when every variant is already matched, reintroducing by attribute the default
//!   path ADR 0001 forbids. ADR 0015 argued this against an in-repo consumer; ADR
//!   0021 observes that publishing widens the audience beyond that case, so the
//!   refusal stands but is not re-argued for the wider one.
//! - No sealed trait: the enforcement ADR 0015 relies on is a real second consumer
//!   (`repon sets`), not defensive API ceremony guarding against a use case nothing
//!   presents.
//! - No separate versioning scheme: repon-core is a path dependency with one
//!   in-workspace consumer, so a breaking change and its fix land in the same commit.
//! - No git-backend trait abstraction: the crate's existing test drives a real
//!   disposable repository rather than a mock, so a trait would buy testability
//!   already paid for.

// Repon is a Unix program, and the restriction lands here rather than on the
// terminal crate because this one owns Action fan-out: a step goes into a new
// session with setsid(2) and is read back over a PTY. See docs/spec/actions.md.
#[cfg(not(unix))]
compile_error!("repon-core requires a Unix target: see docs/spec/actions.md");

mod auto_update;
mod base;
mod cell;
mod core;
mod default_branch;
mod discovery;
mod entity;
mod environment;
mod executor;
mod fanout;
mod fetch;
mod filter;
mod git;
mod landing;
/// The one module reachable by name rather than re-exported flat: it holds no vocabulary a
/// consumer needs, only the wait a test in either crate uses for a liveness property, and a
/// namespace is what keeps names that generic out of the crate root. Gated with it, and the
/// gate is what
/// `every_pub_item_documented_as_test_only_is_either_gated_or_has_a_production_use_site`
/// reads this doc comment to require.
#[cfg(any(test, feature = "test-util"))]
pub mod liveness;
mod patch_equivalence;
mod poll;
mod snapshot;
#[cfg(test)]
mod test_support;
#[cfg(feature = "serde")]
mod wire;

pub use cell::{Cell, Generation, Settled, Timestamp, Unknown};
pub use core::AutoUpdateAttempt;
pub use core::FetchFailures;
pub use core::ManagementHandle;
pub use core::{ActionSpec, AutoUpdateSpec, Core, CoreSpec, FetchSpec, RepoOverride, Step};
pub use discovery::{Discovery, SetSpec, count, discover};
pub use entity::ActionReceipt;
pub use entity::AheadBehind;
pub use entity::CaptureElision;
pub use entity::DefaultBranch;
pub use entity::DefaultBranchStopped;
pub use entity::DeleteRisk;
pub use entity::Diagnostics;
pub use entity::DirtyCounts;
pub use entity::EntityKey;
pub use entity::EntityState;
pub use entity::Head;
pub use entity::Kind;
pub use entity::OwnWork;
pub use entity::Presence;
pub use entity::RunningStep;
pub use entity::Skip;
pub use entity::StepOutcome;
pub use entity::StepResult;
pub use entity::SyncState;
pub use entity::WorktreeState;
pub use environment::environment;
pub use filter::{Applicability, Filter, KeyVocabulary, vocabulary};
pub use git::{InProgressOperation, ProbeError, RecentCommit};
pub use snapshot::{RowSummary, Snapshot, summary};
#[cfg(feature = "serde")]
pub use wire::SettledDocument;

#[cfg(test)]
mod tests {
    /// The exported name from one `pub use` item: `Name`, or the alias in
    /// `Name as Alias`. Panics naming `line` if `item` is neither, so a form this
    /// cannot read fails the test rather than being silently dropped.
    fn exported_name(item: &str, line: &str) -> String {
        if let Some((_, alias)) = item.split_once(" as ") {
            return alias.trim().to_string();
        }
        let name = item.trim();
        assert!(
            !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_'),
            "cannot read an exported name from `{item}` in `pub use` line `{line}`"
        );
        name.to_string()
    }

    /// Everything in `source` ahead of the test module, which is the region where a
    /// crate-root declaration's visibility is decided and the only region either scan
    /// below reads: a `pub` item inside the test module is not part of the surface.
    fn crate_root_declarations(source: &str) -> &str {
        source.split("mod tests {").next().unwrap_or(source)
    }

    /// The crate's actual public surface: every name reachable at the crate root
    /// of `source`, read from the crate's own `src/lib.rs` rather than a hand-kept
    /// list, so a name added without a matching glossary entry has nowhere to hide.
    ///
    /// Three declaration forms carry a name onto that surface, and all three are
    /// read: a `pub use` re-export, a `pub mod` namespace, and a `pub const`. A
    /// line scan, not a parser, since this crate controls the formatting of its own
    /// crate-root lines: it recognises `pub use path::Name;`,
    /// `pub use path::Name as Alias;`, a single-line braced group
    /// `pub use path::{Name, Other as Alias};`, `pub mod name;` and
    /// `pub const NAME: Type = ...;`. Any other `pub ` line at the crate root is a
    /// panic rather than a skip, since a scanner that quietly passes over an
    /// unfamiliar form is the same drift this test exists to catch: `pub mod
    /// liveness;` was invisible to the `pub use`-only version of this scan. What it
    /// does *not* check is privacy: a name collected here is only ever asked whether
    /// the glossary documents it, so a module made public whose name happens to be a
    /// glossary term passes. `docs/spec/releasing.md`'s cleared-gate item 1 is the
    /// privacy claim, and
    /// [`modules_the_release_gate_names_are_private_at_the_crate_root`] is what
    /// enforces it.
    fn crate_root_public_surface(source: &str) -> Vec<String> {
        let mut names = Vec::new();
        for line in crate_root_declarations(source).lines() {
            let line = line.trim();
            if !line.starts_with("pub ") {
                continue;
            }
            if let Some(rest) = line.strip_prefix("pub mod ") {
                let name = rest
                    .strip_suffix(';')
                    .unwrap_or_else(|| panic!("`pub mod` line is not `;`-terminated: `{line}`"));
                names.push(exported_name(name, line));
                continue;
            }
            if let Some(rest) = line.strip_prefix("pub const ") {
                let name = rest
                    .split(':')
                    .next()
                    .unwrap_or_else(|| panic!("`pub const` line names nothing: `{line}`"));
                names.push(exported_name(name, line));
                continue;
            }
            let body = line
                .strip_prefix("pub use ")
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or_else(|| {
                    panic!("crate-root `pub` line is in no form this scan reads: `{line}`")
                });
            match body.split_once('{') {
                Some((_path, rest)) => {
                    let group = rest.strip_suffix('}').unwrap_or_else(|| {
                        panic!("`pub use` group is not `}}`-terminated: `{line}`")
                    });
                    for item in group.split(',') {
                        let item = item.trim();
                        if !item.is_empty() {
                            names.push(exported_name(item, line));
                        }
                    }
                }
                None => {
                    let last = body.rsplit("::").next().unwrap_or(body);
                    names.push(exported_name(last, line));
                }
            }
        }
        names
    }

    /// True if `name`'s words, read together as one phrase, appear in `glossary`.
    ///
    /// A Rust identifier's casing never matches the glossary's Capitalised prose
    /// terms directly, so `name` is split into words on `_` and on a
    /// lowercase-to-uppercase boundary (never inside a run of capitals, or a
    /// SCREAMING_CASE constant would come apart one letter at a time), rejoined
    /// with single spaces, and searched
    /// for case-insensitively. Matching is deliberately whole-phrase rather than
    /// any-single-word: a name like `EntityState` must find "entity state" together,
    /// not pass because "state" alone occurs in unrelated prose such as "Worktree
    /// state". The false-negative risk this leaves: a name whose words are the
    /// glossary's own terms but in a different order, plural, or hyphenated (a
    /// `RepoSet` next to a glossary that only ever writes "a Set of Repos") reads as
    /// undocumented even though a reader would find it.
    fn glossary_covers(glossary: &str, name: &str) -> bool {
        let mut words = Vec::new();
        let mut word = String::new();
        for ch in name.chars() {
            if ch == '_' {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                continue;
            }
            if ch.is_uppercase() && word.ends_with(|last: char| !last.is_uppercase()) {
                words.push(std::mem::take(&mut word));
            }
            word.push(ch);
        }
        if !word.is_empty() {
            words.push(word);
        }
        let phrase = words.join(" ").to_lowercase();
        glossary.to_lowercase().contains(&phrase)
    }

    /// Every crate-root re-export must name something the project glossary already
    /// names, so that reading the crate root and reading the glossary give the same
    /// answer.
    ///
    /// Both files are read at test time from `CARGO_MANIFEST_DIR` rather than with
    /// `include_str!`: `GLOSSARY.md` lives at the repository root, outside this
    /// crate's own directory, so it is not among the files `cargo package` ships.
    /// `include_str!` was tried first; it compiles fine in the workspace checkout
    /// and does not itself break `cargo publish --dry-run` (packaging only builds,
    /// it does not run tests), but `cargo test` against the extracted package
    /// (`target/package/repon-core-*`) then fails to compile at all, a missing-file
    /// error with no test to report it. Reading both paths at runtime instead
    /// degrades that to one failing assertion in a context nothing here needs to
    /// support, rather than a compile error in any context that later turns test
    /// code on.
    #[test]
    fn public_surface_matches_glossary() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(manifest_dir.join("src/lib.rs"))
            .expect("read this crate's own source");
        let glossary = std::fs::read_to_string(manifest_dir.join("../../GLOSSARY.md"))
            .expect("read the project glossary");

        for name in crate_root_public_surface(&source) {
            assert!(
                glossary_covers(&glossary, &name),
                "crate-root public name `{name}` has no matching entry in the project glossary"
            );
        }
    }

    /// The module names `docs/spec/releasing.md`'s cleared-gate item 1 requires to stay
    /// private, read out of the document rather than restated beside it.
    ///
    /// The item spells each one as a backticked `mod name;` fragment, which is what this
    /// picks out: a name the gate stops naming stops being enforced here, and one it adds
    /// is enforced without an edit to this file. An item naming none panics, since a
    /// reworded gate would otherwise leave the test below asserting nothing.
    fn modules_the_release_gate_requires_to_be_private(releasing: &str) -> Vec<String> {
        let gate = releasing
            .split("## Before the first crates.io publish")
            .nth(1)
            .expect("releasing.md must carry the pre-publish gate section");
        let item = gate
            .lines()
            .find(|line| line.trim_start().starts_with("1. "))
            .expect("the pre-publish gate must carry a numbered item 1");
        let names: Vec<String> = item
            .split('`')
            .skip(1)
            .step_by(2)
            .filter_map(|code| Some(code.strip_prefix("mod ")?.strip_suffix(';')?.to_string()))
            .collect();
        assert!(
            !names.is_empty(),
            "cleared-gate item 1 names no `mod name;` fragment any more, so the privacy it \
             records has nothing left to check: {item}"
        );
        names
    }

    /// `docs/spec/releasing.md`'s cleared-gate item 1 is a privacy claim about this crate
    /// root, so the names it makes that claim about are read from the document and asserted
    /// against the source rather than restated here.
    ///
    /// Each named module must still be declared at the crate root, in either form, before
    /// its declaration is required to be the private one: a gate naming a module this crate
    /// no longer has is a stale gate, not a passing check.
    #[test]
    fn modules_the_release_gate_names_are_private_at_the_crate_root() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(manifest_dir.join("src/lib.rs"))
            .expect("read this crate's own source");
        let releasing = std::fs::read_to_string(manifest_dir.join("../../docs/spec/releasing.md"))
            .expect("read the releasing spec");
        let declarations: Vec<&str> = crate_root_declarations(&source)
            .lines()
            .map(str::trim)
            .collect();

        for name in modules_the_release_gate_requires_to_be_private(&releasing) {
            let private = format!("mod {name};");
            let public = format!("pub {private}");
            let declaration = declarations
                .iter()
                .find(|line| **line == private || **line == public)
                .unwrap_or_else(|| {
                    panic!(
                        "`docs/spec/releasing.md`'s cleared-gate item 1 names `{private}`, \
                         which this crate root no longer declares in any form"
                    )
                });
            assert_eq!(
                **declaration, private,
                "`{name}` is public at this crate root, which `docs/spec/releasing.md`'s \
                 cleared-gate item 1 requires to stay private"
            );
        }
    }

    /// Every `.rs` file under `dir`, recursively.
    fn rust_source_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).expect("read a source directory") {
            let path = entry.expect("read a directory entry").path();
            if path.is_dir() {
                files.extend(rust_source_files(&path));
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
        files
    }

    /// `gix::interrupt::IS_INTERRUPTED` is a process-global static wired to
    /// SIGINT; using it would cancel every entity's probe at once, defeating the
    /// one `Arc<AtomicBool>` per in-flight entity [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "Cancellation" requires. Scans every source file under `src`, not just
    /// `core.rs`, so a future module reaching for it is caught too. A line only
    /// counts as real usage when it is not a comment, which is what lets doc
    /// comments (this crate's own, explaining the ban) keep naming it.
    #[test]
    fn gix_interrupt_is_interrupted_is_never_used() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // Built from two pieces rather than written as one path literal, so this
        // check's own source line is never itself a match for what it scans for.
        let banned = format!("interrupt::{}", "IS_INTERRUPTED");
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = std::fs::read_to_string(&path).expect("read a crate source file");
            for (number, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains(&banned) {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "gix's process-global interrupt static must never be used outside a comment, found at: {offending_locations:?}"
        );
    }

    /// `docs/spec/actions.md`'s "The run on screen": "the parse cannot live in
    /// repon-core, because ansi-to-tui produces ratatui types and the core has a CI
    /// line asserting its tree contains no ratatui". `just check-core-isolation`
    /// proves the dependency is absent; this proves the same claim at the source
    /// level, so a hand-rolled parser producing ratatui types some other way (not
    /// through the `ansi-to-tui` dependency at all) is caught too. Scans every
    /// source file under `src`, not just `executor.rs`, since a future module is as
    /// capable of reaching for either name as that one.
    #[test]
    fn no_source_file_in_this_crate_names_the_rendering_crates_that_parse_ansi() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let banned = [
            format!("{}{}", "rata", "tui"),
            format!("{}_{}", "ansi", "to_tui"),
        ];
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = std::fs::read_to_string(&path).expect("read a crate source file");
            for (number, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if banned.iter().any(|needle| line.contains(needle)) {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "found a rendering crate named in repon-core's own source, which must stay raw \
             bytes with no interpretation: {offending_locations:?}"
        );
    }

    /// [`RowSummary`](crate::RowSummary)'s mapping to a gutter glyph is
    /// `docs/spec/core-api.md`'s explicit consumer-side job, never this crate's.
    /// Scans every source file under `src` for the two shapes that mapping would
    /// take here: a function returning a bare `char`, or a match arm whose
    /// right-hand side is a character literal.
    #[test]
    fn no_state_is_mapped_to_a_character_anywhere_in_this_crate() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let banned_return = format!("-{}", "> char");
        let banned_arm = format!("={}", "> '");
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = std::fs::read_to_string(&path).expect("read a crate source file");
            for (number, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains(&banned_return) || line.contains(&banned_arm) {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "repon-core must never map a state to a character; the mapping belongs to the \
             consumer, found at: {offending_locations:?}"
        );
    }

    /// The manifest text a consumer actually resolves against, not a copy: `test-util` gates
    /// `Timestamp::at` off the default published surface per
    /// [ADR 0021](https://github.com/paulchiu/repon/blob/main/docs/adr/0021-a-release-is-what-the-tag-pipeline-publishes.md),
    /// and a `default = [...]` naming it would silently turn every consumer's default build
    /// back into the thing the gate exists to prevent.
    #[test]
    fn test_util_is_never_a_default_feature() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = std::fs::read_to_string(manifest_dir.join("Cargo.toml"))
            .expect("read this crate's own Cargo.toml");
        let features_section = manifest
            .split("[features]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .unwrap_or("");
        assert!(
            features_section.contains("test-util"),
            "expected a `test-util` feature declared in `[features]`; this test's own premise \
             is stale if it moved: {manifest}"
        );
        let default_line = features_section
            .lines()
            .find(|line| line.trim_start().starts_with("default"));
        assert!(
            default_line.is_none_or(|line| !line.contains("test-util")),
            "`test-util` must never be named in a default feature list, or it ships on every \
             consumer's default build: {default_line:?}"
        );
    }
}
