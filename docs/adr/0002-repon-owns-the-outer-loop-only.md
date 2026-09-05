# Repon owns the outer loop only

Repon owns the outer loop: state across many Repos, and acting on many at once. lazygit owns the inner loop: staging, committing, diffing, rebasing. Repon never reimplements the inner loop; it hands off through Launchers instead. This boundary is the product's identity, so there is no staging view, no commit editor, no diff viewer and no conflict resolution, ever, and a request for one is answered with a Launcher.

Mutating git operations are limited to the narrowest safe cases, fast-forward-only auto-update and fetch, which also removes most of the need for a mature mutating git backend (see [0004](0004-gix-over-git2.md)). The bound governs what Repon decides to do unbidden, never what the user asks for behind a confirm gate: an Action runs arbitrary commands across N Repos because someone typed them and confirmed them. Crossing the bound would put a half-built second git client inside a tool whose whole pitch is getting you into the right Repo quickly.

**Enforcement:** `no_push_commit_merge_rebase_or_reset_operation_exists_in_production_code` in `crates/repon/src/test_support.rs` scans every workspace crate's production source and fails on a push, commit, merge, rebase or reset reaching gix. `the_fast_forward_only_auto_update_actually_exists`, beside it, finds `fn fast_forward(` in `repon-core`, so the absence claim is not vacuous against a crate that simply never built the auto-update.

Earlier revisions of this record, including its amendment history, are in the git history of this file.
