//! `repon sets`: config.md's "Selection order" section fixes its output as each declared
//! Set's name, roots and match count. Built as
//! [core-api.md](../../../docs/spec/core-api.md)'s literal second consumer of
//! [`repon_core::count`], the enforcement its "What enforces this" section relies on: this
//! file's only reach into `repon-core` is `count` and `SetSpec`, never `Core`, an
//! `EntityState`, a `Cell` or a `Snapshot`, so nothing here probes, carries provenance or
//! opens a special-cased path into the core.

use std::io::{self, Write};

use repon_core::{SetSpec, count};

use crate::config::document::{self, Document};

/// Prints every declared Set in file order to standard output: its name, its roots (as
/// written, `~` unexpanded, since that is what the user typed) and its match count, calling
/// [`count`] exactly once per Set with the same [`SetSpec`] shape [`crate::app`]'s own
/// `CoreSpec` builds one from.
pub fn print(document: &Document) {
    write_sets(document, &mut io::stdout()).expect("write to stdout");
}

/// [`print()`]'s whole effect, against an injected writer rather than real stdout, which is
/// what lets a test read the printed lines back without a pipe or a subprocess.
fn write_sets(document: &Document, out: &mut impl Write) -> io::Result<()> {
    for set in &document.sets {
        let spec = SetSpec {
            name: set.name.get_ref().clone(),
            roots: set
                .roots
                .iter()
                .map(|root| document::expand_home(root))
                .collect(),
            include: set.include.clone().unwrap_or_default(),
            exclude: set.exclude.clone().unwrap_or_default(),
        };
        let matches = count(&spec);
        let roots = set.roots.join(", ");
        writeln!(
            out,
            "{}  roots: {roots}  matches: {matches}",
            set.name.get_ref()
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::test_support::{production_source_at, rust_source_files};

    /// A real disposable git repository, the same pattern `app.rs`'s own `init_repo` uses:
    /// `count` only needs a `.git` boundary to exist, so `git init` alone is enough, with no
    /// commit required.
    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).expect("create repo dir");
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(path)
            .status()
            .expect("run git init");
        assert!(status.success());
    }

    fn set_config(name: &str, root: &Path) -> document::SetConfig {
        document::SetConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            roots: vec![root.to_string_lossy().into_owned()],
            include: None,
            exclude: None,
        }
    }

    /// Two Sets over two distinct roots with distinct repo counts, so an implementation that
    /// computed one count and printed it for every line (calling `count` once for the whole
    /// document rather than once per Set) fails here: each printed line must carry its own
    /// Set's own name, roots and number.
    #[test]
    fn prints_each_declared_sets_own_name_roots_and_match_count() {
        let dir_a = tempfile::tempdir().expect("temp dir a");
        let root_a = dir_a.path().canonicalize().expect("canonicalize");
        init_repo(&root_a.join("repo-a1"));
        init_repo(&root_a.join("repo-a2"));

        let dir_b = tempfile::tempdir().expect("temp dir b");
        let root_b = dir_b.path().canonicalize().expect("canonicalize");
        init_repo(&root_b.join("repo-b1"));

        let mut document = Document::default();
        document.sets.push(set_config("alpha", &root_a));
        document.sets.push(set_config("beta", &root_b));

        let mut buffer = Vec::new();
        write_sets(&document, &mut buffer).expect("write");
        let output = String::from_utf8(buffer).expect("utf8");
        let lines: Vec<&str> = output.lines().collect();

        assert_eq!(
            lines.len(),
            2,
            "expected one line per declared Set: {lines:?}"
        );
        assert!(
            lines[0].starts_with("alpha") && lines[0].contains(&root_a.display().to_string()),
            "expected alpha's own roots first, got: {:?}",
            lines[0]
        );
        assert!(
            lines[0].contains("matches: 2"),
            "expected alpha's own count of 2, got: {:?}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("beta") && lines[1].contains(&root_b.display().to_string()),
            "expected beta's own roots second, got: {:?}",
            lines[1]
        );
        assert!(
            lines[1].contains("matches: 1"),
            "expected beta's own count of 1, not alpha's, got: {:?}",
            lines[1]
        );
    }

    /// Criterion 2's "no special-cased path into the core", and half of criterion 3's "no
    /// cell ever written": this file's own production source (not the whole crate, since
    /// unrelated modules legitimately use `Core`) must never reach past `count` and `SetSpec`
    /// into anything that could construct an `EntityState` or touch a `Cell`. Scoped to this
    /// one file rather than the whole crate because `Core`, `EntityState` and `Cell` are
    /// legitimate vocabulary everywhere else `repon` talks to `repon-core`; the absence claim
    /// is about this consumer's own reach, not about the crate as a whole.
    #[test]
    fn this_files_production_source_names_no_core_symbol_beyond_count_and_setspec() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let this_file = manifest_dir.join("src/sets.rs");
        let production = production_source_at(&this_file);

        let forbidden = [
            "Core",
            "EntityState",
            "Cell",
            "Snapshot",
            "probe_now",
            "discover(",
            "resolve(",
            "summary(",
            "environment(",
        ];
        let mut offending = Vec::new();
        for (number, line) in production.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for needle in forbidden {
                if line.contains(needle) {
                    offending.push(format!("{}:{} ({needle})", this_file.display(), number + 1));
                }
            }
        }
        assert!(
            offending.is_empty(),
            "found a repon-core symbol beyond `count`/`SetSpec` in sets.rs, which would mean \
             this consumer probes, carries provenance, or opens a special-cased path into the \
             core: {offending:?}"
        );
    }

    /// The counting claim itself: [`write_sets`] calls `count(` exactly once, inside its one
    /// loop over `document.sets`. A second call site (validating the count before printing
    /// it, or computing it twice for two different messages) would still leave
    /// `prints_each_declared_sets_own_name_roots_and_match_count` green, since both calls
    /// would agree on the same real repositories; this catches that mutation by shape rather
    /// than by output.
    #[test]
    fn write_sets_calls_count_exactly_once() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let production = production_source_at(&manifest_dir.join("src/sets.rs"));
        let call_sites: Vec<usize> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                !line.trim_start().starts_with("//") && line.contains("count(&spec)")
            })
            .map(|(number, _)| number + 1)
            .collect();
        assert_eq!(
            call_sites.len(),
            1,
            "expected exactly one `count(&spec)` call site in sets.rs, found at lines: \
             {call_sites:?}"
        );
    }

    /// The same absence claim as
    /// [`this_files_production_source_names_no_core_symbol_beyond_count_and_setspec`], read
    /// from `repon-core`'s own side: `count`'s defining file must itself never construct an
    /// `EntityState` or a `Cell`, which is what makes "no cell ever written" true by
    /// construction rather than by a probe simply not happening to run this time.
    ///
    /// The file is found by locating `count`'s own definition rather than by name, so moving
    /// the function carries the scan with it; a scan keyed to a filename would inspect
    /// nothing and pass after such a move.
    #[test]
    fn counts_defining_file_never_constructs_an_entity_state_or_a_cell() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let core_src = manifest_dir.join("../repon-core/src");
        let repon_src = manifest_dir.join("src");

        let mut definitions = Vec::new();
        for dir in [core_src, repon_src] {
            for path in rust_source_files(&dir) {
                let production = production_source_at(&path);
                if production.contains("pub fn count(spec: &SetSpec)") {
                    definitions.push((path, production));
                }
            }
        }

        assert_eq!(
            definitions.len(),
            1,
            "expected exactly one definition of `count` to scan, found {}: {:?}",
            definitions.len(),
            definitions
                .iter()
                .map(|(path, _)| path.display().to_string())
                .collect::<Vec<_>>()
        );

        let (path, production) = &definitions[0];
        let offending: Vec<String> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim_start().starts_with("//"))
            .filter(|(_, line)| line.contains("EntityState") || line.contains("Cell"))
            .map(|(number, _)| format!("{}:{}", path.display(), number + 1))
            .collect();

        assert!(
            offending.is_empty(),
            "found `EntityState` or `Cell` in `count`'s own defining file: {offending:?}"
        );
    }
}
