//! Holds the convention that a live decision record names what enforces it. Every record in
//! `docs/adr/` that is not marked retired must carry an `**Enforcement:**` line, and every
//! symbol, recipe and path that line names in backticks must exist. The prose register in
//! `docs_register.rs` deliberately skips `docs/adr/`; this checks structure rather than prose,
//! so the two do not overlap.
//!
//! What this cannot check is whether a named test actually holds the claim above it. That stays
//! a review job, and the record says so.

use std::fs;
use std::path::{Path, PathBuf};

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `docs/adr/*.md` file, sorted, as `(file name, contents)`.
fn records() -> Vec<(String, String)> {
    let dir = repo_root().join("docs/adr");
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let contents = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let name = path
                .file_name()
                .expect("adr file name")
                .to_string_lossy()
                .into_owned();
            (name, contents)
        })
        .collect()
}

/// A record is retired when a blockquote in its opening lines says so. Liveness is read from
/// the file itself rather than from a list, which would be one more thing to go stale.
fn is_retired(contents: &str) -> bool {
    contents
        .lines()
        .take(6)
        .any(|line| line.starts_with("> **Retired."))
}

/// The whole `**Enforcement:**` paragraph, which may wrap over several lines and ends at the
/// first blank line.
fn enforcement(contents: &str) -> Option<String> {
    let start = contents.find("**Enforcement:**")?;
    let rest = &contents[start..];
    let end = rest.find("\n\n").unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Every backtick-delimited span in `text`.
fn backticked(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The whole workspace's Rust source, concatenated once, for symbol lookups.
fn rust_sources() -> String {
    fn walk(dir: &Path, out: &mut String) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(text) = fs::read_to_string(&path)
            {
                out.push_str(&text);
                out.push('\n');
            }
        }
    }
    let root = repo_root();
    let mut out = String::new();
    walk(&root.join("crates"), &mut out);
    walk(&root.join("tools"), &mut out);
    out
}

/// What a backticked span in an `Enforcement` line claims exists, when that is decidable. Spans
/// naming a type, a fragment of prose or a compiler diagnostic are not claims about a symbol and
/// are skipped rather than guessed at.
enum Claim {
    /// A `just` recipe, which must be declared in the justfile.
    Recipe(String),
    /// A repository-relative path, which must be a real file.
    File(String),
    /// A function, which must be defined somewhere under `crates/` or `tools/`.
    Function(String),
    Unchecked,
}

fn classify(span: &str) -> Claim {
    if let Some(recipe) = span.strip_prefix("just ") {
        return Claim::Recipe(recipe.trim().to_string());
    }
    if span.contains('/') && !span.contains('*') && Path::new(span).extension().is_some() {
        return Claim::File(span.to_string());
    }
    // A `Type::method` span is a claim about the method.
    let tail = span.rsplit("::").next().unwrap_or(span);
    let snake_case = |s: &str| {
        !s.is_empty()
            && s.contains('_')
            && s.starts_with(|c: char| c.is_ascii_lowercase())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    if snake_case(tail) {
        return Claim::Function(tail.to_string());
    }
    Claim::Unchecked
}

#[test]
fn every_live_record_names_its_enforcement() {
    let missing: Vec<String> = records()
        .into_iter()
        .filter(|(_, contents)| !is_retired(contents))
        .filter(|(_, contents)| enforcement(contents).is_none())
        .map(|(name, _)| name)
        .collect();
    assert!(
        missing.is_empty(),
        "these records are live and carry no `**Enforcement:**` line, so a reader cannot tell \
         whether the constraint is held or merely asserted: {missing:?}"
    );
}

#[test]
fn every_enforcement_line_names_something_that_exists() {
    let root = repo_root();
    let justfile = fs::read_to_string(root.join("justfile")).expect("read justfile");
    let sources = rust_sources();
    let mut failures = Vec::new();

    for (name, contents) in records() {
        if is_retired(&contents) {
            continue;
        }
        let Some(line) = enforcement(&contents) else {
            continue;
        };
        for span in backticked(&line) {
            let found = match classify(&span) {
                Claim::Recipe(recipe) => justfile
                    .lines()
                    .any(|l| l.starts_with(&format!("{recipe}:"))),
                Claim::File(path) => root.join(&path).exists(),
                Claim::Function(function) => sources.contains(&format!("fn {function}")),
                Claim::Unchecked => true,
            };
            if !found {
                failures.push(format!("{name} names `{span}`, which does not exist"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "an enforcement line that names a guard nobody has is worse than admitting there is \
         none:\n{}",
        failures.join("\n")
    );
}

/// The retirement banner: the blockquote run opening with `> **Retired.`, joined into one line.
fn retirement_banner(contents: &str) -> Option<String> {
    let mut lines = contents
        .lines()
        .skip_while(|line| !line.starts_with("> **Retired."));
    let first = lines.next()?;
    let rest = lines.take_while(|line| line.starts_with('>'));
    Some(
        std::iter::once(first)
            .chain(rest)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

#[test]
fn a_retired_record_accounts_for_itself_in_its_banner() {
    // A record leaves the live set one of two ways: its content moved, and the banner links
    // where to, or the decision was withdrawn and the banner says so. Leaving silently is the
    // third way, and the only one this rejects.
    let unaccounted: Vec<String> = records()
        .into_iter()
        .filter(|(_, contents)| is_retired(contents))
        .filter(|(_, contents)| {
            let banner = retirement_banner(contents).unwrap_or_default();
            !(banner.contains("](../product.md)")
                || banner.contains("](../spec/")
                || banner.contains("Withdrawn"))
        })
        .map(|(name, _)| name)
        .collect();
    assert!(
        unaccounted.is_empty(),
        "a retired record must say where its content went, or that it was withdrawn: \
         {unaccounted:?}"
    );
}
