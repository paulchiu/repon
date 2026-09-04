//! Guards the register cleanup this issue made: `README.md` and `docs/spec/*.md` are read
//! back and checked for the specific AI-writing patterns the issue's acceptance criteria
//! ban, so a later edit that reintroduces one of them fails here instead of drifting back in
//! unnoticed. `docs/adr/**` is deliberately not scanned: an ADR is a point-in-time record and
//! is out of scope for this register.

use std::fs;
use std::path::PathBuf;

/// `README.md` plus every `docs/spec/*.md` file, each as `(display path, contents)`.
fn corpus() -> Vec<(String, String)> {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.join("../..");

    let mut paths = vec![repo_root.join("README.md")];
    let spec_dir = repo_root.join("docs/spec");
    let mut spec_files: Vec<PathBuf> = fs::read_dir(&spec_dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", spec_dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    spec_files.sort();
    paths.extend(spec_files);

    paths
        .into_iter()
        .map(|path| {
            let contents = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            (path.display().to_string(), contents)
        })
        .collect()
}

/// The line or sentence containing `needle`, for a readable failure message: bounded by
/// whichever comes first among `.`, a newline, or 200 characters either side.
fn sentence_around<'a>(text: &'a str, needle: &str) -> &'a str {
    let at = text.find(needle).expect("needle present");
    let scan_start = at.saturating_sub(200);
    let start = text[scan_start..at]
        .rfind(['.', '\n'])
        .map_or(scan_start, |i| scan_start + i + 1);
    let scan_end = (at + 200).min(text.len());
    let end = text[at..scan_end]
        .find(['.', '\n'])
        .map_or(scan_end, |i| at + i + 1);
    text[start..end].trim()
}

#[test]
fn no_load_bearing_vocabulary() {
    for (path, text) in corpus() {
        assert!(
            !text.contains("load-bearing"),
            "{path} still says \"load-bearing\": {:?}",
            sentence_around(&text, "load-bearing")
        );
    }
}

#[test]
fn no_em_dashes() {
    for (path, text) in corpus() {
        assert!(
            !text.contains('\u{2014}'),
            "{path} still contains an em dash: {:?}",
            sentence_around(&text, "\u{2014}")
        );
    }
}

/// `not just X, but Y` (or the elliptical `not just X` this issue also removed).
#[test]
fn no_not_just_antithesis() {
    for (path, text) in corpus() {
        assert!(
            !text.contains("not just "),
            "{path} still contains a \"not just\" antithesis: {:?}",
            sentence_around(&text, "not just ")
        );
    }
}

/// `it is not X, it is Y` (or `it is not X: it is Y`): a second `it is` inside the same
/// sentence as an `it is not`.
#[test]
fn no_it_is_not_it_is_antithesis() {
    for (path, text) in corpus() {
        let mut search_from = 0;
        while let Some(rel) = text[search_from..].find("it is not ") {
            let start = search_from + rel;
            let sentence = sentence_around(&text[start..], "it is not ");
            let after_not = &sentence["it is not ".len()..];
            assert!(
                !after_not.contains("it is "),
                "{path} still contains an \"it is not X, it is Y\" antithesis: {sentence:?}"
            );
            search_from = start + "it is not ".len();
        }
    }
}

/// `X is what makes/lets/keeps/stops Y`, the cleft-sentence antithesis this issue's own
/// commit rewrote everywhere else it appeared; `is what actually` is the same shape with an
/// intensifier in front of the verb.
#[test]
fn no_is_what_antithesis() {
    let banned = [
        "is what makes",
        "is what lets",
        "is what keeps",
        "is what stops",
        "is what actually",
    ];
    for (path, text) in corpus() {
        for phrase in banned {
            assert!(
                !text.contains(phrase),
                "{path} still contains the antithesis phrase {phrase:?}: {:?}",
                sentence_around(&text, phrase)
            );
        }
    }
}
