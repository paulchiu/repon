# justfile for repon - a terminal UI for the outer loop across many git repos

# Show available recipes
default:
    @just --list

# Build the workspace in debug mode
build:
    cargo build --workspace --locked

# Build the workspace in release mode
release:
    cargo build --workspace --release --locked

# Run all tests
test:
    cargo test --workspace --locked
    # The run above uses default features, so it never reaches a single test behind the
    # `fetch` feature: the periodic fetch and the fast-forward-only update both live there,
    # and without this second run their suites are written and then executed on nobody's
    # machine but the author's.
    cargo test -p repon-core --locked --features fetch
    # `repon`'s own built-in `sync` action is eligible only on a build with the `fetch`
    # feature (0031), so its own crate needs the identical second run: without this, the
    # `Eligible` half of that decision is untested on anyone's machine but the author's,
    # the same gap the run above closes for `repon-core`.
    cargo test -p repon --locked --features fetch --bin repon

# Format code with rustfmt
fmt:
    cargo fmt --all

# Fail if code is not formatted
fmt-check:
    cargo fmt --all --check

# Lint with clippy, warnings are errors
lint:
    cargo clippy --workspace --all-targets --locked --no-deps -- -D warnings
    # The run above uses default features, so the `fetch` module is only ever compiled by
    # `just test`'s own fetch pass, as a test target, where a field the production path
    # ignores still reads as live. Without this, dead code behind the feature reaches a
    # user's `cargo install --features fetch` as a warning CI never saw.
    cargo clippy --workspace --all-targets --locked --no-deps --features fetch -- -D warnings

# Fail on a broken intra-doc link or a malformed doc comment
docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

