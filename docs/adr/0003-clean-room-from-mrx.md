# Repon is a full clean-room rebuild

> **Retired.** Withdrawn rather than moved: Repon has diverged far enough that
> derivation is no longer a live question, so the constraint has nothing left to hold.
> The README keeps its credit to mrx, as a credit. `is_forbidden_dir_name` in
> `tools/fanout-sweep`, named below as the nearest thing to a check, went with it.
> Nothing below is maintained.

Repon's problem statement comes from [mrx](https://github.com/benfriebe/mrx), a multi-repo tool covering similar ground, and the README credits it under Influences. Naming and linking a public repository makes no licence claim, so the link is safe. Public visibility is not a licence: mrx carries no licence file, so all rights are reserved, its source is not consulted while building Repon, and every solution here is re-derived from the recorded requirements in [the mrx research](<../research/2026-08-28 mrx history and requirements (clean room).md>). Repon takes the problem and none of the code.

The credit may carry what mrx is and where it lives. It may never carry anything that is not the upstream author's to publish: no quote or paraphrase from a private conversation, no local path, no account of mrx's internals or history, and no framing of mrx by a relationship to anyone rather than by what it is. That bound governs everything this repository publishes, and it binds hardest on the README, which [releasing.md](../spec/releasing.md) ships to crates.io, where a published page cannot be taken back.

**Enforcement:** None. This is a convention, not a constraint. The nearest thing to a check is `is_forbidden_dir_name` in `tools/fanout-sweep/src/main.rs`, which refuses to walk any directory named `mrx` at any depth, case-insensitively; that guard covers one benchmark's filesystem walk, sits in its own workspace outside `just ci`, and says nothing about the licence posture. The pre-publish README re-read recorded in [releasing.md](../spec/releasing.md) is a checklist item performed by a person, not a test.

Earlier revisions of this record, including its amendment history, are in the git history of this file.
