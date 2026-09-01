# HEAD

HEAD has three shapes, and which shape it is decides what six of the seven columns can say. This spec fixes the shapes, what each column shows in each, and what a detached row costs to refresh. The reasoning is in [0019](../adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md).

## The three shapes

gix 0.87.1's `head::Kind` has exactly three variants, and Repon takes them one to one.

| shape | gix variant | has a branch name | has a commit |
| --- | --- | --- | --- |
| Attached | `Symbolic` | yes | yes |
| Detached | `Detached { target, peeled }` | no | yes |
| Unborn | `Unborn` | yes | no |

Unborn is the exact mirror of detached: a branch name and no commit, where detached is a commit and no branch name. Measured across the 403-entity population: 125 entities are detached, 2 are unborn, and the rest are attached.

## What each column shows

| column | attached | detached | unborn |
| --- | --- | --- | --- |
| `name` | unchanged | unchanged | unchanged |
| `branch` | the branch name | the short object id | the branch name |
| `sync` | ahead/behind against the upstream | `-` | `-`, or `∅` with no remote |
| `base` | computed | computed | Not applicable |
| `dirty` | normal | normal | normal |
| `state` | the four Worktree states | `Merged` or Not applicable | Not applicable |

`branch` on a detached row is the short object id, with no marker word and no prefix, which is the identical rule [discovery.md](discovery.md) already fixes for a Submodule row. On an unborn row it is the branch name plainly, because the ref exists and only the commit does not.

`sync` is `-` on a detached row because there is no branch to carry an upstream. On an unborn row it is `-`, or `∅` when the Repo has no remote at all, which is what both measured unborn entities show.

`base` is computed in every shape that has a commit, so attached and detached both compute it and unborn is Not applicable. A detached HEAD is a commit, and how far a commit sits behind the default branch needs nothing else. Measured on the 125 detached entities: computable on 125 of 125, median 46 commits behind the default branch, p90 286, max 530, and 5 at zero.

`dirty` is unchanged in all three shapes. 9 of the 125 detached entities are Dirty.

`state` is the load-bearing cell and takes the next section.

All three shapes on one screen, with a Submodule row for the fourth case that reaches the same rendering by a different route:

```
╭ repos ───────────────────────────────────────────────────────────────────────────────────╮
│  name                         branch                   sync      base   dirty  state     │
│  manage                       main                     ≡                ·                │
│    └ manage-cad-1958          feature/cad-1958-phase-0 ↑2        ↓12    ●3     Active    │
│    └ manage-pr-3920           ac7feed53                -         ↓530   ·                │
│    └ manage-pr-3966           7272ad5e9                -         ↓521   ·      Merged    │
│  qmk_firmware                 main                     ≡                ·                │
│    └ lib/chibios              1a2b3c4d5                -                ·                │
│  HelloWorldCLI                main                     ∅                ●1               │
╰──────────────────────────────────────────────────────────────────────────────────────────╯
```

`manage` is attached and on its default branch, so `base` is Not applicable. The two `-pr-` rows are detached: object id, `-`, a real behind count, and `state` blank on one and `Merged` on the other. `lib/chibios` is a Submodule, detached like the other two and blank in `base` for the separate reason below. `HelloWorldCLI` is unborn: a branch name, `∅` for no remote, and no commit to compute anything else from.

## The state cell on a detached row

Merged needs a commit and a default branch, not a branch name, so it stays provable when HEAD is detached. Gone, Local only and Active all need an upstream, and a detached HEAD structurally cannot have one, because an upstream is configured against a branch. The cell therefore carries a positive answer or nothing: `Known(Merged)` when ancestry or patch equivalence proves it, `NotApplicable` when it does not. `Cell<WorktreeState>` already expresses this without change, because `NotApplicable` lives in the `Cell` rather than in `WorktreeState`, so the four states in [GLOSSARY.md](../../GLOSSARY.md) are untouched.

Measured: 2 of the 125 detached entities are ancestors of the default branch, and a further 53 are patch-equivalent to a commit on it, so 55 of 125 read Merged. Across the whole list that takes the `state` column from 42 to 96 of 403 rows carrying a value, and makes 54 of 163 Worktrees read Merged.

The Submodule row is the exception, and it states its own reason: [discovery.md](discovery.md) keeps `state` and `base` Not applicable there, not because a Submodule is detached but because [default-branch.md](default-branch.md) records that population's default branch as known-wrong with no local detector, so a proof computed against it would be a confident lie. The rule is "no trustworthy default branch, so no proof", and it survives this spec unchanged.

## The branch cell

The cost, stated plainly: an object id is a legal git branch name, so the cell holds two categorically different things and the detail pane is the only discriminator. [discovery.md](discovery.md) already accepts that for 16 Submodule rows; it now covers 141 rows. Colour cannot carry the distinction, because [theming.md](theming.md) forbids colour as the only carrier of meaning, so the branch cell takes the ordinary text role in all three shapes. The `head:detached` term ([filter.md](filter.md)) makes those rows reachable without opening the detail pane, on the precedent of the failure term [actions.md](actions.md) added.

