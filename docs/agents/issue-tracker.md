# Issue tracker: GitHub

Issues and specs for this repo live as GitHub issues on `paulchiu/repon`. Use the `gh`
CLI for all operations; it infers the repo from `git remote -v` when run inside a clone.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`. Use a heredoc for
  multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`.
- **List issues**: `gh issue list --state open --json number,title,body,labels --jq '[.[] | {number, title, labels: [.labels[].name]}]'`, with `--label` and `--state` filters.
- **Comment**: `gh issue comment <number> --body "..."`
- **Label**: `gh issue edit <number> --add-label "..."` / `--remove-label "..."`
- **Close**: `gh issue close <number> --comment "..."`

## Pull requests as a triage surface

**PRs as a request surface: no.** _(Set to `yes` if this repo treats external PRs as
feature requests; `/triage` reads this flag.)_

GitHub shares one number space across issues and PRs, so a bare `#42` may be either:
resolve with `gh pr view 42` and fall back to `gh issue view 42`.

## When a skill says "publish to the issue tracker"

Create a GitHub issue.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> --comments`.

## Wayfinding operations

Used by `/wayfinder`. The **map** is issue #2, labelled `wayfinder:map`; tickets are its
child issues.

- **Child ticket**: linked to the map as a GitHub sub-issue. Labels: `wayfinder:<type>`
  (`research`/`prototype`/`grilling`/`task`), or `ready-for-agent` once specified.
- **Blocking**: GitHub's **native issue dependencies**, which this repo uses, are the
  canonical representation. Add an edge with
  `gh api --method POST repos/paulchiu/repon/issues/<child>/dependencies/blocked_by -F issue_id=<blocker-db-id>`,
  where the blocker's numeric **database id** comes from
  `gh api repos/paulchiu/repon/issues/<n> --jq .id` (not the `#number` or `node_id`).
  Each ticket body also carries a `## Blocked by` section listing the same edges in
  human-readable form; both are kept in step.
- **Frontier query**: an open ticket is unblocked when
  `gh api repos/paulchiu/repon/issues/<n> --jq .issue_dependencies_summary.blocked_by`
  reads `0`, since that field counts open blockers only. Drop any ticket with an
  assignee; first in map order wins.
- **Claim**: `gh issue edit <n> --add-assignee @me`, the session's first write.
- **Resolve**: `gh issue comment <n> --body "<answer>"`, then `gh issue close <n>`, then
  append a context pointer to the map's Decisions-so-far.

## House rule

Never resolve more than one ticket per session, research tickets excepted.
