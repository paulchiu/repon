# Discovery

Discovery decides what exists before anything is probed. This spec fixes the walk, how Submodules are reached, and what never becomes a row. The reasoning is in [0017](../adr/0017-discovery-stops-at-the-repo-boundary.md).

## The walk

A boundary is a directory containing a `.git` entry, whether that entry is a file or a directory. On reaching one, the directory is recorded as an entity and the walk does not descend into it. Discovery starts from each of the active Set's `roots`, and that is the whole strategy: there is no `max_depth`, no denylist and no deep mode.

Measured warm, median of three, in Rust, over `~/dev` and `~/dev-misc` together:

| walk | time | entries touched | entities |
| --- | --- | --- | --- |
| boundary-stop | 20.6ms | 14,386 | 403 |
| deep, honouring `.gitignore` | 20,850ms | 857,691 | 418 |
| deep, skipping eleven directory names (`node_modules`, `target`, `vendor`, `Pods`, `.venv` and others) | 10,673ms | 645,808 | 440 |
| deep, no exclusions | 89,141ms | 6,424,758 | 441 |

The table is the argument. Every deep variant costs seconds where boundary-stop costs milliseconds: the cheapest of them is still 518 times the boundary-stop walk, and honouring `.gitignore` is 1,012 times it. The extra entities the deep walks find are 17 Submodules, which the next section reaches for 3.92ms without walking at all, and 21 checkouts a tool put inside a working tree, which Repon should not show anyway. So the deep walk pays seconds for something cheaper to read and something not wanted.

A repository deliberately nested inside another repository's working tree is not reached by the walk, because the walk stopped at the outer boundary. It is reached by adding its parent directory as a Set `root`. That is the only escape hatch, and it is deliberate: the boundary rule stays absolute, and the config names the exception rather than the walk guessing at one.

The boundary-stop walk finds all 163 linked Worktrees, because a Worktree sits beside its parent rather than inside it, so no boundary hides it. The walk finds exactly 163 `.git` files: the file form of the entry is, in this population, precisely the linked Worktrees.

## Symlinks

A directory symlink is followed only when its target is itself a Repo, and then only to record that Repo, never to walk through it. Because the walk never descends through a symlink, cycles are impossible rather than guarded against: there is no visited set, because nothing can be reached twice by descent.

Measured, the boundary-stop walk sees 69 directory symlinks. 66 sit below a repo boundary (typically a `node_modules` shared between a Repo and its Worktrees) and are never reached at all. Three sit above any boundary: two point at Repos already discovered under their real names (Obsidian plugin links to `obsidian-sample-plugin` and `obsidian-fitkit` in `~/dev-misc`), and one points at the user's home directory from inside a mise state fixture in a test fixture directory. The home directory is not a Repo, so it is not followed; following it would walk `$HOME`, which [config.md](config.md) records as the case that had not finished after 100 seconds having touched 1.45 million entries. There are no broken symlinks in the measured population.

The cost, stated plainly: a symlink pointing at a directory *of* Repos is not followed, because the target is not itself a Repo. That case is a root.

## Identity

The canonical path is the entity. A symlink resolving to an already-discovered Repo is silently dropped: not a Vanished row, not a warning, because nothing is wrong. Two paths reached one Repo, and one of them is its name. [config.md](config.md) records zero name collisions across all 403 entities and keys the Selection by name in the state file, which two paths to one Repo would break with two live instances of the same name.

## Reaching Submodules

Discovery is two halves returning one entity list: walk to the boundaries, then read `.gitmodules` in what was found.

The pass runs over every discovered entity, Repos and Worktrees alike, because a linked Worktree has its own working tree and therefore its own `.gitmodules`, which can pin a different commit than its parent's. Skipping Worktrees would report the parent's answer for a checkout that may disagree with it.

Measured: statting `.gitmodules` in all 403 entities costs 2.82ms; parsing the four that exist costs 1.10ms; 3.92ms in total. In the population that yields 16 entries across `vial-qmk` (8), `qmk_firmware` (6), `nex` (1) and `nex-fork` (1), all 16 initialised.

The pass does not recurse. One level from each discovered entity, no further. `qmk_firmware/lib/chibios-contrib/ext/mcux-sdk` is a Submodule of a Submodule and stays invisible, and that is the accepted cost: unbounded recursion is how a 3.92ms pass becomes a walk again.

