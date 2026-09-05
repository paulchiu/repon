# Releasing

Repon's release channels are whatever its tag pipeline publishes, and nothing is published by hand; the reasoning is in [0021](../adr/0021-a-release-is-what-the-tag-pipeline-publishes.md). This spec carries what that decision leaves open: the channels and their triggers, the platform claim, the metadata, the CI gates, the publish rehearsal and the checklist the first crates.io release must clear. Every measurement in it was hand-run on 2026-08-30 on macOS with rustc 1.95.0 and cargo 1.95.0.

## A version move is not a release

`Cargo.toml`'s version moves on `main` after a change merges, never on the branch that carried it, with `cargo set-version --workspace --bump <label>`. That command moves both crates and the pinned inter-crate `repon-core` dependency together. Nobody runs it: `version-tag.yml` runs it when a labelled pull request merges, then commits the bump and pushes a `vX.Y.Z` tag, and the tag is what starts a release.

The label on the pull request is what picks the bump, and `check-labels.yml` fails any pull request that carries none of `major`, `minor`, `patch` and `norelease`, or more than one. `norelease` merges and moves nothing.

The number is semantic, with the major held at 0. A change that gives someone something they could not do before moves the minor; a change that fixes, tightens or documents what was already there moves the patch. A breaking change also moves the minor rather than the major, which is what semantic versioning already says a 0.x line means. The major moves to 1 exactly once, when the maintainer says the interface is one they are willing to keep, and never as a side effect of a change being large.

So a new key, a new column behaviour or a new operation is a minor, and a defect fix is a patch however deep it went. Neither is a release either way, since the tag is the release, but the number is the only thing a built binary can say about itself, and a marker calling a new capability a patch tells the person running it the opposite of the truth.

## Where Repon can be installed from

| channel | status | trigger |
| --- | --- | --- |
| `cargo install --git` | live | none; it works against `main` today |
| crates.io | live | a `vX.Y.Z` tag; the four blockers below are cleared |
| prebuilt binaries | live | the same tag; macOS on both architectures and Linux x86_64 |
| Homebrew | live | the same tag, which pushes a formula to `paulchiu/homebrew-tap` |

Every channel but the first is fed by one tag. `v0.29.0` put both crates on crates.io, created the GitHub release the prebuilt archives hang off, and pushed the formula to `paulchiu/homebrew-tap`, so every route below works today. Cutting a tag stays a separate act from opening a channel.

The Homebrew route:

```sh
brew install paulchiu/tap/repon
```

The `cargo install --git` route, which needs no tag and works now:

```sh
cargo install --git https://github.com/paulchiu/repon --locked repon
```

Measured, that costs 44.9 seconds and produces a 1,599,200 byte binary. A cold release build from an empty `CARGO_HOME` is 50.2 seconds wall and 124.8 seconds CPU over 333 packages, needing 175 MB of registry and 350 MB of target, so the channel's real price is a Rust toolchain and about a minute, not disk. `--locked` builds from the committed lockfile, so the binary a user gets is the dependency set CI tested rather than a fresh resolution.

The channel has one measured wart. On a machine whose git config rewrites GitHub HTTPS to SSH (`url.ssh://git@github.com/.insteadOf https://github.com/`), cargo's built-in git transport fails with "no authentication methods succeeded". `CARGO_NET_GIT_FETCH_WITH_CLI=true` fixes it with the rewrite still in place, because the git CLI holds the SSH credentials cargo's own transport cannot reach. The README carries the same advice, because the README is where a failing user is standing when it happens.

The command above builds `repon-core` with its periodic fetch and fast-forward-only auto-update mechanism unconditionally: [refresh.md](refresh.md)'s periodic fetch runs whenever `config.toml` sets `fetch.enabled = true`, with no separate feature flag to discover or ask for.

## Platform support

Repon runs on macOS and Linux, and never on Windows. That is a decision, not a current limitation: [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md) is what closes that window, because [actions.md](actions.md) puts `setsid(2)` in `pre_exec`, opens the capture channel with `openpty(3)` through libc and hands the child `/dev/null` on stdin, and Windows has none of the three.

