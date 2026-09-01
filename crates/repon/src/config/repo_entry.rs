//! The `[[repo]]` entries Repon writes, read-modify-written on the file on disk.
//!
//! [0028](../../../../docs/adr/0028-repon-writes-the-repo-entries-it-owns.md) bounds what may
//! be written to `[[repo]]` and nothing else, and
//! [repo-management.md](../../../../docs/spec/repo-management.md)'s "Writing config" fixes the
//! mechanism: every write reads the file, edits the parsed text and writes it back, so a
//! concurrent hand edit loses only the keys both writers touched. Nothing here ever
//! serialises [`super::Document`], which would rewrite the whole file from this crate's own
//! structs and drop every comment the file exists to carry.
//!
//! `toml_edit` rather than the `toml` the schema is deserialised with: a hand-edited
//! document only survives a round trip through a format-preserving parser.

use std::{fs, path::Path};

use color_eyre::eyre::{Result, WrapErr};
use toml_edit::{DocumentMut, Item, Table, value};

use super::document::expand_home;

/// The array of tables this module is bounded to. Named once so no call site below can
/// reach a different one by typo.
const REPO: &str = "repo";

/// The one key an entry Repon appends carries besides `path`, and the one key `unignore`
/// removes.
const EXCLUDE: &str = "exclude";

const PATH: &str = "path";

/// What a management operation asks of one `[[repo]]` entry, per
/// [repo-management.md](../../../../docs/spec/repo-management.md)'s operations table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edit {
    /// `ignore`: `exclude = true`, on the entry the path already has or on one appended
    /// with its provenance comment.
    Exclude,
    /// `unignore`: the `exclude` key alone, leaving whatever else the table carries. An
    /// entry left with nothing but `path` goes with it.
    Unexclude,
    /// `delete`: the whole entry, once the working tree is gone.
    Remove,
}

/// The comment an entry Repon appends carries on the line above it
/// ([repo-management.md](../../../../docs/spec/repo-management.md)'s "Writing config"): the
/// file's whole value under [0014](../../../../docs/adr/0014-config-is-read-only-and-a-set-bounds-the-work.md)
/// is that its comments say more than its values, so an entry appearing with no explanation
/// is the loss that argument was protecting against.
fn provenance_comment(today: &str) -> String {
    format!("# ignored from Repon on {today}")
}

/// Today's date as `YYYY-MM-DD`, the leading date of [`repon_core::Timestamp`]'s own RFC 3339
/// rendering: UTC, and read from the core's one calendar rather than a second one here.
pub(crate) fn today() -> String {
    let now = repon_core::Timestamp::now().to_string();
    now.split('T').next().unwrap_or(&now).to_string()
}

/// Reads `config_file`, applies `edit` to the `[[repo]]` entry naming `path`, and writes the
/// result back. A missing file is read as the empty document it already behaves as
/// ([`super::document::load`]'s own "a missing file is not an error"), so a first `ignore`
/// with no config at all still writes one.
///
/// Returns whether the file's text changed, so a caller can tell an edit that did something
/// from one that found nothing to do.
pub(crate) fn write(config_file: &Path, path: &Path, edit: Edit) -> Result<bool> {
    let before = match fs::read_to_string(config_file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).wrap_err_with(|| format!("could not read {}", config_file.display()));
        }
    };
    let after = apply(&before, path, edit, &today())?;
    if after == before {
        return Ok(false);
    }
    if let Some(parent) = config_file.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(config_file, &after)
        .wrap_err_with(|| format!("could not write {}", config_file.display()))?;
    Ok(true)
}