# Assert the core crate stays usable by a consumer that owns no terminal
#
# An allowlist rather than a denylist: every dependency the core takes on has to
# be added here deliberately, so a terminal or rendering crate cannot reach it by
# being one nobody thought to ban. Each entry below pairs a crate with the reason
# it is allowed, and the comparison set is derived from that pairing, so a crate
# cannot be added here without recording why.
#
# Checked twice, once per state of the `serde` feature: off (the default a
# published consumer builds against) and on (what `repon` itself turns on for the
# settled document's wire format, ADR 0015). `cargo tree -p repon-core` alone
# only ever resolves the default feature set, so an allowlist that skipped the
# "on" pass would never see `serde` join the tree at all, and one that only ever
# ran with the feature on would let it (or anything else the feature happens to
# pull in as a direct dependency) into the default build unnoticed.
# The terminal and rendering crates never belong here.
check-core-isolation:
    #!/usr/bin/env bash
    set -euo pipefail

    check_isolation() {
        local label="$1"
        local features="$2"
        shift 2
        local -a allowed_with_reasons=("$@")

        local allowed_names=()
        for entry in "${allowed_with_reasons[@]}"; do
            name="${entry%%:*}"
            reason="${entry#*:}"
            if [ -z "$name" ] || [ "$name" = "$entry" ] || [ -z "$reason" ]; then
                echo "malformed allowlist entry: '$entry' (expected 'crate:reason')" >&2
                exit 1
            fi
            allowed_names+=("$name")
        done
        local allowed
        allowed=$(printf '%s\n' "${allowed_names[@]}" | sort -u | tr '\n' ' ' | sed 's/ $//')
        local actual
        # `$features` is passed unquoted and deliberately left out of `--features` entirely
        # when empty, rather than kept in a bash array: an empty array expanded under `set -u`
        # is a portability trap on bash 3.2 (macOS's own `/bin/bash`), which reads it as an
        # unbound variable rather than as nothing.
        if [ -n "$features" ]; then
            actual=$(cargo tree -p repon-core --edges normal --depth 1 --prefix none --features "$features" \
                | awk 'NR > 1 && NF { print $1 }' | sort -u | tr '\n' ' ' | sed 's/ $//')
        else
            actual=$(cargo tree -p repon-core --edges normal --depth 1 --prefix none \
                | awk 'NR > 1 && NF { print $1 }' | sort -u | tr '\n' ' ' | sed 's/ $//')
        fi
        if [ "$actual" != "$allowed" ]; then
            echo "repon-core's direct dependencies changed ($label)" >&2
            echo "allowed, with reasons:" >&2
            for entry in "${allowed_with_reasons[@]}"; do
                echo "  ${entry%%:*}: ${entry#*:}" >&2
            done
            echo "actual: $actual" >&2
            echo "an undeclared crate was added or removed; add it above with its reason, or remove it" >&2
            exit 1
        fi
        echo "repon-core ($label) depends on nothing beyond: $allowed"
    }

    base_allowed_with_reasons=(
        "crossbeam-channel:the fan-out result channel between rayon workers and the core"
        "globset:Set include/exclude glob matching in the discovery walk"
        "gix:the git backend this crate wraps"
        "libc:setsid(2) and openpty(3) for an Action step's PTY-backed child, per ADR 0018"
        "rayon:the probe phases' worker pool"
    )

    check_isolation "serde feature off, the default build" "" "${base_allowed_with_reasons[@]}"
    check_isolation "serde feature on, the settled document's wire format" serde \
        "${base_allowed_with_reasons[@]}" \
        "serde:the settled document's wire format on stdout, off by default (ADR 0015)"
    # The periodic fetch's blocking network client, HTTP transport and credential
    # machinery resolve inside gix's own already-allowed subtree, so they never
    # surface as a direct dependency and the allowed set here is identical to the
    # base build. That identity is not itself evidence of anything: a depth-1 check
    # cannot see them either way, which is what `check_network_stack_is_gated`
    # below exists to cover (ADR 0015's "The read-only invariant is scoped to the
    # probe path").
    check_isolation "fetch feature on, the periodic fetch" fetch "${base_allowed_with_reasons[@]}"

    # The claim the depth-1 check above is structurally blind to: the mutating
    # path's network stack is absent from the default build and present only under
    # the feature. Read over the whole transitive tree, since that is the depth the
    # crates actually live at.
    #
    # Checked against both `repon-core` alone and the `repon` binary a user actually
    # builds, because `repon-core` passing this in isolation already did once while
    # `repon`'s own manifest requested `repon-core/fetch` unconditionally, putting the
    # network stack (and aws-lc-sys's C sources, which fail to cross-compile to
    # Windows) in every ordinary `cargo build`. `repon` now carries its own opt-in
    # `fetch` feature forwarding to `repon-core/fetch` (`Cargo.toml`'s own comment on
    # the dependency line), the one path `cargo install --features fetch` reaches, so
    # its "on" case is asserted here too: without it, `repon`'s own `[features]`
    # entry could be spelled wrong, forward to nothing, or forward to the wrong
    # feature name, and this check would still pass.
    check_network_stack_is_gated() {
        local -a network_crates=(reqwest rustls hyper-rustls tokio-rustls)

        local core_default_tree core_fetch_tree repon_default_tree repon_fetch_tree
        core_default_tree=$(cargo tree -p repon-core --edges normal --prefix none)
        core_fetch_tree=$(cargo tree -p repon-core --edges normal --prefix none --features fetch)
        repon_default_tree=$(cargo tree -p repon --edges normal --prefix none)
        repon_fetch_tree=$(cargo tree -p repon --edges normal --prefix none --features fetch)

        for name in "${network_crates[@]}"; do
            if grep -qE "^${name} v" <<<"$core_default_tree"; then
                echo "$name is in repon-core's default build; the mutating fetch path's" >&2
                echo "network stack must reach the tree only under the fetch feature" >&2
                exit 1
            fi
            if grep -qE "^${name} v" <<<"$repon_default_tree"; then
                echo "$name is in repon's own default build; the binary a user actually" >&2
                echo "builds must not pull the fetch path's network stack in either" >&2
                exit 1
            fi
            if ! grep -qE "^${name} v" <<<"$core_fetch_tree"; then
                echo "$name is absent even with the fetch feature on; this check is naming a" >&2
                echo "crate the fetch path no longer pulls, so it proves nothing as written" >&2
                exit 1
            fi
            if ! grep -qE "^${name} v" <<<"$repon_fetch_tree"; then
                echo "$name is absent from 'cargo build -p repon --features fetch'; repon's" >&2
                echo "own fetch feature must forward to repon-core/fetch, the one path" >&2
                echo "'cargo install --features fetch' actually reaches" >&2
                exit 1
            fi
        done
        echo "repon-core's and repon's own default builds pull none of: ${network_crates[*]}, and both crates' fetch feature pulls all of them"
    }

    check_network_stack_is_gated

# Build and test against the declared floor
#
# The number is read out of the manifest rather than written here, so the version
# Cargo promises and the version this proves cannot drift apart.
msrv:
    #!/usr/bin/env bash
    set -euo pipefail
    version=$(cargo metadata --no-deps --format-version 1 \
        | jq -r '.packages[] | select(.name == "repon") | .rust_version')
    if [ -z "$version" ] || [ "$version" = "null" ]; then
        echo "no rust-version in the manifest, so there is no floor to prove" >&2
        exit 1
    fi
    rustup toolchain install "$version" --profile minimal
    cargo "+$version" test --workspace --locked
    cargo "+$version" test -p repon-core --locked --features fetch

# The single definition of CI: the GitHub workflow runs this recipe, so a green
# run here is a green pipeline
ci: fmt-check lint test docs check-core-isolation build