The guard is a `compile_error!` under `cfg(not(unix))` in `crates/repon-core/src/lib.rs`, so a Windows build meant to reach it fails at once with a sentence rather than later with a missing libc symbol. It belongs to the library rather than the binary because [0015](../adr/0015-the-core-owns-the-table.md) assigns Action fan-out and the child-process environment contract to the core: the crate that owns the Unix-only work is the crate that declares the requirement, and any future consumer of `repon-core` inherits the claim without knowing to ask for it. In practice a Windows `cargo check` today fails earlier, in `aws-lc-sys`'s build script, since the periodic fetch's network stack (pulled in unconditionally, per "Where Repon can be installed from" above) does not cross-compile to Windows; the guard still fires in any configuration that reaches compilation, but that configuration no longer exists for this workspace.

docs.rs would trip over the guard: it builds five default targets and two of them are Windows (`x86_64-pc-windows-msvc`, `i686-pc-windows-msvc`), which would put two failed builds on every release's documentation page. Both crates therefore set `[package.metadata.docs.rs] targets` to one Linux and one Darwin triple (`x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`), the two platforms the support claim actually names.

## The minimum supported Rust version

`rust-version = "1.88"` is declared once in `[workspace.package]` and inherited by both crates. The floor is set by twelve lockfile packages, not by ratatui alone: ratatui 0.30.2 with ratatui-core, ratatui-crossterm, ratatui-widgets, ratatui-termina and ratatui-termwiz; darling 0.24.1 with darling_core and darling_macro; time 0.3.55 with time-core; and instability 0.3.13. 1.88.0 builds and tests the workspace clean, measured, so the declaration is true rather than aspirational.

Declaring the floor changes what the resolver does, and the effect deserves stating precisely. The workspace sets `resolver = "3"`, which makes version resolution MSRV-aware. `cargo update --dry-run` prints "Locking 1 package to latest Rust 1.95.0 compatible version" with nothing declared, against "Locking 1 package to latest Rust 1.88.0 compatible version" once declared; the resolved lockfile is identical today, with `generic-array` 0.14.7 held back either way. The effect is therefore latent. What it buys now is that `cargo update` no longer depends on whose machine runs it: undeclared, the resolver's ceiling is whatever toolchain happens to be installed, so two contributors on different toolchains would resolve different lockfiles from the same manifests.

There is no support-window promise: no "N-2" and no time window. A window is a commitment to recurring work on a schedule, and nothing earns that until a user's constraint (a distro toolchain, a pinned build image) justifies one. Until then the number moves when a wanted dependency requires it to move, and the move is an ordinary versioned change.

Enforcement is `just msrv`, which reads the number out of `cargo metadata` rather than repeating it, installs that toolchain with `--profile minimal` and runs the full workspace test suite under it. Because the recipe reads the manifest, the version Cargo promises and the version CI proves cannot drift apart; the recipe also fails outright when no `rust-version` is declared, so the floor cannot silently stop being proven.

## What CI does

`.github/workflows/ci.yml` is four jobs:

| job | runs | on |
| --- | --- | --- |
| `quality-check` | `just ci` | a matrix of `ubuntu-latest` and `macos-latest` |
| `msrv` | `just msrv` | `ubuntu-latest` |
| `publish-check` | `just publish-check` | `ubuntu-latest` |
| `workflows` | `just workflows` | `ubuntu-latest` |
| `ci` | asserts every job above succeeded | `ubuntu-latest` |

The justfile owns what CI means. Each job's only step of substance is `just <recipe>`, so a green local run is a green pipeline and the definition of passing cannot drift between the two. `just ci` composes `fmt-check lint test docs check-core-isolation build`: rustfmt in check mode, clippy with warnings as errors across all targets, the workspace tests, rustdoc with warnings as errors (which is what catches a broken intra-doc link), the isolation check below, and a debug build, everything `--locked`. All of these passed on the untouched tree before any of this was added, so the gates started green; nothing was relaxed to obtain a first passing run.

There are two OS legs in `quality-check` because standard GitHub runners are free on public repositories, macOS included. macOS is the primary development platform and Linux the other supported one, so both are proven on every push, and the matrix sets `fail-fast: false` so a failure on one still reports the other.

There is no CI job proving the Windows refusal. One existed once, asserting `cargo check --workspace --target x86_64-pc-windows-msvc` failed with the Unix `compile_error!`'s own message, which was true while some configuration of `repon-core` had no network stack in it: `cargo check` never reaches the linker, so the guard fired first and cleanly, before anything else could fail. Once the periodic fetch's network stack became unconditional, `aws-lc-sys`'s C sources fail in a build script before the compiler ever reaches the guard, on every target the workspace resolves for, Windows included. There is no longer a configuration in which the Unix `compile_error!` is the first failure, so a job asserting it fired would pass for the wrong reason, or not at all; a job that passes without proving its claim is worse than no job, so the job was deleted rather than retargeted at a diagnostic that is no longer the guard's own.