/// `text` with `edit` applied to the `[[repo]]` entry naming `path`, and nothing else
/// touched.
///
/// Matching is by expanded path rather than by the written string, so an entry the user wrote
/// as `~/dev/noisy` is found by the absolute path Repon knows the entity by. An appended
/// entry is written back in the `~`-relative form the file's own examples use, since that is
/// the form a user reading the file afterwards would have written themselves.
pub(crate) fn apply(text: &str, path: &Path, edit: Edit, today: &str) -> Result<String> {
    let mut document: DocumentMut = text
        .parse()
        .wrap_err("could not parse the config file to edit it")?;

    let existing = find_entry(&document, path);
    match (edit, existing) {
        (Edit::Exclude, Some(index)) => {
            entries_mut(&mut document)
                .and_then(|entries| entries.get_mut(index))
                .expect("the index just found still resolves")[EXCLUDE] = value(true);
        }
        (Edit::Exclude, None) => append_entry(&mut document, path, today),
        (Edit::Unexclude, Some(index)) => {
            let Some(entries) = entries_mut(&mut document) else {
                unreachable!("an index was found, so the array of tables is there");
            };
            let entry = entries
                .get_mut(index)
                .expect("the index just found still resolves");
            entry.remove(EXCLUDE);
            if carries_only_a_path(entry) {
                entries.remove(index);
            }
        }
        (Edit::Remove, Some(index)) => {
            let Some(entries) = entries_mut(&mut document) else {
                unreachable!("an index was found, so the array of tables is there");
            };
            entries.remove(index);
        }
        (Edit::Unexclude | Edit::Remove, None) => {}
    }
    Ok(document.to_string())
}

/// The index of the `[[repo]]` entry whose `path` expands to `path`, if any. A duplicate
/// path cannot reach here: [`super::document::load`] rejects the whole file for one
/// ([`super::document`]'s own `reject_duplicate_names`).
fn find_entry(document: &DocumentMut, path: &Path) -> Option<usize> {
    document
        .get(REPO)?
        .as_array_of_tables()?
        .iter()
        .position(|entry| entry_path_matches(entry, path))
}

fn entry_path_matches(entry: &Table, path: &Path) -> bool {
    entry
        .get(PATH)
        .and_then(Item::as_str)
        .is_some_and(|declared| expand_home(declared) == path)
}

fn entries_mut(document: &mut DocumentMut) -> Option<&mut toml_edit::ArrayOfTables> {
    document.get_mut(REPO)?.as_array_of_tables_mut()
}

/// Whether `entry` is left with nothing to say: only its own `path` key, which alone
/// declares no fact at all ([repo-management.md](../../../../docs/spec/repo-management.md)'s
/// "Writing config"). Counts keys rather than naming the ones it knows, so a `[[repo]]` key
/// added to the schema later keeps its own entry alive here without editing this.
///
/// Removing the last such entry is also what removes the array of tables: `toml_edit` renders
/// an emptied `ArrayOfTables` as no text at all, so the array goes with its last entry with
/// no separate step. Measured rather than assumed, and `ignore_then_unignore_on_a_file_with_no_repo_array_returns_it_byte_for_byte`
/// is what holds it.
fn carries_only_a_path(entry: &Table) -> bool {
    entry.len() == 1 && entry.contains_key(PATH)
}

/// Appends a new `[[repo]]` entry carrying `path`, `exclude = true` and its provenance
/// comment. `toml_edit` lands it after the last existing `[[repo]]` when the array is already
/// there, and at the end of the document when it is not; the blank line in the prefix is what
/// separates it from whatever it lands after, and the trailing comment a document ends with
/// stays after it rather than being captured as this table's own.
fn append_entry(document: &mut DocumentMut, path: &Path, today: &str) {
    let mut entry = Table::new();
    entry[PATH] = value(contract_home(path));
    entry[EXCLUDE] = value(true);
    entry
        .decor_mut()
        .set_prefix(format!("\n{}\n", provenance_comment(today)));

    let entries = document
        .entry(REPO)
        .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    if let Some(entries) = entries.as_array_of_tables_mut() {
        entries.push(entry);
    }
}