An implementation note on gix. `Repository::modules()` falls back from the worktree `.gitmodules` file to loading the whole index, and then to peeling HEAD to a tree, so calling `submodules()` on a Repo with no `.gitmodules` is not cheap; the population holds 14.6 MB of index files across 238 entities, none of which needs reading to learn a file is absent. Repon stats the file itself and calls gix only for the entities that have one. gix 0.87.1 needs no new cargo feature: `Submodule` is gated on `attributes`, which the already-enabled `status` feature turns on transitively through `blob-diff`. One quirk worth recording: gix treats a `.gitmodules` that is a symlink as absent.

## What is not a Submodule

`.gitmodules` is the sole authority. A gitlink in the index with no `.gitmodules` entry is not a Submodule, which is git's own position: `fitkit-pocs` holds such a gitlink (mode 160000) and `git submodule status` there returns empty. Reading index gitlinks across the population instead was measured at 13,610ms and would find 17 gitlinks in 5 repos against 16 `.gitmodules` entries in 4: slower by orders of magnitude, and wrong by git's own definition of what a Submodule is.

## What is never a row

Two families, one reason each.

**A vendored or embedded checkout.** Never a row, and this needs no rule of its own, because boundary-stopping produces it: the outer boundary is reached first and the walk never sees what sits inside. The measured inventory: of the 38 entities boundary-stopping misses, 17 are Submodules and one is the bare gitlink above; the remaining 20 are 19 independent clones under `manage/.claude/worktrees/` (every one a `.git` directory, all created on the same day, all on `main`, all clean, all matched by the owner's global gitignore rule for `.claude/`) and one transitive CocoaPods checkout at `crew-frontend/ios/Pods/libvmaf/vmaf` (a `.git` file reading `gitdir: ../.git/modules/vmaf`, matched by that project's `ios/.gitignore`). Every one is something a tool put inside a working tree, ignored by the project that contains it.

**A bare repository.** No working tree, so five of the seven columns would be permanently Not applicable, and boundary-stopping cannot find one anyway, because there is no `.git` entry to stop at. Six exist under the roots, all fixture remotes in a test fixture directory. The case that matters beyond fixtures is the `clone --bare` worktree hub, and its Worktrees are all found already; only the hub itself, which has nothing to show, is absent.

## Showing Submodules

`show_submodules` narrows the view rather than bounding the work, which is the opposite of how a Set behaves ([config.md](config.md): a Set bounds the work and an excluded entity is never discovered, where a Filter only narrows what is visible). The pass always runs, so Submodules are always known, and the flag decides three things: whether they are rows, whether they are probed, and whether the two-second metadata sweep in [refresh.md](refresh.md) touches them.

Four consequences fall out. Toggling is instant, because nothing needs discovering. The poll's cost stays proportional to the screen, since the sweep never touches a hidden Submodule's gitdir. `repon sets` can report how many Submodules a Set covers while they are hidden. And a fault found by the pass is reported whether or not Submodules are shown, because the pass ran either way.

A Set's `include` and `exclude` globs are tested against a Submodule's own absolute path. A Submodule arrives by the pass rather than the walk, and without this rule it would slip a bound the Set is supposed to hold: a parent excluded by glob would still surface its Submodules.

Once shown, a Submodule is a full citizen: selectable, counted in the Selection, a valid Launcher and Action target, with `REPON_KIND=submodule` already reserved in [config.md](config.md). One asymmetry falls out for free: `a`, select every visible row, is bounded by what is visible, so hidden Submodules are never silently in a Selection.

## The Submodule row

The payload, stated honestly: all 16 initialised Submodules in the measured population are at a detached HEAD, and none has an attached branch. A Submodule row is mostly blank, and correctly so.

Detachment is not what blanks those cells. [head.md](head.md) records a detached Worktree computing both `base` and Merged, because each needs a commit and a default branch rather than a branch name. A Submodule's stay blank for a different reason: [default-branch.md](default-branch.md) records that population's default branch as known-wrong with no local detector, so a proof against it would be a confident lie.

| column | on a Submodule row |
| --- | --- |
| `name` | the submodule path |
| `branch` | the short object id; the detail pane says detached ([head.md](head.md)) |
| `sync` | `-`, no upstream, for all 16 |
| `base` | Unknown |
| `dirty` | normal |
| `state` | Unknown |