`check-core-isolation` is where [0015](../adr/0015-the-core-owns-the-table.md)'s enforcement finally lands. It reads `cargo tree -p repon-core --edges normal --depth 1` and fails unless the direct dependency set is exactly `crossbeam-channel gix rayon`. It is an allowlist rather than the denylist 0015 described, and deliberately so: a denylist bans ratatui and crossterm by name, and a rendering crate nobody thought to ban walks straight past it, while an allowlist makes every new core dependency a deliberate edit to the recipe with the boundary argument in front of whoever makes it.

`publish-check` is a separate job rather than a sixth recipe inside `just ci` for a mechanical reason: `cargo publish` refuses a dirty working tree, and `ci` is the recipe you run locally with work in progress. Folding the rehearsal into `ci` would make the local gate fail on every uncommitted change, so it runs beside `ci` in the pipeline and on demand locally.

`workflows` parses every file under `.github/workflows` and checks that each job whose `uses:` names a local file names one that exists. `release.yml` is generated by dist and is four hundred lines nobody reads, so a hand-written workflow beside it with a typo, or a call pointing at a file that is not there, would otherwise surface as a failed release rather than as a failed check. It is Linux only and outside `just ci` because it needs PyYAML, and `just ci` is the recipe you run on a laptop.

The aggregate `ci` job exists so branch protection names a single required status check. When a job is added, `needs` grows by one word and branch protection needs no editing; without the aggregate, every new job is a settings change in the GitHub UI that the repository's history never records.

## The publish, and why it is one command

The rehearsal, and eventually the release, is one command:

```sh
cargo publish --workspace --dry-run --locked
```

It packages and verifies both crates and exits 0, because `--workspace` resolves `repon-core` from the workspace rather than from the registry. The two failure modes that made this worth writing down, both observed:

- `cargo publish -p repon` fails with "all dependencies must have a version requirement specified when publishing. dependency `repon-core` does not specify a version". A bare path dependency is legal inside a workspace and illegal in a published archive.
- With the version requirement added but the library not yet on the index, the same command fails with "no matching package named `repon-core` found", because the verify step builds the packaged `repon` against the registry, where `repon-core` does not exist yet.

So the workspace form is the only form that rehearses a publish, and it is also what the tag pipeline runs without `--dry-run`: cargo orders the publishes itself, library first, and waits for the index between them.

The packaged artefacts, measured: `repon-core` is 28 files, 1.0 MiB, 254.0 KiB compressed; `repon` is 51 files, 1.5 MiB, 382.7 KiB compressed. Every count here has moved at least twice since the first measurement, because both archives grow with ordinary work: cargo packages `Cargo.lock`, every source file the crate gains ships, and the source itself carries its own tests. Re-measure with `cargo package --workspace --allow-dirty --no-verify` rather than trusting this line; nothing reads it at test time, and a figure in prose goes stale the moment a file is added. Both archives still sit far under crates.io's 10 MB limit, so archive size never enters the release checklist.

## Crate metadata

Every field lives in `[workspace.package]` and is inherited with `field.workspace = true` in both crates, so a future change is an edit in one place.

| field | value | why it is there |
| --- | --- | --- |
| `version` | one shared number, `0.29.0` at the time of writing | [0015](../adr/0015-the-core-owns-the-table.md) already decided lockstep with "no separate versioning"; one number moves both crates. It moves on a labelled merge rather than by hand ("A version move is not a release" above), so read `Cargo.toml` for the current figure rather than this row |
| `rust-version` | `1.88` | the declared floor, per the section above |
| `license` | `MIT` | matches the root `LICENSE` |
| `authors` | `Paul Chiu` | attribution on the registry page |
| `repository` | `https://github.com/paulchiu/repon` | the repository link on crates.io and docs.rs |
| `homepage` | the same URL | crates.io renders it as its own link; absent, the slot is empty rather than defaulted |
| `readme` | `README.md` | the crates.io page body, shipped in both archives |
| `keywords` | `git`, `tui`, `worktree`, `monorepo`, `terminal` | crates.io search; 5 is the cap and all five are spent |
| `categories` | `command-line-utilities`, `development-tools` | crates.io's controlled vocabulary, both slugs valid in it |
| `version` on the `repon-core` path dependency | the same shared number | what the workspace needs to be publishable at all, per the section above. `cargo set-version --workspace` rewrites it with the rest |

