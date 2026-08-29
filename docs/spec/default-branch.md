# The default branch

Two things are computed against a Repo's default branch: the `Merged` Worktree state, and the behind-the-default count in the `base` column. Both lie quietly if the default branch is wrong, which is why resolution is a chain with a recorded outcome rather than a guess. The reasoning is in [0012](../adr/0012-the-default-branch-is-a-remote-tracking-ref.md).

## What resolves

A **remote-tracking ref**, always. Never `refs/heads/<name>`. The result of resolution is a ref name such as `refs/remotes/origin/main`, plus the rung of the chain that produced it.

## The chain

Tried in order, first answer wins.

| rung | source | offline |
| --- | --- | --- |
| 1 | An explicit per-Repo override from config | yes |
| 2 | `refs/remotes/<remote>/HEAD`, symbolic target validated | yes |
| 3 | `<remote>/main`, then `<remote>/master`, then `<remote>/trunk` | yes |
| 4 | Unknown | |

Rung 1's shape is settled in [the config spec](config.md): a `[[repo]]` entry with a `path` and a `default_branch`, matched by git common dir so one entry covers a Repo and all of its linked Worktrees, and no Set-level default. That keys rung 1 the same way the rest of this chain is already memoised.

Not in the chain, and deliberately: `init.defaultBranch`, which records what the machine that ran `git init` was configured with and says nothing about a clone; `remote.origin.HEAD`, which is not a git config key and appeared in 0 of 409 measured entities; and local branch names, which resolved one measured Repo incorrectly because its only local branch was `master` while its default was `main`.

## Choosing the remote

gix's `remote_default_name(Direction::Fetch)`, unmodified:

- `origin` when a remote of that name exists.
- The sole remote when there is exactly one, whatever it is called.
- Nothing when there are two or more and none is called `origin`.

The refusal to guess in the last case is kept rather than patched. A Repo with `origin` and `upstream` resolves against `origin`, because a fork's branches merge into the fork's own default, not upstream's.

## Reading `origin/HEAD`

Three properties govern the read.

**It lives in the git common dir, not the checkout.** A linked Worktree has no `refs/remotes` of its own. Reading `<repo>/.git/refs/remotes/origin/HEAD` therefore fails on 163 of 409 measured entities. Resolve through the common dir: either `--git-common-dir`, or parse the `gitdir:` line in the `.git` file and follow that gitdir's `commondir`.

**A symbolic ref is never packed**, so the loose file is the only place to look when the ref is symbolic. A non-symbolic `origin/HEAD` is a real if unusual case (some fetch paths write one) and it *can* be packed, so a lookup that misses loose must fall through to the normal path rather than concluding absence.

**Validate the target.** `git symbolic-ref` and gix's `Reference::target()` both return a stale target with exit 0 or `Ok`, because neither checks that the branch it names still exists. If the target ref does not resolve, treat rung 2 as absent and fall through to rung 3, recording that this is what happened. Without the check, `Merged` is computed against a ref that resolves to nothing, which surfaces as a git error and marks the row `!`, a lie about what actually went wrong.

In gix: `try_find_reference("refs/remotes/<remote>/HEAD")`, then match `target()` on `TargetRef::Symbolic`. `shorten()` yields `origin/main` rather than `main`, so the remote prefix is stripped by the caller. `TargetRef::Object` is the non-symbolic case and is not an error.

## Cadence

Re-resolved on every refresh, never persisted. Persisting would sit against [0006](../adr/0006-no-git-state-cache-session-state-by-name.md), and there is nothing to gain: reading the ref in-process across the whole measured population costs about 20ms.

Memoised per common dir within a single refresh generation. 409 entities collapse to 246 distinct reads, with zero measured disagreement between checkouts sharing a parent, since there is one file.

Shelling out to the git binary for this costs about 4 seconds across the same population, of which roughly 99% is git's own startup rather than the ref lookup. The read is done in-process.

## Disagreement

