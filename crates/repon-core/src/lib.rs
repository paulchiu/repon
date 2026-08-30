//! The rendering-agnostic core: it computes git state and knows nothing about terminals.
//!
//! Its public surface is flat, re-exported from the crate root rather than through
//! `fanout` or `git`, which are private: a generic scatter primitive and a single
//! branch read are not vocabulary a second consumer needs. That surface is currently
//! empty, because neither of today's two primitives is a name the project glossary
//! (`CONTEXT.md`) names either; the types `docs/spec/core-api.md` describes land in
//! later work and get re-exported the same way once they exist.
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

mod fanout;
mod git;

#[cfg(test)]
mod tests {
    /// Names this crate re-exports from its root. There is no `pub use` yet, so
    /// this is empty; update it by hand alongside any `pub use` this crate root
    /// gains, since nothing here parses the source to find them automatically.
    const PUBLIC_SURFACE: &[&str] = &[];

    /// True if `name`'s words, read together as one phrase, appear in `glossary`.
    ///
    /// A Rust identifier's casing never matches the glossary's Capitalised prose
    /// terms directly, so `name` is split into words on `_` and on a
    /// lowercase-to-uppercase boundary, rejoined with single spaces, and searched
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
            if ch.is_uppercase() && !word.is_empty() {
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
    /// Read from `CARGO_MANIFEST_DIR` at test time rather than with `include_str!`:
    /// `CONTEXT.md` lives at the repository root, outside this crate's own directory,
    /// so it is not among the files `cargo package` ships. `include_str!` was tried
    /// first; it compiles fine in the workspace checkout and does not itself break
    /// `cargo publish --dry-run` (packaging only builds, it does not run tests), but
    /// `cargo test` against the extracted package (`target/package/repon-core-*`)
    /// then fails to compile at all, a missing-file error with no test to report it.
    /// Reading the path at runtime instead degrades that to one failing assertion in
    /// a context nothing here needs to support, rather than a compile error in any
    /// context that later turns test code on.
    #[test]
    fn public_surface_matches_glossary() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../CONTEXT.md");
        let glossary = std::fs::read_to_string(&path).expect("read the project glossary");

        for name in PUBLIC_SURFACE {
            assert!(
                glossary_covers(&glossary, name),
                "public re-export `{name}` has no matching entry in the project glossary"
            );
        }
    }
}