Two mechanics were each got wrong once, and measured, so they are recorded:

- `readme` must be inherited with a workspace-root-relative path. `readme = "../../README.md"` in `[workspace.package]` resolves relative to each member, becomes `../../../../README.md` and errors; `readme = "README.md"` resolves from the workspace root and ships the one README in both crates.
- The licence text reaches each archive through a symlink from each crate directory to the root `LICENSE`, which cargo dereferences into a real 1,066 byte file when packaging. Without the symlinks neither crate shipped the MIT text at all, while six of six of Repon's own direct dependencies (ratatui, gix, clap, serde, rayon, crossterm) ship theirs.

## Before the first crates.io publish

crates.io's own rule sets the stakes: "a publish is generally permanent. The version can never be overwritten, and the code cannot be deleted". Whatever the first published version contained is carried forever, so the four items below were blockers rather than preferences, and item 4 recurs before every publish rather than clearing once.

This spec has always carried four items in this gate. [0021](../adr/0021-a-release-is-what-the-tag-pipeline-publishes.md)'s original text, kept in that file's git history, named only the first two, the surface demolition and the config move, because it was written before the tag pipeline and the README's registry-copy review existed as separate, checkable items. This spec is the living checklist, so it carries all four.

1. [0015](../adr/0015-the-core-owns-the-table.md)'s core API has landed. **Done.** `crates/repon-core/src/lib.rs` declares `mod fanout;` and `mod git;` with no `pub`, and no `Box<dyn std::error::Error>` remains anywhere in the crate; the git error is now a closed, cloneable enum. Verified against the tree at commit `c44a8fc`.
2. [0014](../adr/0014-config-is-read-only-and-a-set-bounds-the-work.md)'s config path has moved to `etcetera`. **Done.** `crates/repon/src/config/mod.rs` resolves `config_dir()` from `etcetera::choose_base_strategy`, not `directories::ProjectDirs`. `directories` is still a dependency, but only for `data_dir()`, which 0014's own Consequences section says explicitly stays on `directories`; the concern this item names, config read from the wrong platform path, is closed.
3. The tag pipeline has a crates.io publish job, for the reason [0021](../adr/0021-a-release-is-what-the-tag-pipeline-publishes.md) records: a channel fed by hand is a channel that silently stops being fed. **Done.** `.github/workflows/publish-crates.yml` runs `cargo publish --workspace --locked` and is called by `release.yml` as one of its publish jobs; see below.
4. The README's Influences section has been read as what it is: the crates.io page. `readme` ships in both archives, so every sentence in it becomes front matter on a public registry page rather than a repository aside. **Done**, as of this review: the section names mrx, superfile and lazygit as influences and links each of them. What stays out is what is not the upstream author's to publish: no private-conversation quote, no local path, no description of mrx's internals or history, and no framing of mrx by a relationship to anyone rather than by what it is. This re-read has to happen again before the actual first publish; once here is not enough.

All four blockers were cleared before the first tag, and that tag has since been cut: `v0.29.0` ran `publish-crates.yml` and both crates are on the index. Item 4 is the one that does not stay cleared, because the README ships in every archive of every version.

Done already, proved by CI on every push:

- the `version` on the `repon-core` path dependency, `rust-version`, `readme`, `homepage`, `authors`, `keywords`, `categories` and the symlinked `LICENSE`, all of which `just publish-check` rehearses
- the docs.rs target override in both crates, which nothing local can prove because only docs.rs builds those targets
- the Unix `compile_error!` guard, verified by hand against `x86_64-pc-windows-msvc`: no CI job proves it, per "What CI does" above

## What the tag pipeline does