Rung 3 runs even when rung 2 answers, and a disagreement between them is recorded on the Repo.

This is not redundancy. Validation catches a dangling `origin/HEAD`, and the measured population has none. The failure that actually occurs is stale-but-resolvable: the ref exists, resolves cleanly, and is out of date, and no local inspection can detect it. A sweep of 220 common dirs against their remotes found 7 such Repos; on the one visible Repo among them, rung 3 disagreed with rung 2 and was right.

The disagreement surfaces in the detail pane, not in the list. `origin/HEAD` still wins. It lives on the entity beside the cells rather than in one, along with the rung that answered and the reason resolution stopped, because none of the three is a value with its own provenance; [the core API spec](core-api.md) calls that field the diagnostics.

The ceiling is stated rather than engineered around: on the six hidden Submodules in that sweep, both rungs agreed and both were wrong, because the true default was `qmk-master`. No local signal exists for that case, and only the network closes it.

## The network

Repon renders fully offline. Branch, status and both ahead/behind computations read local refs; remote-tracking refs are a local cache that fetch populates. The network probe never sits on the render path: 3 of 220 measured remotes were unreachable, and a round trip per Repo against a 20ms local budget would turn an instant screen into a slow one.

It is coupled to fetch instead. A fetch handshake already advertises HEAD, so the remote's answer arrives inside a round trip already being paid for and supersedes the local one for that session. In gix this is `Remote::connect(Direction::Fetch)` then `ref_map` with:

- `extra_refspecs: ["HEAD"]`, because the default `prefix_from_spec_as_filter_on_remote: true` derives `ref-prefix refs/heads/` from the standard refspec and the server then never advertises `HEAD` at all. Setting the flag to `false` also works but drops the server-side filter, so the extra refspec is preferred.
- `.with_credentials(|_| Ok(None))`, so a missing credential fails closed instead of prompting on a terminal Repon has taken the alternate screen of.
- `handshake::Ref::Symbolic { full_ref_name: "HEAD", target, .. }` carries the answer. `Ref::Unborn` covers an empty remote and is not an error.

The transport needs `blocking-network-client` plus an HTTP transport feature, which pulls in a dependency set well beyond the read-only surface the core was scaffolded with. It is isolated behind a cargo feature.