# Rehearse the release: package and verify both crates without uploading
#
# `--workspace` resolves repon-core from the workspace rather than the registry,
# which is what makes the binary's publish rehearsable before the library exists
# on crates.io. Kept out of `ci` because it refuses a dirty tree, and `ci` is the
# recipe you run with work in progress.
#
# Its own target directory, because the packaged crates are built with
# `-L dependency=<target>/debug/deps`: sharing that directory with the workspace
# build lets the packaged binary resolve the workspace's own repon-core rather
# than the packaged one, and the rehearsal stops rehearsing what it claims to.
#
# The same class of leak sits one level further out, in a directory this recipe
# does not own at all. To verify `repon` against the just-packaged `repon-core`,
# cargo routes it through a local registry and extracts it into
# `~/.cargo/registry/src/<hash>/repon-core-<version>/`, then never re-extracts
# into a directory that already exists. The hash comes from this checkout's local
# registry path, so it is the same on every run: left alone, a second run
# verifies `repon` against whatever `repon-core` source the first run left there,
# not the one just packaged, and can pass silently against a stale-but-compatible
# copy. So this recipe clears any such extraction of a workspace crate before
# rehearsing (only a crate another workspace crate depends on is ever routed this
# way; `repon` itself, with no in-workspace consumer, builds straight from its own
# package directory and is never at risk), then diffs what the rehearsal actually
# extracted against the source on disk, so a rehearsal that verifies a stale copy
# again fails loudly instead of quietly passing.
publish-check:
    #!/usr/bin/env bash
    set -euo pipefail
    export CARGO_TARGET_DIR=target/publish-check

    metadata=$(cargo metadata --no-deps --format-version 1)
    registry_src="${CARGO_HOME:-$HOME/.cargo}/registry/src"

    # A workspace crate is only ever extracted into the local registry above if some
    # other workspace crate names it as a path dependency; derived here rather than
    # named, so a crate added later is covered without editing this recipe.
    at_risk=$(jq -r '[.packages[].dependencies[] | select(.path != null) | .name] | unique[]' <<< "$metadata")

    package_field() {
        jq -r --arg name "$1" --arg field "$2" '.packages[] | select(.name == $name) | .[$field]' <<< "$metadata"
    }
    extraction_dirs() {
        local name="$1" version="$2"
        find "$registry_src" -mindepth 2 -maxdepth 2 -type d -name "$name-$version" 2>/dev/null
    }

    while IFS= read -r name; do
        [ -z "$name" ] && continue
        version=$(package_field "$name" version)
        while IFS= read -r stale; do
            [ -z "$stale" ] && continue
            rm -rf "$stale"
        done <<< "$(extraction_dirs "$name" "$version")"
    done <<< "$at_risk"

    cargo publish --workspace --dry-run --locked

    while IFS= read -r name; do
        [ -z "$name" ] && continue
        version=$(package_field "$name" version)
        manifest_path=$(package_field "$name" manifest_path)
        src_dir="$(dirname "$manifest_path")/src"
        extracted="$(extraction_dirs "$name" "$version")"
        if [ -z "$extracted" ]; then
            echo "publish-check never extracted $name-$version during the rehearsal it just ran" >&2
            exit 1
        fi
        while IFS= read -r extracted_dir; do
            [ -z "$extracted_dir" ] && continue
            if ! diff -rq "$src_dir" "$extracted_dir/src" > /dev/null; then
                echo "publish-check verified $name-$version from $extracted_dir, whose src no longer matches $src_dir" >&2
                diff -rq "$src_dir" "$extracted_dir/src" >&2 || true
                exit 1
            fi
        done <<< "$extracted"
    done <<< "$at_risk"

# Sweeps the probe fan-out's pool width against gix's own thread limit, over a fresh
# synthetic corpus this recipe builds and discards. Lives in `tools/fanout-sweep`, its
# own workspace, so it never reaches `just ci`'s build, lint, test or doc passes: a
# benchmark that runs on every push is a benchmark people delete. Run it by hand when
# the fan-out shape itself is in question. `docs/adr/0013` and `docs/spec/refresh.md`'s
# "The fan-out shape" record what the last sweep found, the machine it ran on and the
# corpus it ran over.
sweep-fanout entities="400" seed="1":
    cd tools/fanout-sweep && cargo run --release -- synthetic --entities {{entities}} --seed {{seed}}

# The same sweep, read-only, against real repositories rather than a synthetic corpus:
# pass comma-separated roots, e.g. `just sweep-fanout-real ~/dev,~/dev-misc`. Opens each
# repository exactly as `dirty_counts` does and never fetches, clones or writes. `roots`
# is never defaulted or read from config or the environment, so this recipe cannot
# silently depend on any one machine's checkout; naming a repository ADR 0003 excludes
# from reading is refused rather than skipped.
sweep-fanout-real roots:
    cd tools/fanout-sweep && cargo run --release -- real --roots {{roots}}
