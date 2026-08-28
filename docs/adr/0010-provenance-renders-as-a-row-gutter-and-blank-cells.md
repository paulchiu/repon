# Provenance renders as a row gutter and blank cells

A throwaway ratatui prototype with 37 fake entities and fake arrival timings compared three structurally different renderings of per-cell provenance ([0001](0001-per-cell-provenance.md)): a glyph in the value's own cell for every state, which makes a table of hundreds of rows noisy; a trailing column reporting freshness as relative time; and a one-character gutter at the row start carrying the row's least-settled state, with any cell that has no value left blank. The gutter won.

Blankness is safe here because of a contract: a blank cell means "there is no value here, and the gutter says why". That contract is what lets the table stay quiet without lying, and it holds only while no provenance mark shares a glyph with a real value. The prototype's first draft rendered Unknown as `·` and a clean Worktree as `·`, which is the exact defect per-cell provenance exists to prevent, arriving through the glyph set rather than through the type.

## Consequences

- A future reader will see a blank cell, read it as missing information, and want to fix it by putting glyphs back in the cells. That is the rejected per-cell variant, and it was rejected for noise at hundreds of rows.
- The glyph sets are disjoint by rule. Real values use `≡` for in sync, `·` for clean, `-` for no upstream, `↑n` and `↓n` for ahead and behind, and `●n` for a count of changed files; the gutter uses a space for fresh, `~` for stale, `?` for unknown, a braille spinner for loading (per row, never one global spinner) and `!` for failed.
- Not applicable is a sixth case the five provenance states do not cover. Worktree state has no meaning for a Repo, so that cell renders blank and is excluded from the row summary; otherwise every Repo row reads as unresolved.
- Unknown is reserved for the settled answer "we asked and got nothing back". A row whose probe has not started yet is Loading, because nothing has been asked, so no answer can be missing.
- The gutter collapses four cells into one summary, so a row with a known branch and an unreadable status is labelled unresolved as a whole. Blankness locates which cell is missing, the gutter names why, and the detail pane always reports provenance per cell, which is the escape hatch. Accepted with open eyes.