/// `path` written the way the file's own examples write one: `~`-relative when it is under
/// the home directory, absolute otherwise. The inverse of
/// [`super::document::expand_home`], which is what reads it back.
fn contract_home(path: &Path) -> String {
    if let Ok(home) = etcetera::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This repository's own commented config, in the house style
    /// [config.md](../../../../docs/spec/config.md)'s annotated example fixes: comments above
    /// tables, comments at the end of a line, `[[repo]]` entries carrying `default_branch`
    /// and `exclude`, and a `[[launcher]]` array following them. Read at test time rather
    /// than restated here, so a fixture written to suit these assertions cannot drift from
    /// the one the program actually ships.
    fn commented_fixture() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config/example.toml"),
        )
        .expect("read the shipped annotated example")
    }

    /// Every comment line in `text`, in order: a whole-line comment or the trailing half of
    /// a line that carries one.
    fn comments(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix('#') {
                    return Some(format!("#{rest}"));
                }
                trimmed.find(" # ").map(|at| trimmed[at + 1..].to_string())
            })
            .collect()
    }

    fn path(declared: &str) -> std::path::PathBuf {
        expand_home(declared)
    }

    /// Criterion 1: a comment anywhere in the file survives every write. Every one of the
    /// three edits is run against the same commented fixture, and each is asserted to leave
    /// the file's whole comment list unchanged, except for the one comment an appended entry
    /// adds and the one a removed entry takes with it, which are named rather than tolerated.
    #[test]
    fn every_comment_in_the_commented_fixture_survives_every_write() {
        let before = commented_fixture();
        let original = comments(&before);
        assert!(
            original.len() > 5,
            "the fixture has to actually carry comments for this to prove anything, found {original:?}"
        );

        // `exclude` set on an entry that already exists: nothing is added or removed.
        let after = apply(
            &before,
            &path("~/dev/legacy-api"),
            Edit::Exclude,
            "2026-09-01",
        )
        .expect("the fixture parses");
        assert_eq!(comments(&after), original);

        // `exclude` removed from an entry that still carries `default_branch`: the table
        // stays, so its own comment does too.
        let after = apply(
            &after,
            &path("~/dev/legacy-api"),
            Edit::Unexclude,
            "2026-09-01",
        )
        .expect("the written document parses");
        assert_eq!(comments(&after), original);

        // An appended entry adds exactly its own provenance comment and nothing else. Its
        // position among the others is asserted separately, by
        // `an_appended_entry_lands_after_the_last_existing_repo_entry`.
        let after = apply(&before, &path("~/dev/noisy"), Edit::Exclude, "2026-09-01")
            .expect("the fixture parses");
        let written = comments(&after);
        let mut without_the_new_one = written.clone();
        without_the_new_one.retain(|comment| comment != &provenance_comment("2026-09-01"));
        assert_eq!(without_the_new_one, original);
        assert_eq!(
            written.len(),
            original.len() + 1,
            "an append must add its own comment once and no other"
        );

        // A removed entry takes the comment written above it, which is a comment about the
        // entry being removed, and no other.
        let after = apply(
            &before,
            &path("~/dev/legacy-api"),
            Edit::Remove,
            "2026-09-01",
        )
        .expect("the fixture parses");
        let lost: Vec<&String> = original
            .iter()
            .filter(|comment| !comments(&after).contains(comment))
            .collect();
        assert_eq!(
            lost,
            vec!["# origin/HEAD on this one still says master; pin it."],
            "removing an entry must take its own leading comment and no other"
        );
    }

    /// Criterion 1, sharpened: the write is bounded to `[[repo]]`, so nothing the user
    /// hand-wrote is reformatted. Every line of the fixture that is not part of the one
    /// entry being edited comes back byte for byte.
    #[test]
    fn a_write_reformats_nothing_outside_the_repo_entry_it_edits() {
        let before = commented_fixture();
        let after = apply(
            &before,
            &path("~/dev/legacy-api"),
            Edit::Exclude,
            "2026-09-01",
        )
        .expect("the fixture parses");

        // One line inserted into the one table this edit names, and every other byte of the
        // file identical: taking that single line back out has to give the original exactly.
        assert_eq!(
            after.replacen(
                "default_branch = \"main\"\nexclude = true\n",
                "default_branch = \"main\"\n",
                1
            ),
            before,
            "a write must touch nothing but the `[[repo]]` entry it names, got {after:?}"
        );
    }

    /// Criterion 2: `ignore` then `unignore` on a file that had no `[[repo]]` array returns
    /// it byte for byte, the array of tables going with the last entry in it.
    #[test]
    fn ignore_then_unignore_on_a_file_with_no_repo_array_returns_it_byte_for_byte() {
        let before = "# the whole file's value is its comments\ntheme = \"default\"\n\n\
                      [refresh]\npoll_interval = \"2s\"   # a trailing comment\n\n\
                      # a comment at the end of the file\n";

        let ignored = apply(before, &path("~/dev/noisy"), Edit::Exclude, "2026-09-01")
            .expect("the document parses");
        assert!(
            ignored.contains("[[repo]]"),
            "the ignore has to have written something for the round trip to prove anything: \
             {ignored:?}"
        );
        let unignored = apply(
            &ignored,
            &path("~/dev/noisy"),
            Edit::Unexclude,
            "2026-09-01",
        )
        .expect("the written document parses");

        assert_eq!(unignored, before);
    }

    /// [0028](../../../../docs/adr/0028-repon-writes-the-repo-entries-it-owns.md)'s measured
    /// claim, made a test: an appended entry lands after the last existing `[[repo]]` and
    /// before the array of tables that follows it, rather than at the end of the file.
    #[test]
    fn an_appended_entry_lands_after_the_last_existing_repo_entry() {
        let before = commented_fixture();

        let after = apply(&before, &path("~/dev/noisy"), Edit::Exclude, "2026-09-01")
            .expect("the fixture parses");

        let lines: Vec<&str> = after.lines().collect();
        let appended = lines
            .iter()
            .position(|line| *line == "path = \"~/dev/noisy\"")
            .expect("the appended entry is there");
        let vendor_mirror = lines
            .iter()
            .position(|line| *line == "path = \"~/dev/vendor-mirror\"")
            .expect("the last existing entry is still there");
        let first_launcher = lines
            .iter()
            .position(|line| *line == "[[launcher]]")
            .expect("the following array of tables is still there");
        assert!(
            vendor_mirror < appended && appended < first_launcher,
            "the appended entry must land after the last `[[repo]]` and before the following \
             `[[launcher]]`, got {after:?}"
        );
    }

    /// Criterion 2's other half: the end-of-file comment stays at the end rather than being
    /// captured by the appended table, which is what the byte-for-byte round trip above
    /// would otherwise hide by restoring either way.
    #[test]
    fn an_appended_entry_lands_after_a_trailing_comment_rather_than_capturing_it() {
        let before = "theme = \"default\"\n\n# a comment at the end of the file\n";

        let after = apply(before, &path("~/dev/noisy"), Edit::Exclude, "2026-09-01")
            .expect("the document parses");

        let end_of_file = after
            .lines()
            .position(|line| line == "# a comment at the end of the file")
            .expect("the trailing comment survives");
        let header = after
            .lines()
            .position(|line| line == "[[repo]]")
            .expect("the appended entry is there");
        assert!(
            header < end_of_file,
            "the trailing comment must stay at the end rather than being captured as the \
             appended table's own leading comment: {after:?}"
        );
    }

    /// Criterion 3: `unignore` on an entry carrying `default_branch` removes the `exclude`
    /// key alone and leaves the table, since the table still states a fact.
    #[test]
    fn unignore_on_an_entry_carrying_default_branch_removes_exclude_alone() {
        let before = "# pinned, and ignored for now\n[[repo]]\npath = \"~/dev/legacy-api\"\n\
                      default_branch = \"main\"\nexclude = true\n";

        let after = apply(
            before,
            &path("~/dev/legacy-api"),
            Edit::Unexclude,
            "2026-09-01",
        )
        .expect("the document parses");

        assert_eq!(
            after,
            "# pinned, and ignored for now\n[[repo]]\npath = \"~/dev/legacy-api\"\n\
             default_branch = \"main\"\n"
        );
    }

    /// Criterion 3's boundary: the same edit on an entry carrying nothing but `path` and
    /// `exclude` takes the whole table, and the array of tables with it.
    #[test]
    fn unignore_on_an_entry_left_with_nothing_but_path_removes_the_table_and_the_array() {
        let before = "theme = \"default\"\n\n[[repo]]\npath = \"~/dev/noisy\"\nexclude = true\n";

        let after = apply(before, &path("~/dev/noisy"), Edit::Unexclude, "2026-09-01")
            .expect("the document parses");

        assert!(
            !after.contains("[[repo]]") && !after.contains("noisy"),
            "an entry left with nothing but `path` must go entirely: {after:?}"
        );
    }

    /// Criterion 4: an appended entry carries its provenance comment on the line above it,
    /// in the shape repo-management.md's own example shows, and the date it names is the one
    /// it was given.
    #[test]
    fn an_appended_entry_carries_its_provenance_comment_on_the_line_above_it() {
        let after = apply("", &path("~/dev/noisy"), Edit::Exclude, "2026-09-01")
            .expect("an empty document parses");

        let lines: Vec<&str> = after.lines().filter(|line| !line.is_empty()).collect();
        assert_eq!(
            lines,
            vec![
                "# ignored from Repon on 2026-09-01",
                "[[repo]]",
                "path = \"~/dev/noisy\"",
                "exclude = true",
            ],
            "got {after:?}"
        );
    }

    /// Criterion 4, pinned to the specification rather than to this module's own words: the
    /// fenced example in repo-management.md is read at test time and reproduced from
    /// [`apply`]'s own output for the same path and date, so the comment's wording cannot
    /// drift from the document that fixes it.
    #[test]
    fn the_appended_entry_matches_repo_management_mds_own_fenced_example() {
        let spec = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/spec/repo-management.md"),
        )
        .expect("read docs/spec/repo-management.md");
        let fenced = spec
            .split("```toml\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("repo-management.md still carries its fenced provenance example");
        let date = fenced
            .lines()
            .next()
            .and_then(|line| line.rsplit(' ').next())
            .expect("the example's comment names a date");

        let written =
            apply("", &path("~/dev/noisy"), Edit::Exclude, date).expect("an empty document parses");

        assert_eq!(written.trim_start_matches('\n'), fenced);
    }

    /// The entry Repon appends is matched again by the absolute path the entity is known by,
    /// not only by the `~`-relative string it was written as, which is what makes an
    /// `unignore` after an `ignore` find its own entry at all.
    #[test]
    fn an_appended_entry_is_found_again_by_the_absolute_path_it_was_written_for() {
        let absolute = expand_home("~/dev/noisy");
        let written =
            apply("", &absolute, Edit::Exclude, "2026-09-01").expect("an empty document parses");

        assert_eq!(
            find_entry(&written.parse().expect("parses"), &absolute),
            Some(0)
        );
    }

    /// `Edit::Remove` on a path with no `[[repo]]` entry at all leaves the file alone, which
    /// is the ordinary case for `delete` on a Repo the user never configured.
    #[test]
    fn removing_an_entry_that_was_never_there_changes_nothing() {
        let before = commented_fixture();

        let after = apply(
            &before,
            &path("~/dev/never-configured"),
            Edit::Remove,
            "2026-09-01",
        )
        .expect("the fixture parses");

        assert_eq!(after, before);
    }

    /// The file on disk is what is read and written, never a serialisation of the in-memory
    /// document: a key this crate's schema does not know survives a write it was not part of.
    #[test]
    fn a_key_this_crates_schema_never_reads_survives_a_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("config.toml");
        std::fs::write(
            &file,
            "# hand-written\nsomething_repon_has_never_heard_of = 3\n",
        )
        .expect("write the config file");

        let changed = write(&file, &path("~/dev/noisy"), Edit::Exclude).expect("the write runs");

        assert!(changed);
        let after = std::fs::read_to_string(&file).expect("read it back");
        assert!(
            after.contains("something_repon_has_never_heard_of = 3")
                && after.contains("# hand-written"),
            "a read-modify-write of the file on disk must keep what it does not understand: \
             {after:?}"
        );
    }

    /// A first `ignore` with no config file at all writes one rather than failing, the same
    /// way a missing file is not an error to read.
    #[test]
    fn a_write_with_no_config_file_at_all_creates_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("nested").join("config.toml");

        let changed = write(&file, &path("~/dev/noisy"), Edit::Exclude).expect("the write runs");

        assert!(changed);
        let after = std::fs::read_to_string(&file).expect("read it back");
        assert!(after.contains("[[repo]]"), "got {after:?}");
    }

    /// [`today`] is the date half of the core's own RFC 3339 rendering, so the provenance
    /// comment cannot carry a second, differently-computed calendar.
    #[test]
    fn today_is_the_date_half_of_the_cores_own_timestamp_rendering() {
        let today = today();

        assert!(
            repon_core::Timestamp::now().to_string().starts_with(&today),
            "expected today's date to lead the core's own timestamp, got {today:?}"
        );
        assert_eq!(today.len(), "2026-09-01".len());
    }
}