The abbreviation is nine characters, fixed, rather than git's own. `core.abbrev auto` scales with object count, measured at 9 characters in one Repo of the population and 7 in another, so leaving it to git gives a ragged column, and nine is what the largest Repo measured already needs. Nothing consumes the id as an input, so uniqueness is not load-bearing; the detail pane carries the full id for anything that is.

## The sync cell

`-` on a detached row, which is what [discovery.md](discovery.md) already draws on all 16 Submodule rows. Record what this settles: [core-api.md](core-api.md) had `Unknown::NoUpstream`, "The branch tracks nothing", rendering blank with `?` in the gutter, while [default-branch.md](default-branch.md) renders the same fact as `-`, a value behind a blank gutter. `-` wins, because it is in the value glyph set [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md) fixes and it is already on screen. `Unknown::NoUpstream` and `Unknown::NoRemote` leave the closed set, which shrinks to `TimedOut` and `NoDefaultBranch`.

## The gutter

A detached row whose `state` is Not applicable folds to Fresh, because Not applicable cells are excluded from the fold, so the gutter renders a space. That is accepted rather than worked around: a Repo row already does exactly this on 240 rows, since its `state` is Not applicable by kind. The gutter summarises provenance, not completeness. Marking these rows `?` instead would put an unknown mark on 125 of 403 rows for a settled fact.

## In-progress operations

gix's `Repository::state()` returns `Option<state::InProgress>` with ten variants covering rebase, interactive rebase, mailbox apply, cherry-pick, revert, merge and bisect. It needs no new cargo feature, and it reads the per-worktree git dir, which is where git writes the markers for a linked Worktree. Measured: ten marker stats across all 403 entities cost 6.55ms, with 0 hits in the measured population.

It reaches the detail pane and nothing else. It is not a state, not a gutter mark, and not a gate on an Action, because [0002](../adr/0002-repon-owns-the-outer-loop-only.md) says report rather than fix, and refusing an Action the user typed is fixing. It earns its 6.55ms because a Worktree stopped mid-rebase and a PR review checkout are both "detached at a sha" and would otherwise render identically.

## The environment

`REPON_BRANCH` is unset on a detached HEAD, by the rule [config.md](config.md) already states: an Unknown or Not applicable value unsets the variable rather than setting it empty. A new `REPON_HEAD` carries the resolved commit id of HEAD in every shape that has one, so a step wanting the commit has it and a step wanting a branch gets nothing rather than something wrong. Without this, `REPON_BRANCH` would be set to an object id on 121 of 163 Worktrees, and `git push -u origin "$REPON_BRANCH"` in a `shell = true` step would push a sha as a branch name.

## Refreshing

Two things the refresh model needs, both measured.

The patch-equivalence pass covers 176 entities, not the 163 [refresh.md](refresh.md) records: 11 Repos, 42 attached Worktrees and 123 detached, measured across the whole population. 163 was the linked-Worktree count standing in for the pass population.

The pass is memoised per git common dir. The 123 detached entities sit in only 14 distinct common dirs, 110 of them in three, and the expensive half of the proof, the default branch's patch-ids, depends only on the common dir and the deepest merge base. Computing it once per common dir takes the pass from 321 seconds to 20.7 seconds serial: 10.67 seconds over 14 shared scans plus 10.04 seconds over 123 per-entity diffs at 82ms each. It is keyed the same way the `origin/HEAD` read already is, per common dir per Generation.

The poll sees a commit on a detached row through `HEAD` itself. [refresh.md](refresh.md)'s evidence for a commit is `index` and `refs/heads/<branch>`, and there is no `refs/heads/<branch>` on a detached row. Measured: on a detached HEAD, `git commit` writes the new object id straight into the per-worktree `HEAD` file, so the poll sees it, and sees it better than on an attached row, where [refresh.md](refresh.md) already warns that a commit does not touch `HEAD` at all.

## The unborn row

Both measured unborn entities show git's `## No commits yet on main`, have a branch name, no commit and no remote. `branch` shows the branch name plainly, because the ref is real. `sync` shows `∅`, because neither has a remote. `base` and `state` are Not applicable, because there is no commit to compare rather than no answer to a question that was asked. `dirty` works normally, and everything in the working tree is untracked.

## Failure

A HEAD that cannot be read at all is `Failed`, not detached and not unborn, and the row is marked `!` per [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md). The three shapes are readings of a readable HEAD; an unreadable one never reaches them, and the detail pane names the error.

## Open

- Reporting "detached with work that exists nowhere else". 43 of the 125 detached entities hold committed work reachable from no ref and not landed, and Dirty sees only 2 of them, so it is a second danger Dirty cannot see. Rejected on cost, a full reachability walk measured at 5.1 seconds serial marginal across the 125 entities against [refresh.md](refresh.md)'s 4.4 seconds for the entire probe, and because Repon does not manage Worktrees, so the fact has no action attached here. Reopenable if Worktree management ever arrives.