A user-triggered re-derive over the Selection uses the same path without fetching. Its key belongs to [Decide the keybinding map](https://github.com/paulchiu/repon/issues/12).

The answer is never written back to `refs/remotes/<remote>/HEAD`. That is a mutation, and gix has no `set-head` equivalent in any case; it would have to be a hand-built `RefEdit`.

## Failure

When the chain reaches rung 4, the default branch is **Unknown**, and every value derived from it is Unknown: the `Merged` classification and the `base` count. Not Failed. No default branch determinable is a settled answer, and Failed stays reserved for a git error such as an unreadable checkout.

The gutter shows `?` per [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md). The detail pane names the rung that was reached and why it stopped. There is no second row-level warning channel; it would compete with the gutter for the same square of screen.

A default branch that came from rung 3 is marked in the detail pane and nowhere in the list. In the measured population that rung fires only on the 17 Repos with no remote at all, which show `∅`, have no upstream, and have no ancestry to be misleading about. It reaches the screen only in the narrow-clone case (`git clone --single-branch --branch <non-default>`), which produces a Repo whose `origin/HEAD` is absent and never self-heals on later fetches.

## Merged

`Merged` is "the branch's work has landed in the default branch", by either of two proofs. This amends [0009](../adr/0009-worktree-state-model.md), which defined it by ancestry alone.

**Ancestry** is checked first and wins. `merge-base --is-ancestor <branch> <default>`, about 20ms with a commit-graph present and 10 to 15 times that without one. Exit 1 means not an ancestor; any other non-zero is an error and must not be read as "not merged", or every broken Repo renders as `Active`.

**Patch equivalence** runs only where ancestry says no, as a second pass. It catches the squash merge, which ancestry structurally cannot: a squash commit leaves the common ancestor unchanged, so the branch never becomes an ancestor. Without it, `Merged` is dead on any Repo that squash-merges and every landed branch arrives as `Gone`.

Two constraints:

- **It must not write to the object database.** The widely-copied recipe builds a dangling commit with `git commit-tree` and asks `git cherry` about it, which writes a loose object per probe. The core does not write. The patch-id comes from an in-memory diff of the merge base against the branch tip instead.
- **It is neither sound nor complete.** Patch-id ignores whitespace and line numbers, so a whitespace-only difference can read as equivalent, and a conflict resolved during the squash can read as not equivalent. Tree comparison is worse: it holds only until the default branch advances once. This is a proof of "very probably landed", and it is why `Dirty` remains an orthogonal veto on anything destructive.

`Gone` is what remains: the upstream vanished and neither proof showed the work landed. That is the case worth looking at before sweeping, and it is a sharper state than it was, because the squash merges have left it.

`Gone` requires a prune. A plain fetch never produces it. If Repon fetches it must prune; if it does not fetch, `Gone` is under-reported, which is input to [Decide the refresh model](https://github.com/paulchiu/repon/issues/7).

## Two passes on screen

Ancestry resolves inside the cheap pass. Patch equivalence costs roughly 130ms per branch, and the load falls almost entirely on Worktree branches, since a Repo row is usually on the default branch itself and settles free on an object-id equality check. That is about 163 branches in the measured population: 21 seconds serial, under 2 seconds parallel.

The state cell stays **blank and Loading** for any branch where ancestry says no, until the second pass answers. It never shows `Gone` and then flips to `Merged`. A cell that changes value under the reader is a screen contradicting itself, which is the defect [0001](../adr/0001-per-cell-provenance.md) exists to prevent, and the blank-cell contract already covers "no value here yet, the gutter says why".

## The two behind counts

They are different measurements and both are shown.

| column | compares | answers |
| --- | --- | --- |
| `sync` | branch against its upstream | someone pushed to my branch, pull before you push |
| `base` | branch against the default branch | trunk moved under me, rebase |

They coincide only when a branch's upstream *is* the default branch, which is the default branch's own row.

`base` is a new column, 6 wide, sitting after `sync`. It reuses `↓n` for behind and `≡` for level, with the `behind` role from [theming.md](theming.md). There is no ahead-of-default: that only says the branch has commits of its own, which is not an integration signal.

`base` and `sync` are separate provenance cells. They fail independently, since a Repo can have a resolvable upstream and an Unknown default branch.

`base` is **not applicable**, rendering blank and excluded from the row summary, in two cases: on a row whose branch is itself the default branch, where it would duplicate `sync`; and on any Repo with no remote, where it would otherwise hold all 17 such Repos permanently at `?` over a fact that is settled rather than missing. Not applicable is the sixth case already named in [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md), the same rule that blanks Worktree state on a Repo row.

## Glyphs

One addition to the value set: `∅` in the `sync` cell means the Repo has **no remote at all**, distinct from `-`, which stays "this branch has no upstream". Different facts: `-` means you could push and have not, `∅` means there is nowhere to push. It appears on the Repo row and on all of its Worktree rows, since none of them can have an upstream.

`∅` is disjoint from both the value set (`≡`, `·`, `-`, `↑n`, `↓n`, `●n`) and the gutter set (space, `~`, `?`, spinner, `!`), which is the rule [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md) holds. It is an ambiguous-width character, as `≡`, `·`, `↑` and `↓` already are, and the `glyphs = "ascii"` switch in [theming.md](theming.md) covers terminals that cannot draw it.

## Column widths

Name 28, branch 24, sync 9, base 6, dirty 6, state 10, then the filler column. With the gutter and single-space gaps this is 90 columns, up from 83. Below that the existing rule already gives the frame to the detail pane.