`.github/workflows/release.yml` is generated by [dist](https://github.com/axodotdev/cargo-dist) 0.32.0 from `dist-workspace.toml` and is never hand-edited; `dist generate` rewrites it from that config. It triggers on a pushed tag matching `**[0-9]+.[0-9]+.[0-9]+*` and runs:

| job | does |
| --- | --- |
| `plan` | works out which artifacts this tag needs |
| `build-local-artifacts` | builds and archives one binary per target, on a runner matching each |
| `build-global-artifacts` | the installers, checksums and the manifest |
| `host` | creates the GitHub release and uploads everything to it |
| `publish-homebrew-formula` | writes the formula into `paulchiu/homebrew-tap` |
| `custom-publish-crates` | calls `.github/workflows/publish-crates.yml` |
| `announce` | marks the release published once every job above has passed or skipped |

The tag comes from `.github/workflows/version-tag.yml`, which fires when a labelled pull request merges to `main`: it reads the `major`, `minor` or `patch` label, runs `cargo set-version --workspace --bump`, commits, and pushes `main` and the tag. A pull request labelled `norelease` merges and nothing runs.

The targets are `aarch64-apple-darwin`, `x86_64-apple-darwin` and `x86_64-unknown-linux-gnu`, the two platforms "Platform support" above claims, with macOS shipping one archive per architecture rather than a universal binary because Homebrew picks the archive matching the machine it installs on.

crates.io is a custom publish job rather than one of dist's own, because dist does not publish to registries. It is inside the pipeline all the same, which is what [0021](../adr/0021-a-release-is-what-the-tag-pipeline-publishes.md) requires and what the owner's blubat is the evidence for: blubat published 0.4.0 of both its crates on 2026-08-02 and nothing since, while shipping GitHub releases through v0.17.2 on 2026-08-21, thirteen releases stale on the one channel that sat outside its pipeline. Adopting dist here does not repeat that: dist's custom-publish-job seam keeps the registry leg inside.

A changelog is produced by the pipeline rather than by hand: GitHub's release notes, built from the commits and merged pull requests since the previous tag.

Three secrets make it run, and adding them is a manual step in the repository settings that no change in this repository can perform:

| secret | read by | is |
| --- | --- | --- |
| `RELEASE_TOKEN` | `version-tag.yml` | a token with push access to this repository. GitHub's recursion guard means a tag pushed with the default `GITHUB_TOKEN` starts no further workflow runs, so a tag pushed that way would never reach `release.yml`. Without it the bump and the tag still land and the release has to be started by hand. |
| `CARGO_REGISTRY_TOKEN` | `publish-crates.yml` | a crates.io API token. Cargo's own conventional name for it. |
| `HOMEBREW_TAP_TOKEN` | `publish-homebrew-formula` | a token with push access to `paulchiu/homebrew-tap`. |

All three are set on `paulchiu/repon`. One fine-grained token covers both `RELEASE_TOKEN` and `HOMEBREW_TAP_TOKEN`, since the two repositories it needs are this one and the tap. That token expires, so its renewal is a diary entry rather than a setting. An expired one takes the release pipeline with it quietly: `version-tag.yml` falls back to `GITHUB_TOKEN`, so the bump and the tag keep landing and only the release stops.

crates.io trusted publishing (OIDC, no stored token) was unverifiable for a crate name that did not exist, so the pipeline uses a stored token and sidesteps the question. Both crates are on the index now, so the condition that deferral named is met and the switch is a change someone can make rather than one waiting on the registry.

`v0.29.0` exercised the pipeline for the first time and ran every job green, opening all four channels in the table above at once; `v0.29.1` repeated it. `just workflows` parses every workflow and checks that each local `uses:` resolves, which is what a change can prove before a tag runs the pipeline against it.

## What is deliberately not here

- **A homebrew-core submission.** The formula goes to `paulchiu/homebrew-tap`. homebrew-core's notability bar is 30 forks, 30 watchers or 75 stars for a third-party submission and 90, 90 or 225 for a self-submission, and a tap has no bar at all.
- **A Windows target.** The library refuses to compile there, per "Platform support" above.
- **release-plz.** Its value is coordinating a release-pull-request review cycle that does not exist with one maintainer.
- **A support-window MSRV promise**, per the MSRV section.
- **Reserving the crate name with a placeholder publish.** `repon` and `repon-core` are claimed by a real publish rather than by a placeholder, and `repo-n` and `repo_n` remain unclaimed. The neighbourhood was occupied when this was written: `reponest` 0.1.0-alpha ("A TUI/CLI tool for managing multiple git repositories written in Rust", published 2025-12-14) and `gitpane` ("Multi-repo Git workspace dashboard TUI", 1,638 downloads, released 2026-08-29) both sit one search away. The risk of losing the name was real and was accepted, because a placeholder publish is itself a publish: permanent, and of nothing.
