# Releasing

Repon's release channels are whatever its tag pipeline publishes, and nothing is published by hand; the reasoning is in [0021](../adr/0021-a-release-is-what-the-tag-pipeline-publishes.md). This spec carries what that decision leaves open: the channels and their triggers, the platform claim, the metadata, the CI gates, the publish rehearsal and the checklist the first crates.io release must clear. Every measurement in it was hand-run on 2026-08-30 on macOS with rustc 1.95.0 and cargo 1.95.0.

## Where Repon can be installed from

| channel | status | trigger |
| --- | --- | --- |
| `cargo install --git` | live | none; it works against `main` today |
| crates.io | at beta | the four blockers below cleared, and the tag pipeline carrying a publish job |
| prebuilt binaries | deferred | beta; a binary is worth shipping when there is a user without a Rust toolchain to hand it to |
| Homebrew | deferred | the repository clearing homebrew-core's age and notability bars, at which point the path is a personal tap |

The one live channel, verbatim:

```sh
cargo install --git https://github.com/paulchiu/repon --locked repon
```

Measured, that costs 44.9 seconds and produces a 1,599,200 byte binary. A cold release build from an empty `CARGO_HOME` is 50.2 seconds wall and 124.8 seconds CPU over 333 packages, needing 175 MB of registry and 350 MB of target, so the channel's real price is a Rust toolchain and about a minute, not disk. `--locked` builds from the committed lockfile, so the binary a user gets is the dependency set CI tested rather than a fresh resolution.

The channel has one measured wart. On a machine whose git config rewrites GitHub HTTPS to SSH (`url.ssh://git@github.com/.insteadOf https://github.com/`), cargo's built-in git transport fails with "no authentication methods succeeded". `CARGO_NET_GIT_FETCH_WITH_CLI=true` fixes it with the rewrite still in place, because the git CLI holds the SSH credentials cargo's own transport cannot reach. The README carries the same advice, because the README is where a failing user is standing when it happens.

## Platform support

Repon runs on macOS and Linux, and never on Windows. That is a decision, not a current limitation: the workspace compiles clean for `x86_64-pc-windows-msvc` today (a 27.2 second `cargo check`), and [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md) is what closes that window, because [actions.md](actions.md) puts `setsid(2)` in `pre_exec`, opens the capture channel with `openpty(3)` through libc and hands the child `/dev/null` on stdin, and Windows has none of the three.

The guard is a `compile_error!` under `cfg(not(unix))` in `crates/repon-core/src/lib.rs`, so a Windows build fails at once with a sentence rather than later with a missing libc symbol. It belongs to the library rather than the binary because [0015](../adr/0015-the-core-owns-the-table.md) assigns Action fan-out and the child-process environment contract to the core: the crate that owns the Unix-only work is the crate that declares the requirement, and any future consumer of `repon-core` inherits the claim without knowing to ask for it.

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
| `ci` | asserts every job above succeeded | `ubuntu-latest` |

The justfile owns what CI means. Each job's only step of substance is `just <recipe>`, so a green local run is a green pipeline and the definition of passing cannot drift between the two. `just ci` composes `fmt-check lint test docs check-core-isolation build`: rustfmt in check mode, clippy with warnings as errors across all targets, the workspace tests, rustdoc with warnings as errors (which is what catches a broken intra-doc link), the isolation check below, and a debug build, everything `--locked`. All of these passed on the untouched tree before any of this was added, so the gates started green; nothing was relaxed to obtain a first passing run.

There are two OS legs because standard GitHub runners are free on public repositories, macOS included. macOS is the primary development platform and Linux the other supported one, so both are proven on every push, and the matrix sets `fail-fast: false` so a failure on one still reports the other. There is no Windows leg to add, per the section above.

`check-core-isolation` is where [0015](../adr/0015-the-core-owns-the-table.md)'s enforcement finally lands. It reads `cargo tree -p repon-core --edges normal --depth 1` and fails unless the direct dependency set is exactly `crossbeam-channel gix rayon`. It is an allowlist rather than the denylist 0015 described, and deliberately so: a denylist bans ratatui and crossterm by name, and a rendering crate nobody thought to ban walks straight past it, while an allowlist makes every new core dependency a deliberate edit to the recipe with the boundary argument in front of whoever makes it.

