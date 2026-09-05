# No git-state cache; session state persists by name

Computed git state is never persisted to disk, while session state (the Selection, the active Filter, the active Set, the row order and the worktrees toggle) is. The distinction: session state is the user's input, so it can only be absent, never stale; git state is the world, which goes stale and lies, and a stale cache would undermine the provenance guarantees of [0001](0001-per-cell-provenance.md). Every launch therefore recomputes git state from scratch, which means first-frame performance has to come from progressive loading and can never be bought with a cache.

Selection restores by name, never by index, so a row discovered earlier this run cannot shift what a stored index points at; an unknown name is dropped silently. A corrupt state file behaves exactly like no file, and an explicit flag always beats stored state.

**Enforcement:** `the_written_file_holds_only_selection_filter_sort_and_show_worktrees_nothing_else` in `crates/repon/src/state.rs` fails if anything beyond session state reaches `state.toml`, and `malformed_toml_loads_the_same_as_a_missing_file` beside it holds the corruption rule. `restore_by_name_selects_the_matching_entities_and_drops_an_unknown_name_silently` and `restore_by_name_survives_an_index_shift_from_a_row_discovered_earlier_this_run`, in `crates/repon/src/selection.rs`, hold restore-by-name.

Earlier revisions of this record, including its amendment history, are in the git history of this file.