`state` and `base` are `Unknown`, carrying one reason between them rather than two: [0012](../adr/0012-the-default-branch-is-a-remote-tracking-ref.md) already records this exact population as its ceiling, Submodules cached as `master` where the truth is `qmk-master`, with no local detector, so a proof against it would be a confident lie. "No trustworthy default branch, so no proof" is a question that applies and has no answer Repon can stand behind, which is `Unknown`; Not applicable is reserved for a question with no meaning on the row at all, the way Worktree state has none on a Repo row ([0009](../adr/0009-worktree-state-model.md)). `state` and `base` therefore move together rather than settling on different variants for the one reason ([ADR 0017](../adr/0017-discovery-stops-at-the-repo-boundary.md)'s "Amended by #173"). `Unknown` renders blank exactly like Not applicable does, so the row still reads mostly blank, but it folds into the row summary rather than being excluded from it: with the periodic fetch off, the default, this puts `?` in the gutter where a Not applicable pair would have left it a plain space, a cost accepted for the 16 of 403 entities hidden by default.

There is no Submodule row marker of its own. A Submodule is a child row, indented under its parent and marked `└`, which is the mark a Worktree row already carries. The `∙` (U+2219) marker previously drawn in [layout-and-provenance.md](layout-and-provenance.md) is dropped: it sits one codepoint from `·` (U+00B7), the clean-dirty value, on the same row, and a distinction that fine is a misreading waiting to happen. The cost is that a Submodule row and a Worktree row look alike in the gutter and the indent, so the name column, the gutter mark and the detail pane are what tell them apart.

An uninitialised Submodule is a row with every cell blank and `?` in the gutter. All 16 in the population are initialised, so this is the case that arrives right after a plain clone. The alternative, no row at all, makes rows appear on `git submodule update --init` with nothing having changed in what the Repo declares.

Probing cost is paid only when Submodules are shown. `vial-qmk` tracks 29,016 files and its 8 Submodules track 11,249 more between them, so revealing them makes the population's heaviest entity about 40% heavier. This is additive rather than double work, because a parent's index holds one gitlink per Submodule rather than the Submodule's files, so nothing is counted twice. [refresh.md](refresh.md) already records the slowest ten entities carrying 23.5% of the expensive probe phase; showing Submodules concentrates that load further.

## Failure

A `.gitmodules` that will not parse or will not read marks the parent Repo's gutter `!`, Failed rather than Unknown, because we asked and got something we could not use, which is a different fact from getting nothing back. No Submodule rows appear for that Repo, its own cells are untouched, and the detail pane names the failure. The mark appears whether or not `show_submodules` is on, because the pass ran either way.

This gives `RowSummary` its first input that is not a cell, amending [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md): the fold is over the cells and the entity's own derivations. The honest consequence, worth stating: a row can now show `!` with no blank cell, so the detail pane is the only place that names which derivation failed. The default branch's rung and its disagreement stay out of the fold, because those are metadata about how a value was obtained rather than values that can fail on their own; [default-branch.md](default-branch.md) calls them the diagnostics.

## Cadence and bounds

Discovery, both halves, re-runs at the start of every Generation and is never held, and never on the thread that asked for that Generation ([refresh.md](refresh.md)'s "Discovery is never on the calling thread"). At 20.6ms there is nothing to save, and nothing cheap can validate a cache. A direct probe of APFS shows a directory's mtime does not propagate upward (creating `a/b/c/newrepo` bumps `a/b/c` and leaves `a` and `a/b` untouched) while an ordinary file write inside a working tree bumps it constantly, so validating an mtime-keyed cache means statting every directory, which is the walk. This puts discovery on the uncached side of [0006](../adr/0006-no-git-state-cache-session-state-by-name.md)'s line for a reason unrelated to git state going stale: the cache cannot be validated for less than the cost of not having it.

The one-second warning and the thirty-second abandon specified in [config.md](config.md) are unchanged, and now only ever fire on a misconfigured root, since no supported strategy is slow by construction.

A Submodule that appears or disappears between Generations goes Vanished by exactly the rule a Repo does, because discovery returns one entity list and the Vanished comparison never asks which half produced an entry.

## Open

- Whether the pass should recurse one further level. Reopenable if a nested Submodule ever matters; `mcux-sdk` is the only measured instance.
- Whether a Submodule should compute Merged once its default branch can be trusted. The proof itself needs no branch ([head.md](head.md)); the input is what is missing, and only the network closes it.
