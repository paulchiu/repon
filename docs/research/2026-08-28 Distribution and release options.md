# Distribution and release options

Research scope: how a small, single-maintainer, MIT-licensed Rust TUI gets to users, and what that implies for Repon's release setup. Primary sources only: doc.rust-lang.org, cargo's own docs, crates.io's own API/policies, docs.github.com, Homebrew's own docs.brew.sh and Homebrew/brew repo, cross-rs's own repo, keepachangelog.com, and the actual repos/READMEs/workflow files of the tools and projects named. Every claim is cited to the exact page it came from. Sentences that are my own assessment rather than an observed fact are marked **Opinion:**. Compiled 2026-08-28.

## Installation channels

### `cargo install` from crates.io

`cargo install <crate>` compiles the crate from source and installs the resulting binary into `$CARGO_HOME/bin` (usually `~/.cargo/bin`), resolved through a precedence chain of `--root` flag, `CARGO_INSTALL_ROOT` env var, `install.root` config, `CARGO_HOME` env var, then `$HOME/.cargo`. Only packages with `[[bin]]` or `[[example]]` targets are installable. `--locked` pins to the published `Cargo.lock` for a reproducible build. Source: [doc.rust-lang.org/cargo/commands/cargo-install.html](https://doc.rust-lang.org/cargo/commands/cargo-install.html).

Cost to the user: a working Rust toolchain and compile time. There is no prebuilt-binary path in plain `cargo install`.

Cost to the maintainer, one-off setup: a crates.io account, created by logging in via GitHub ("required for now"), a verified email address, and an API token stored locally via `cargo login`. `Cargo.toml` should declare `license`, `description`, `homepage`, `repository`, `readme` (recommended: `keywords`, `categories`). Source: [doc.rust-lang.org/cargo/reference/publishing.html](https://doc.rust-lang.org/cargo/reference/publishing.html).

Cost to the maintainer, per release: `cargo publish --dry-run` then `cargo publish`, which packages, verifies, and uploads the crate. Publication is free but permanent: "a publish is generally permanent. The version can never be overwritten, and the code cannot be deleted." The `.crate` file has a 10MB size limit. Source: same page.

crates.io's own policies (crates.io/policies) prohibit name squatting ("publishing something without genuine development purpose") and rate-limit new-crate publishing to a burst of 5 followed by 1 every 10 minutes. Crate deletion is restricted to within 72 hours of publish, under 1,000 monthly downloads, no dependents, single owner. Source: [crates.io/policies](https://crates.io/policies).

No licence conflict: crates.io requires a declared `license` field but does not mandate which one.

### Homebrew: formula vs cask, homebrew-core's bar, and a personal tap

Per Homebrew's own docs: a formula "builds from upstream sources"; a cask "installs pre-compiled binaries built and signed by upstream." The guidance is explicit: "Use a formula for open source command-line software and libraries that Homebrew can build from source. Use a cask for native macOS applications and for proprietary or supported binary-only software." Source: [docs.brew.sh/Adding-Software-to-Homebrew](https://docs.brew.sh/Adding-Software-to-Homebrew). For an open-source Rust CLI/TUI, formula is the correct category, not cask.

homebrew-core's acceptance bar is the thing a brand-new project will not clear. Its notability rule: "A new package must demonstrate public interest beyond its author. A GitHub project normally satisfies this requirement by meeting one of these thresholds: at least 30 forks, 30 watchers or 75 stars. at least 90 forks, 90 watchers or 225 stars for a self-submission by the repository owner," and "A code repository less than 30 days old is normally not eligible." Source: [docs.brew.sh/Package-Acceptance-Policy](https://docs.brew.sh/Package-Acceptance-Policy). The self-submission bar (90/90/225) is three times the third-party bar, so a maintainer PRing their own tool is held to a higher standard than someone else nominating it. Other requirements from the formula-specific policy: an open-source licence compatible with the Debian Free Software Guidelines (MIT qualifies), an immutable tagged release rather than a moving branch, and passing homebrew-core's own CI matrix. The doc also warns that "New submissions may be held to a higher standard than existing packages because accepting a package creates an ongoing maintenance commitment." Source: [docs.brew.sh/Acceptable-Formulae](https://docs.brew.sh/Acceptable-Formulae).

A personal tap is the practical alternative until that bar is met. Setup: name the repo `homebrew-<name>` so `brew tap` can use the short form, scaffold it with `brew tap-new $USER/homebrew-tap`, generate a starting formula with `brew create <URL> --tap $USER/homebrew-tap`, and fill in `url`/`sha256`/install steps/a test block. Homebrew's own generated GitHub Actions workflows build bottles (binary packages) automatically, and a merged formula-bump PR is published with `brew pr-pull`. Source: [docs.brew.sh/How-to-Create-and-Maintain-a-Tap](https://docs.brew.sh/How-to-Create-and-Maintain-a-Tap). What it buys the user: `brew install user/repository/tool` works as a single command, same as installing from core, once the tap is trusted (non-core taps require explicit trust by default: `brew trust --formula user/repo/foo`, per [docs.brew.sh/Tap-Trust](https://docs.brew.sh/Tap-Trust)).

No licence conflict: MIT satisfies homebrew-core's DFSG-compatibility rule, and nothing about a personal tap imposes a different requirement.

### Prebuilt binaries via GitHub Releases

GitHub's own docs: "Releases are deployable software iterations you can package and make available for a wider audience to download and use," based on git tags. Up to 1,000 release assets per release, each under 2GiB. Source: [docs.github.com/en/repositories/releasing-projects-on-github/about-releases](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases) and [.../managing-releases-in-a-repository](https://docs.github.com/en/repositories/releasing-projects-on-github/managing-releases-in-a-repository).

A workflow triggers on a tag push via:

```yaml
on:
  push:
    tags:
      - v1.*
```

`tags`/`tags-ignore` and `branches`/`branches-ignore` cannot both be set on the same filter, and defining only one Git-ref kind means the workflow won't run for the other kind's events. Source: [docs.github.com/en/actions/using-workflows/triggering-a-workflow](https://docs.github.com/en/actions/using-workflows/triggering-a-workflow#example-including-branches-and-tags). The first-party way to create the release and attach assets from a workflow step is the `gh` CLI: `gh release create v1.2.3 ./dist/*.tgz --generate-notes`. Source: [cli.github.com/manual/gh_release_create](https://cli.github.com/manual/gh_release_create). The underlying REST endpoint is `POST /repos/{owner}/{repo}/releases`. Source: [docs.github.com/en/rest/releases/releases](https://docs.github.com/en/rest/releases/releases?apiVersion=2022-11-28).

Cost to the maintainer: writing a release workflow that cross-compiles for each target triple, deciding an artifact naming convention, and wiring `gh release create` into the tag-triggered job. Once written, it re-runs automatically on every version tag push, so the ongoing cost is close to zero. What it buys the user: a single binary download, no Rust toolchain needed, and this is also the mechanism cargo-binstall (below) relies on for zero-config detection.

No licence conflict: GitHub Releases and Actions are distribution/CI mechanics, orthogonal to licensing.

### cargo-binstall

cargo-binstall solves compile time and toolchain friction for people who already have `cargo`: "a low-complexity mechanism for installing Rust binaries as an alternative to building from source (via `cargo install`) or manually downloading packages." It fetches crate metadata from crates.io, searches the linked `repository`'s releases for a matching artifact using a default filename-pattern matrix (e.g. `{ name }-{ target }-{ version }{ archive-suffix }`), falls back to the third-party quickinstall artifact host, and finally falls back to `cargo install` from source. Source: [github.com/cargo-bins/cargo-binstall README](https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/README.md) and [SUPPORT.md](https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/SUPPORT.md).

Cost to the maintainer: nothing extra if GitHub Releases artifacts already follow a conventional naming pattern (exactly what cargo-dist produces by default), otherwise a one-time `[package.metadata.binstall]` block in `Cargo.toml` specifying `pkg-url`, `bin-dir`, and `pkg-fmt` templates. What it buys the user: `cargo binstall <name>` fetches a prebuilt binary instead of compiling, but the user still needs `cargo`/`cargo-binstall` installed, so this is a "skip the compile" story rather than a "no toolchain at all" story.

Licence note: cargo-binstall itself is licensed **GPL-3.0-only**, confirmed both via crates.io's own metadata (`license: GPL-3.0-only`, [crates.io/api/v1/crates/cargo-binstall](https://crates.io/api/v1/crates/cargo-binstall)) and GitHub's repo-level licence detection for `cargo-bins/cargo-binstall`. This is a genuine outlier against the otherwise near-universal `MIT OR Apache-2.0` Rust-ecosystem convention. It does not affect Repon's own licensing: cargo-binstall is installed and run by the end user as a separate CLI tool, never linked into or shipped with Repon's binary, so a GPL tool consuming Repon's MIT-licensed release artifacts creates no obligation on Repon.

### cargo-dist ("dist")

cargo-dist automates "Plan, Build, Host, Publish, Announce": `dist init` generates a GitHub Actions release workflow that waits for a version tag, cross-compiles for each configured target, builds installers (shell script, PowerShell script, npm, Homebrew formula, MSI), creates or edits the GitHub Release, uploads artifacts, and pulls release notes from the changelog. Source: [github.com/axodotdev/cargo-dist README](https://raw.githubusercontent.com/axodotdev/cargo-dist/main/README.md) and [axodotdev.github.io/cargo-dist/book/ci/index.html](https://axodotdev.github.io/cargo-dist/book/ci/index.html).

The project renamed from `cargo-dist` to `dist` in October 2024 per its own README, though it still publishes to crates.io as the `cargo-dist` crate and lives in the same `axodotdev/cargo-dist` repo.

Homebrew integration: dist can generate a formula and push it into a tap the maintainer owns, because "the core Homebrew tap does not accept prebuilt binaries from third parties." This requires `installers = ["homebrew"]`, `tap = "<user>/homebrew-tap"` in `dist-workspace.toml`, and a GitHub PAT stored as `HOMEBREW_TAP_TOKEN`; thereafter dist manages the tap repo's contents. Source: [axodotdev.github.io/cargo-dist/book/installers/homebrew.html](https://axodotdev.github.io/cargo-dist/book/installers/homebrew.html). This automates the same personal-tap mechanics described above, rather than replacing them.

Current maintenance status, checked directly against the repo (not a blog post): the repo is not archived, with a push as recent as 2026-08-26 and the latest stable release, v0.32.0, published 2026-05-22. The CHANGELOG documents active development through that date. Its commercial sibling, axo.dev's hosted "Axo Releases" product, was discontinued: the v0.29.0 changelog entry (2025-07-31) states the release "removes support for Axo Releases," while the open-source tool itself kept shipping releases afterward through v0.32.0. The changelog also documents PRs merged in from `astral-sh/cargo-dist` (Astral, makers of uv/ruff, maintain a fork that feeds back upstream), so the contributor base extends beyond the original axo.dev team. Source: `gh api repos/axodotdev/cargo-dist` (live query, 2026-08-28) and [github.com/axodotdev/cargo-dist/blob/main/CHANGELOG.md](https://raw.githubusercontent.com/axodotdev/cargo-dist/main/CHANGELOG.md).

**Opinion:** this is a materially different risk profile from "actively staffed by a funded company." Treat dist as a community-sustained open-source tool with an uncertain long-term commercial backer, not a vendor-supported product, when deciding whether to depend on it.

Cost to the maintainer: install dist, run `dist init` (interactive, generates config and `.github/workflows/release.yml`), commit the generated files. Ongoing cost is close to zero: tag and push, dist does the rest. What it buys the user: everything the GitHub Releases and cargo-binstall sections above provide, generated and kept consistent automatically, plus optional installer scripts, an npm wrapper, and a self-maintained Homebrew tap.

No licence conflict: dist is dual-licensed Apache-2.0/MIT; it only writes CI config and release artifacts, it is not linked into the shipped binary.

## What comparable Rust TUIs actually do

Seven projects were checked against primary sources: their own repos, the actual raw workflow YAML under `.github/workflows/`, their README install sections, their live GitHub Releases pages, and (for Homebrew status) the Homebrew formulae API at `formulae.brew.sh/api/formula/<name>.json`, which reports the owning tap directly. The seven: gitui, bottom, bat, ripgrep, zellij, yazi, and television (a 2024-founded Rust TUI fuzzy-finder by alexpasmantier, chosen as the "recent" entry, 6,211 stars as of this research).

| Project | Release tooling | cargo-dist? | Cross-compilation | Homebrew | MSRV declared | MSRV tested in CI | Changelog |
|---|---|---|---|---|---|---|---|
| gitui | Hand-rolled Makefile + GitHub Actions | No | Manual ARM toolchain download, no `cross` | homebrew-core, auto-bumped from CD | 1.88 | Yes, matrix leg | Hand-written, extracted into release notes |
| bottom | Hand-rolled reusable workflow + `cross` | No | `cross`, pinned commit, custom containers | homebrew-core (an old personal tap is archived) | 1.95.0, marked "not official" | No, CI pins a newer 1.97.1 | Hand-written template + `--generate-notes` |
| bat | Hand-rolled, single CICD.yml | No | `cross`, pinned revision | homebrew-core | 1.88 | Yes, dedicated job deriving the toolchain from `cargo metadata` | Hand-written, CI-enforced per PR |
| ripgrep | Hand-rolled ci.yml + release.yml | No | `cross`, pinned v0.2.5 | homebrew-core | 1.96 | Yes, `pinned` matrix leg | Hand-written prose with tagged bullets |
| zellij | Hand-rolled via a `cargo xtask` crate | No | `cross` via xtask | homebrew-core (no personal tap) | 1.95 | Implicitly, `rust-toolchain.toml` pins the whole pipeline to it | Hand-written CHANGELOG plus separate hand-written release notes |
| yazi | Hand-rolled via `cargo xtask dist` | No | `cross` Docker images plus cross-gcc packages | homebrew-core, built from source tarball | 1.95.0 | No | Hand-written, Keep a Changelog |
| television | Hand-rolled cd.yml + git-cliff | No | `cross` | homebrew-core (an old personal tap is archived) | 1.90 | No | git-cliff generated, backed by a conventional-commits CI gate |

The headline finding: none of the seven use cargo-dist, including television, the only one founded after cargo-dist existed. Every one hand-rolls its release workflow. Six of seven use the `cross` tool in some form, usually pinned to an exact version or commit because, per inline comments in both bat's and ripgrep's workflows, unpinned `cross` releases have broken CI in the past; gitui is the outlier, downloading a prebuilt ARM GNU toolchain tarball directly instead. Six of seven ship through homebrew-core rather than a personal tap; bottom and television both ran a personal tap early on and have since archived it in favour of a core listing, which is a real-world example of the tap-then-graduate path described above.

gitui's repo has moved to `github.com/gitui-org/gitui` (the `extrawurst/gitui` URL now redirects there). Its four workflows split PR checks (`ci.yml`, including an unusual `test-homebrew` job that runs `brew install --build-from-source gitui` in CI to catch formula breakage before it ships) from the tag-triggered release (`cd.yml`), a manual-only Homebrew bump fallback (`brew.yml`), and a nightly cron that uploads to S3 rather than GitHub Releases (`nightly.yml`). Its `cd.yml` auto-bumps the homebrew-core formula on every non-prerelease tag via `mislav/bump-homebrew-formula-action`, and release notes are extracted verbatim from the CHANGELOG's matching section via `ffurrer2/extract-release-notes`, with the same extraction re-run on every PR (`log-test`) so a malformed changelog entry fails before it can reach a release. Source: [github.com/extrawurst/gitui](https://github.com/extrawurst/gitui), workflow files under `.github/workflows/` on the `master` branch.

bottom is the most heavily engineered of the seven: `build_releases.yml` is a reusable workflow (`workflow_call`) consumed by both the tagged-release pipeline and the nightly cron, so the ~24-target build matrix is defined once. It produces `.deb` (via `cargo-deb`), `.rpm` (via `cargo-generate-rpm`, including signature verification), and code-signed Windows binaries (SignPath), and is the only one of the seven with fully automated crates.io publishing: `post_release.yml` runs on `release: released` and calls `cargo publish` using `rust-lang/crates-io-auth-action` for a trusted-publisher token rather than a stored secret. Its declared `rust-version = "1.95.0"` carries an inline comment, "not an official MSRV," and CI does not test it: the pipeline actually runs on 1.97.1, read from a separate `.github/ci/rust_version.txt` file. Source: [github.com/ClementTsang/bottom](https://github.com/ClementTsang/bottom), `.github/workflows/build_releases.yml`, `.github/workflows/post_release.yml`, `Cargo.toml`.

bat runs everything, PR checks and the tag-triggered release, out of a single `CICD.yml`, gated by an `all-jobs` job that checks `jq --exit-status 'all(.result == "success")'` over every other job so branch protection needs only one required check regardless of how the matrix changes. Its MSRV job is the cleanest of the seven: it reads `rust_version` out of `cargo metadata` and installs exactly that toolchain via `dtolnay/rust-toolchain@master`, so the CI pin cannot drift from the manifest the way a hand-copied version number can. `.deb` packaging is hand-assembled from `install -Dm755` and a written `DEBIAN/control` file rather than using `cargo-deb`. A separate `require-changelog-for-PRs.yml` workflow fails any PR that doesn't add a CHANGELOG.md line naming the PR number and submitter, the strictest changelog enforcement of the seven. Source: [github.com/sharkdp/bat](https://github.com/sharkdp/bat), `.github/workflows/CICD.yml`, `.github/workflows/require-changelog-for-PRs.yml`.

ripgrep's `release.yml` is a commonly cited reference implementation, and two of its guards are worth naming directly: its `create-release` job derives the version from the pushed tag and fails the build if that doesn't match the `version =` line in Cargo.toml, and it has no clippy job at all, only fmt, test, docs, and fuzz checks, despite being widely held up as exemplary CI. `cargo publish` is not automated; `RELEASE-CHECKLIST.md` documents it as a manual step with an explicit multi-crate publish order (globset, ignore, cli, matcher, regex, pcre2, searcher, printer, grep, core), and the in-repo Homebrew formula (`pkg/brew/ripgrep-bin.rb`) is hand-edited from a helper script's SHA-256 output rather than generated. Source: [github.com/BurntSushi/ripgrep](https://github.com/BurntSushi/ripgrep), `.github/workflows/release.yml`, `RELEASE-CHECKLIST.md`.

zellij drives its release through an in-tree `xtask` crate rather than raw workflow shell steps: `cargo xtask publish` reads the workspace version, tags, pushes (which is what triggers the release workflow), and then loops through the workspace's crates in dependency order (zellij-utils, zellij-tile-utils, zellij-tile, zellij-client, zellij-server, then the main crate last) calling `cargo publish --locked` on each, prompting interactively on failure. `rust-toolchain.toml` pins the exact MSRV, so every CI job runs on it by construction with no separate MSRV job needed, at the cost of never testing newer toolchains. Its release ships only musl Linux binaries, no glibc build at all, and a full parallel "no-web" asset set for a build without its web-client feature. Source: [github.com/zellij-org/zellij](https://github.com/zellij-org/zellij), `xtask/src/pipelines.rs`, `Cargo.toml`, `rust-toolchain.toml`.

yazi's distribution mechanism is the most unusual of the seven: the crate actually published to crates.io on every release is `yazi-build`, whose build script detects it is being compiled from a registry checkout and, at build time, clones the `shipped` git tag of the yazi repo and builds that instead of its own bundled source. The `yazi-fm`/`yazi-cli` crates that produce the real binaries are published far less often and were three releases behind GitHub at the time of this research. Its musl targets build inside `cross`'s prebuilt Docker images directly (`docker://ghcr.io/cross-rs/<target>:edge`) without going through the `cross` CLI. It also runs a first-party, project-hosted APT repository (`yazi-rs/builds`, Ed25519-signed) fed by `repository_dispatch` events from the release workflow. No CI job tests its declared MSRV. Source: [github.com/sxyazi/yazi](https://github.com/sxyazi/yazi), `yazi-build/build.rs`, `.github/workflows/draft.yml`, `.github/workflows/publish.yml`.

television is the only one of the seven with a fully automated changelog: `cliff.toml` drives git-cliff directly, invoked both to produce the GitHub release body and, in a separate `changelog.yml`, to regenerate the committed CHANGELOG.md via an auto-opened PR on every tag. This is backed by a `conventional-commits` CI gate (`webiny/action-conventional-commits`) that enforces the commit discipline git-cliff depends on, so the automation and the discipline that makes it reliable are the same system rather than a hopeful add-on. It also runs a project-hosted, GPG-signed APT repository via `reprepro`. Its declared `rust-version = "1.90"` is untested in CI; the toolchain actually used is pinned newer, at 1.93, via `rust-toolchain.toml`. Source: [github.com/alexpasmantier/television](https://github.com/alexpasmantier/television), `cliff.toml`, `.github/workflows/cd.yml`, `.github/workflows/changelog.yml`.

Patterns that carry directly into Repon's own decision: `cross`, pinned to an exact version or commit, is the near-universal answer to cross-compilation, not raw `rustup target add` and not cargo-dist's generated matrix. MSRV declarations are frequently decorative: only bat, ripgrep, and gitui actually test the version they declare, and bottom explicitly disclaims its own field as unofficial while running CI on something newer. Hand-written changelogs still dominate over generated ones, five of seven, and the two that enforce discipline around them (bat's PR gate, gitui's per-PR extraction test) are markedly more reliable than the ones that don't.

## CI

## CI

### A minimal but honest GitHub Actions setup

The current syntax for a build matrix (`docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/run-job-variations`, the page formerly titled "workflow-syntax-for-github-actions#jobsjob_idstrategymatrix") supports a cross-product matrix (`strategy.matrix.version` / `.os`) and, more usefully for platform-specific target triples, `strategy.matrix.include` to pair a specific `os` with a specific `target` without generating the full cross-product:

```yaml
strategy:
  matrix:
    include:
      - os: ubuntu-latest
        target: x86_64-unknown-linux-gnu
      - os: macos-latest
        target: aarch64-apple-darwin
```

`strategy.matrix.exclude` removes specific generated combinations, `fail-fast` controls whether one failing matrix job cancels the rest, and `max-parallel` caps concurrency. Source: same page.

A clean, current, small-project reference implementation from the rust-lang org itself is `rust-lang/mdBook`'s CI, `github.com/rust-lang/mdBook/blob/master/.github/workflows/main.yml`. It has separate single-purpose jobs: `rustfmt` (`cargo fmt --check`), `clippy` (`cargo clippy --workspace --all-targets --no-deps -- -D warnings`), a matrix `test` job (`cargo test --workspace --locked --target ${{ matrix.target }}`) that includes a dedicated MSRV row pinned to a specific Rust version with a comment reminding the maintainer to keep it in sync with `Cargo.toml` and the docs, a `docs` job (`cargo doc` with `RUSTDOCFLAGS: -D warnings`), a cross-arch build job, and a final gate job that `needs:` all the others and is the single required status check for branch protection. `rust-lang/rust-clippy`'s own merge-queue workflow (`.github/workflows/clippy_mq.yml`) confirms the same `matrix.include` idiom across `ubuntu-latest`/`windows-latest`/`macos-latest`. Together these substantiate a minimal but honest job list: fmt check, clippy with warnings as errors, test across an OS/target matrix, and an MSRV-pinned row, gated by an aggregate status check.

The de facto standard action for installing a Rust toolchain in CI today is `dtolnay/rust-toolchain`: `uses: dtolnay/rust-toolchain@stable` (or `@nightly`, `@1.89.0`, or an expression like `"stable minus 8 releases"`), after which `cargo` commands run directly rather than through a wrapper action. Source: [github.com/dtolnay/rust-toolchain README](https://raw.githubusercontent.com/dtolnay/rust-toolchain/master/README.md). The older `actions-rs/toolchain` is confirmed unmaintained at the source: its repo carries GitHub's own banner, "This repository was archived by the owner on Oct 13, 2023. It is now read-only."

### Cross-compilation targets realistic for a solo maintainer

GitHub-hosted runner architecture, verified against `docs.github.com/en/actions/reference/runners/github-hosted-runners` and the canonical image list at `github.com/actions/runner-images`:

| Label | OS | Architecture |
|---|---|---|
| `ubuntu-latest` | Linux | x64 |
| `ubuntu-24.04-arm` / `ubuntu-22.04-arm` | Linux | arm64 (opt-in label, not `-latest`) |
| `macos-latest` | macOS | arm64 (Apple Silicon; the free default is now Apple Silicon) |
| `macos-15-intel` / `macos-26-intel` | macOS | Intel/x64 (explicit opt-in label) |
| `windows-latest` | Windows | x64 |
| `windows-11-arm` | Windows | arm64 (opt-in label) |

- `aarch64-apple-darwin`: native on `macos-latest`, since that label now defaults to Apple Silicon. Rust platform-support tier 1 with host tools ([doc.rust-lang.org/rustc/platform-support.html](https://doc.rust-lang.org/rustc/platform-support.html)).
- `x86_64-apple-darwin`: needs the explicit `macos-*-intel` runner label, or can be cross-built from an Apple Silicon `macos-latest` runner with `rustup target add x86_64-apple-darwin`, since both are Darwin/Xcode toolchains on the same OS family and this does not require cross-rs. Tier 2 with host tools.
- `x86_64-unknown-linux-gnu`: native on `ubuntu-latest`, tier 1 with host tools.
- `x86_64-unknown-linux-musl`: tier 2 without host tools. Standard approaches are `cross-rs/cross` (Docker/Podman-based, ships a prebuilt image for this target among 60+ supported) or installing `musl-tools` directly on `ubuntu-latest` and using `rustup target add` plus `cargo build --target` without Docker.
- `x86_64-pc-windows-msvc`: native on `windows-latest`, tier 1 with host tools.
- Windows arm64 (`aarch64-pc-windows-msvc`): tier 1 with host tools per Rust's own platform-support list, but only available via the non-`-latest` `windows-11-arm` label, and cross-rs has no meaningful Windows cross-compilation story (its ~60 supported targets are essentially all `*-linux-*`/`*-android*`/embedded). A reasonable target to skip for v1.

Where cross-compiling gets painful, confirmed from cross-rs's own README: Apple Darwin (and MSVC) Dockerfiles "can be found in cross-toolchains. These include MSVC and Apple Darwin targets, which we cannot ship pre-built images of." cross-rs deliberately does not ship ready images for macOS targets because that would require bundling Apple's proprietary SDK/toolchain. So macOS binaries realistically require an actual macOS runner, not cross-compilation from Linux or Windows. musl, by contrast, is one of the easier cases: cross-rs ships prebuilt images for it, or `musl-tools` can be installed directly. Source: [github.com/cross-rs/cross](https://github.com/cross-rs/cross).

**Opinion:** for a solo maintainer, the realistic v1 target list is `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, and `x86_64-pc-windows-msvc`. That's five native or cheaply-cross-compiled targets across three GitHub-hosted OS runners, no Docker required except optionally for musl. Linux arm64 and Windows arm64 are both real gaps worth deferring; neither has a `-latest` runner, and there's no evidence either is meaningfully cheaper to add than the five above.

### MSRV policy

Cargo's own docs on the `rust-version` field: "an optional key that tells cargo what version of the Rust toolchain you support for your package," a bare version number with no operators or pre-release identifiers. "When your package is compiled on an unsupported toolchain, Cargo will report that as an error to the user" (bypassable with `--ignore-rust-version`), and it also affects `cargo add`'s dependency version selection and the resolver. Source: [doc.rust-lang.org/cargo/reference/manifest.html#the-rust-version-field](https://doc.rust-lang.org/cargo/reference/manifest.html#the-rust-version-field) and [doc.rust-lang.org/cargo/reference/rust-version.html](https://doc.rust-lang.org/cargo/reference/rust-version.html).

Cargo's own support expectations for a declared `rust-version`: functionality must be complete on all supported versions, verified ("A package's functionality is verified on its supported Rust versions, including automated testing"), and patchable. Cargo's own policy framing does not mandate a specific scheme; it lists options and states plainly: "The simplest policy to support is to always use the latest Rust version." It also frames a bump to `rust-version` as "assumed to be a minor incompatibility" under semver, i.e. cheap to change later. Source: same rust-version.html page.

The Rust API Guidelines (`rust-lang.github.io/api-guidelines/necessities.html`, checked directly) contain no MSRV guidance at all; the "Necessities" chapter covers only public-dependency stability and permissive licensing. There is no official ecosystem-wide "N-2" convention from that document; any specific number is a per-crate choice, not an api-guidelines rule.

Concrete examples of stated per-crate MSRV policy, checked against each crate's own README/Cargo.toml: crossbeam states "the minimum supported Rust version is 1.74" and "every time the minimum supported Rust version is increased, a new minor version is released," and "supports stable Rust releases going back at least one year" (a time-based, not release-count-based, window). serde declares `rust-version = "1.56"` for the `serde` crate and a higher MSRV for `serde_derive`. Repon's own already-chosen dependencies show a spread: gix declares `rust-version = "1.85"`, toml declares `rust-version = "1.85"`, crossterm declares `rust-version = "1.85.0"`, ratatui declares `rust-version = "1.88.0"`. Repon's effective MSRV floor today is therefore already set by ratatui at 1.88.0, regardless of what Repon's own `Cargo.toml` declares, unless a dependency has since raised it further.

MSRV enforcement in CI happens at two levels. Tracking-only: most jobs (fmt, clippy, test) run against `dtolnay/rust-toolchain@stable`, which floats forward automatically and never verifies the declared floor. Explicit enforcement: a dedicated matrix row pinned to the exact declared `rust-version`, as mdBook's CI does with an inline comment reminding the maintainer to keep the number in sync with `Cargo.toml` and the docs by hand. Clippy's own `incompatible_msrv` lint is a complementary, lighter check that flags MSRV-incompatible API usage without a separate toolchain install. Source: [rust-lang.github.io/rust-clippy/stable/index.html#incompatible_msrv](https://rust-lang.github.io/rust-clippy/stable/index.html#incompatible_msrv).

**Opinion, not sourced from any official policy:** given Cargo's own "simplest policy is always use latest" framing, an MSRV bump counting only as a minor semver break, and Repon having zero downstream users to break, the sensible policy for v1 is to declare `rust-version` in `Cargo.toml` as whatever stable is current at tag time (effectively pinned upward anyway by ratatui's 1.88.0 today), enforce it with one CI matrix row pinned via `dtolnay/rust-toolchain@<version>` running `cargo build --locked` / `cargo test --locked`, and not publish any longer support-window promise ("N-2," "one year back") until real users exist whose constraints would justify the added CI and maintenance cost.

## Release automation

### Keep a Changelog

The spec ([keepachangelog.com/en/1.1.0/](https://keepachangelog.com/en/1.1.0/)) recommends a `CHANGELOG.md`, entries grouped under fixed headings (`Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`), an `Unreleased` section at the top that gets renamed and dated on each release, ISO 8601 dates, and a stated SemVer policy. Its stated guiding principle is explicit: "Changelogs are for humans, not machines," and the spec disclaims automatic generation from arbitrary git history as unreliable because commit conventions vary too much across projects. Adopting it costs nothing beyond the discipline of writing entries by hand; no tooling is required, and it's not something later automation makes obsolete, since git-cliff and release-plz both produce output in this same structure.

### git-cliff

git-cliff (`github.com/orhun/git-cliff`) is a changelog generator: "git-cliff can generate changelog files from the Git history by utilizing conventional commits as well as regex-powered custom parsers," so Conventional Commits gives the least-configuration path but isn't strictly required. Setup: a `cliff.toml` config, scaffoldable via `git cliff --init keepachangelog` (there's a built-in preset targeting the Keep a Changelog format specifically), run manually or wired into CI via `orhun/git-cliff-action`. Licence: dual `MIT OR Apache-2.0`, confirmed via the repo's `LICENSE-MIT` file and crates.io metadata. Actively maintained, with releases through v2.13.1 (2026-04-26). Source: [github.com/orhun/git-cliff](https://github.com/orhun/git-cliff), [git-cliff.org/docs/](https://git-cliff.org/docs/), [crates.io/api/v1/crates/git-cliff](https://crates.io/api/v1/crates/git-cliff).

### release-plz

release-plz (`github.com/release-plz/release-plz`) automates a release-PR loop: it opens or updates a PR that bumps `Cargo.toml` version(s) and updates the changelog based on conventional commits (or a configurable custom regex if not using strict Conventional Commits), and on merge, a second job publishes to crates.io and creates the git tag. It uses git-cliff internally for changelog generation. Setup: a two-job GitHub Actions workflow (`release-plz-release`, `release-plz-pr`), workflow permissions to create/approve PRs, and a `CARGO_REGISTRY_TOKEN` secret unless using crates.io's trusted-publishing feature. Licence: dual `MIT OR Apache-2.0`. Actively maintained with continuous 2026 release cadence. Source: [release-plz.dev/docs/](https://release-plz.dev/docs/), [release-plz.dev/docs/github/quickstart](https://release-plz.dev/docs/github/quickstart), [release-plz.dev/docs/config](https://release-plz.dev/docs/config).

### cargo-release

cargo-release (`github.com/crate-ci/cargo-release`) "extends `cargo publish` with common release practices like validation, version management, tagging, and pushing," run by hand: `cargo release [level] --execute` performs pre-release validation (clean tree, branch/remote sync), version bumping, `cargo publish`, git tagging, and pushing, with configurable hooks for e.g. calling git-cliff. Unlike release-plz, there's no bot-opened PR or automatic conventional-commit-driven version decision; the maintainer runs the command when they decide to release. Licence: dual `MIT OR Apache-2.0`. Source: [github.com/crate-ci/cargo-release README](https://github.com/crate-ci/cargo-release).

### cargo publish, and cargo-dist's relationship to the above

`cargo publish` (`doc.rust-lang.org/cargo/commands/cargo-publish.html`) is the substrate every tool above wraps: package, verify, upload to the registry, poll the index. Every option above eventually calls this exact command.

cargo-dist's own docs state its scope boundary explicitly: "dist intentionally doesn't handle these steps of cutting a release for you: updating the versions of your packages, writing your release notes, committing the results, tagging your commits, pushing to your repo, publishing to crates.io... All dist cares about is that a tagged commit eventually ends up in your repo." Source: [axodotdev.github.io/cargo-dist/book/workspaces/cargo-release-guide.html](https://axodotdev.github.io/cargo-dist/book/workspaces/cargo-release-guide.html). So cargo-dist composes with cargo-release or release-plz rather than replacing either: an upstream tool does version bump, changelog, commit, tag, push, crates.io publish; dist reacts to the resulting tag and handles only binary distribution.

### Licence compatibility of the tooling

Checked against crates.io's own `license` field and each repo's licence files: git-cliff, release-plz, cargo-release, and cargo-dist are all dual `MIT OR Apache-2.0`. cargo-binstall (covered above under installation channels) is the one outlier at `GPL-3.0-only`. None of this affects Repon's own licensing: all five are CI/dev-tooling, invoked as external processes during release or install, never linked into or shipped as part of Repon's compiled binary. A tool's own licence governs redistribution of that tool's source/binary, not the artifacts it merely processes or helps build.

### What's worth adopting now versus later

**Opinion**, reasoned from the above: adopt Keep a Changelog and plain `cargo publish` now, at zero setup cost. cargo-release is a reasonable next step once the manual publish-and-tag sequence has been done by hand a couple of times and compressing it into one command starts to feel worth it; it needs no CI or secrets. git-cliff standalone is premature: its value scales with commit volume and is largely redundant with what release-plz already wraps. release-plz is premature: its entire value proposition is coordinating a release-PR review cycle, which doesn't exist with a single maintainer, and it is the heaviest of the options to set up (a two-job workflow, PR permissions, a stored secret or trusted-publishing config). cargo-dist is premature for the same reason as prebuilt binaries generally: it solves multi-platform binary distribution for a user base that doesn't exist yet.

## Recommendation

The minimum viable distribution setup to adopt now:

- Publish to crates.io via plain `cargo publish`. This is the lowest-cost channel that exists, works immediately, and every other channel either sits on top of it or is orthogonal to it.
- A GitHub Actions PR workflow with `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`, using `dtolnay/rust-toolchain@stable`, following the shape mdBook's own CI uses.
- Declare `rust-version` in `Cargo.toml` at whatever stable is current at v1 (already effectively pinned by ratatui's 1.88.0 today), and add one CI matrix row that pins to that exact version so drift is caught, without publishing any longer support-window promise.
- A hand-maintained `CHANGELOG.md` following the Keep a Changelog format. No tooling, no dependency, and nothing later contradicts it.
- No Homebrew presence yet. homebrew-core is not reachable at v1 (the notability bar alone rules it out), and a personal tap is worth the setup cost only once there's a user asking for one.
- No prebuilt binaries, no cargo-dist, no cargo-binstall metadata, no release-plz at v1. All four solve problems that only exist once there's a user base wanting convenience beyond `cargo install`.

What to defer, and the trigger to revisit: prebuilt GitHub Releases binaries once someone without a Rust toolchain asks to install Repon, or once the maintainer wants Windows/non-Rust-developer users, built by a hand-rolled tag-triggered workflow using `cross` (pinned to an exact version, not floating) rather than cargo-dist; a personal Homebrew tap once the same request comes from a Homebrew user, with homebrew-core itself deferred until Repon can plausibly clear its notability bar and then pursued directly rather than through a tap, matching how bottom and television both wound down their own personal taps once core accepted them; cargo-release once the manual `cargo publish` sequence has been repeated enough times to be worth compressing; release-plz and cargo-dist only if the project situation changes enough to justify them, and neither is likely to: none of the seven comparable projects surveyed use cargo-dist, including one founded well after it existed, and release-plz's value is coordinating a release-PR review cycle that doesn't exist with one maintainer.

The single decision most likely to be regretted is not a channel-ordering choice, it's declaring an MSRV in `Cargo.toml` without a CI job that actually enforces it. Four of the seven comparable projects do exactly this: bottom explicitly disclaims its own declared field as "not an official MSRV" while running CI on something newer, and yazi and television declare a `rust-version` that no workflow ever builds against. An untested MSRV isn't neutral, it's a promise a downstream user or packager can reasonably rely on that quietly stops being true the first time a dependency bump or a convenience feature is added without anyone checking the floor. bat's approach is the one worth copying exactly: derive the MSRV toolchain for that CI job from `cargo metadata`'s own `rust_version` field rather than hand-copying the number into the workflow, so the two can never drift apart. The cost of getting this right is one extra CI job; the cost of getting it wrong is a stated compatibility promise that was never actually true.