`publish-check` is a separate job rather than a sixth recipe inside `just ci` for a mechanical reason: `cargo publish` refuses a dirty working tree, and `ci` is the recipe you run locally with work in progress. Folding the rehearsal into `ci` would make the local gate fail on every uncommitted change, so it runs beside `ci` in the pipeline and on demand locally.

The aggregate `ci` job exists so branch protection names a single required status check. When a job is added, `needs` grows by one word and branch protection needs no editing; without the aggregate, every new job is a settings change in the GitHub UI that the repository's history never records.

## The publish, and why it is one command

The rehearsal, and eventually the release, is one command:

```sh
cargo publish --workspace --dry-run --locked
```

It packages and verifies both crates and exits 0, because `--workspace` resolves `repon-core` from the workspace rather than from the registry. The two failure modes that made this worth writing down, both observed:

- `cargo publish -p repon` fails with "all dependencies must have a version requirement specified when publishing. dependency `repon-core` does not specify a version". A bare path dependency is legal inside a workspace and illegal in a published archive.
- With the version requirement added but the library not yet on the index, the same command fails with "no matching package named `repon-core` found", because the verify step builds the packaged `repon` against the registry, where `repon-core` does not exist yet.

So the workspace form is the only form that rehearses before the first publish, and it is also what the tag pipeline will run without `--dry-run`: cargo orders the publishes itself, library first, and waits for the index between them.

The packaged artefacts, measured: `repon-core` is 8 files, 48.3 KiB, 14.9 KiB compressed; `repon` is 15 files, 108.8 KiB, 31.2 KiB compressed. Both sit far under crates.io's 10 MB limit, so archive size never enters the release checklist.

## Crate metadata

Every field lives in `[workspace.package]` and is inherited with `field.workspace = true` in both crates, so a future change is an edit in one place.

| field | value | why it is there |
| --- | --- | --- |
| `version` | `0.1.0`, shared | [0015](../adr/0015-the-core-owns-the-table.md) already decided lockstep with "no separate versioning"; one number moves both crates |
| `rust-version` | `1.88` | the declared floor, per the section above |
| `license` | `MIT` | matches the root `LICENSE` |
| `authors` | `Paul Chiu` | attribution on the registry page |
| `repository` | `https://github.com/paulchiu/repon` | the repository link on crates.io and docs.rs |
| `homepage` | the same URL | crates.io renders it as its own link; absent, the slot is empty rather than defaulted |
| `readme` | `README.md` | the crates.io page body, shipped in both archives |
| `keywords` | `git`, `tui`, `worktree`, `monorepo`, `terminal` | crates.io search; 5 is the cap and all five are spent |
| `categories` | `command-line-utilities`, `development-tools` | crates.io's controlled vocabulary, both slugs valid in it |
| `version` on the `repon-core` path dependency | `0.1.0` | what makes the workspace publishable at all, per the section above |

Two mechanics were each got wrong once, and measured, so they are recorded:

- `readme` must be inherited with a workspace-root-relative path. `readme = "../../README.md"` in `[workspace.package]` resolves relative to each member, becomes `../../../../README.md` and errors; `readme = "README.md"` resolves from the workspace root and ships the one README in both crates.
- The licence text reaches each archive through a symlink from each crate directory to the root `LICENSE`, which cargo dereferences into a real 1,066 byte file when packaging. Without the symlinks neither crate shipped the MIT text at all, while six of six of Repon's own direct dependencies (ratatui, gix, clap, serde, rayon, crossterm) ship theirs.

## Before the first crates.io publish

The channel is at beta, and the gap between now and then is not polish. crates.io's own rule sets the stakes: "a publish is generally permanent. The version can never be overwritten, and the code cannot be deleted". Whatever 0.1.0 contains is carried forever, which is what makes the first four items blockers rather than preferences.

Blocking:

