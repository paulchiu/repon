# Config is a file Repon only reads, and a Set bounds the work

Config is `~/.config/repon/config.toml` with a `themes/` directory beside it; state and the log live at the platform data directory. Repon does not rewrite what the person wrote. There is no config watcher and no first-run materialisation of a default file: `repon config --example` prints the annotated example to stdout and the redirection is the user's. [0028](0028-repon-writes-the-repo-entries-it-owns.md) lets Repon write `[[repo]]` entries, and that is the whole of what it writes; every comment and every key outside the entry being edited survives the write byte for byte. Reload is an explicit keystroke over the file the user edited elsewhere.

When the file is wrong, the grade depends on the kind of wrong. Malformed TOML, or a bad value in a known key, exits non-zero before the terminal is claimed, because a wholly defaulted Repon is not a degraded view of the same program: with no roots it scans the working directory, which can be a home directory and an unbounded walk. An unknown key warns and Repon continues.

A Set bounds the work; it does not filter a view. A Set is roots plus optional include and exclude globs, and an entity a Set excludes is never discovered and never probed. That separates it from a Filter, which narrows what is on screen and never changes what is computed.

**Enforcement:** `malformed_config_named_by_the_flag_fails_to_parse` and `a_bad_value_in_a_known_key_fails_to_parse_and_reports_its_position`, in `crates/repon/tests/config_flag.rs`, run the real binary and prove the exit lands before the terminal is claimed. `every_comment_in_the_commented_fixture_survives_every_write` and `a_write_reformats_nothing_outside_the_repo_entry_it_edits`, in `crates/repon/src/config/repo_entry.rs`, hold the read-only bound around the one write. `an_include_glob_bounds_what_is_discovered` and `an_exclude_glob_beats_an_include_glob`, in `crates/repon-core/src/discovery.rs`, hold the Set's bound on the work.

Earlier revisions of this record, including its amendment history, are in the git history of this file.
