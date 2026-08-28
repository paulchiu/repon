# Repon owns the outer loop only

Repon owns the outer loop: state across many Repos, and acting on many at once. lazygit owns the inner loop: staging, committing, diffing, rebasing. Repon never reimplements the inner loop; it hands off through Launchers instead. This boundary is the product's identity.

## Consequences

- No staging view, no commit editor, no diff viewer, no conflict resolution, ever. Requests for them are answered with a Launcher.
- Mutating git operations are limited to the narrowest safe cases (fast-forward-only auto-update, fetch), which also removes most of the need for a mature mutating git backend (see [0004](0004-gix-over-git2.md)).
- Repon is the tool you use before you open lazygit. Success is measured by how quickly it gets you into the right Repo.