1. [0015](../adr/0015-the-core-owns-the-table.md)'s core API has landed. Publishing today would pin a surface 0015 has already scheduled for demolition: 0015 says "`fanout` and `git` become private modules" and that the `Box<dyn std::error::Error + Send + Sync>` placeholder "is not `Clone` and cannot survive", while `crates/repon-core/src/lib.rs` itself says "What this crate exposes is not settled". A permanent publish of an unsettled surface is the worst trade on offer.
2. [0014](../adr/0014-config-is-read-only-and-a-set-bounds-the-work.md)'s config path has moved to `etcetera`. `crates/repon/Cargo.toml` still carries `directories = "6.0.0"`, so a published 0.1.0 would read config from `~/Library/Application Support/repon/`, the location 0014 argues is wrong ([config.md](config.md) settles the right one), and moving it after a release breaks a user-visible path with no migration story.
3. The tag pipeline has a crates.io publish job, for the reason [0021](../adr/0021-a-release-is-what-the-tag-pipeline-publishes.md) records: a channel fed by hand is a channel that silently stops being fed.
4. The README's Influences section has been read as what it is: the crates.io page. `readme` ships in both archives, so every sentence in it becomes front matter on a public registry page rather than a repository aside.

Done already:

- the `version` on the `repon-core` path dependency, `rust-version`, `readme`, `homepage`, `authors`, `keywords`, `categories` and the symlinked `LICENSE`, all of which `just publish-check` rehearses on every push
- the docs.rs target override in both crates, which nothing local can prove because only docs.rs builds those targets
- the Unix `compile_error!` guard, verified by hand against `x86_64-pc-windows-msvc`, which fails with the guard's own sentence and no other diagnostic; CI never builds that target, so nothing proves this on a push either
- CI itself: every recipe passes locally, and the workflow has not yet run on GitHub because this is the commit that adds it

## What the tag pipeline must do

The pipeline is not designed here; it is the next ticket, and this section is that ticket's brief. The requirements it has to meet:

- Every channel Repon claims is published by it. That is [0021](../adr/0021-a-release-is-what-the-tag-pipeline-publishes.md)'s rule restated as an acceptance criterion.
- A crates.io publish job is mandatory even if the pipeline is built on tooling that ships everything else. cargo-dist states its own scope boundary as not handling "publishing to crates.io", and the consequence is observable in the wild: the owner's blubat published 0.4.0 of both its crates on 2026-08-02 and nothing since, while shipping GitHub releases through v0.17.2 on 2026-08-21. A pipeline without the job produces exactly that drift.
- A version bump moves both crates and the pinned dependency together. `cargo set-version --workspace` does all three in one command, including the `version` on the `repon-core` path dependency.
- A tag pushed with the default `GITHUB_TOKEN` starts no further workflows, by GitHub's own recursion guard, so a release triggered by a tag needs a token that can: a PAT, a deploy key or a GitHub App token, chosen in that ticket.

Two questions the ticket must answer rather than inherit: whether the pipeline is cargo-dist or hand-rolled, and whether crates.io trusted publishing can be configured for a crate name that does not yet exist, which is unverified.

## What is deliberately not here

- **Prebuilt binaries.** The gzipped release binary is 774,513 bytes, so the asset is cheap; what is missing is anyone to hand it to. The trigger is beta.
- **Homebrew.** The repository is two days old with 0 stars, 0 forks and 0 watchers. homebrew-core's rule that "a code repository less than 30 days old is normally not eligible" rules it out before its notability bar (30 forks, 30 watchers or 75 stars for a third-party submission; 90, 90 or 225 for a self-submission) is even reached. When the trigger fires, the path is a personal tap, not a core submission.
- **A changelog.** Deferred to the tag pipeline ticket, since what generates it is that ticket's question.
- **release-plz.** Choosing release automation before the pipeline is designed would be the pipeline decision made by default.
- **A support-window MSRV promise**, per the MSRV section.
- **Reserving the crate name with a placeholder publish.** `repon`, `repon-core`, `repo-n` and `repo_n` are all unclaimed, and the neighbourhood is occupied: `reponest` 0.1.0-alpha ("A TUI/CLI tool for managing multiple git repositories written in Rust", published 2025-12-14) and `gitpane` ("Multi-repo Git workspace dashboard TUI", 1,638 downloads, released 2026-08-29) both sit one search away. The risk of losing the name is real and accepted, because a placeholder publish is itself a publish: permanent, and of nothing.
