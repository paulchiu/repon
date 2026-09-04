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

# Format code with rustfmt
fmt:
    cargo fmt --all

# Fail if code is not formatted
fmt-check:
    cargo fmt --all --check

# Lint with clippy, warnings are errors
lint:
    cargo clippy --workspace --all-targets --locked --no-deps -- -D warnings

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

    # The other half of the depth-1 check above, which cannot see this either way: the
    # fetch path's network stack is now in the default build of both crates, with no
    # feature gating it. Asserted rather than assumed, because re-gating only gix's own
    # network features would put the tree back as it was while leaving every manifest,
    # spec and ADR here still claiming otherwise, and nothing else in this repo reads the
    # transitive tree.
    check_network_stack_is_unconditional() {
        local -a network_crates=(reqwest rustls hyper-rustls tokio-rustls)

        local core_tree repon_tree
        core_tree=$(cargo tree -p repon-core --edges normal --prefix none)
        repon_tree=$(cargo tree -p repon --edges normal --prefix none)

        for name in "${network_crates[@]}"; do
            if ! grep -qE "^${name} v" <<<"$core_tree"; then
                echo "$name is absent from repon-core's default build; fetch is unconditional" >&2
                echo "now, so the network stack must reach the tree with no feature asked for" >&2
                exit 1
            fi
            if ! grep -qE "^${name} v" <<<"$repon_tree"; then
                echo "$name is absent from repon's default build; the binary a user installs" >&2
                echo "with a plain 'cargo install repon' must carry the fetch path" >&2
                exit 1
            fi
        done
        echo "repon-core's and repon's own default builds both pull: ${network_crates[*]}"
    }

    check_network_stack_is_unconditional

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

# Parse every GitHub workflow and check the ones that call each other line up
#
# release.yml is generated by dist and is 400 lines nobody reads, so a typo in a
# hand-written workflow beside it, or a `uses:` pointing at a file that is not
# there, would otherwise surface as a failed release rather than as a failed
# check. Python is the yaml parser because both runners already have it.
workflows:
    #!/usr/bin/env python3
    import pathlib, sys, yaml

    root = pathlib.Path(".github/workflows")
    files = sorted(root.glob("*.yml"))
    if not files:
        sys.exit("no workflows found under .github/workflows")

    failures = []
    for path in files:
        try:
            document = yaml.safe_load(path.read_text())
        except yaml.YAMLError as error:
            failures.append(f"{path}: {error}")
            continue
        if not isinstance(document, dict) or "jobs" not in document:
            failures.append(f"{path}: no jobs table")
            continue
        for name, job in document["jobs"].items():
            target = isinstance(job, dict) and job.get("uses")
            if isinstance(target, str) and target.startswith("./"):
                # removeprefix, not lstrip: lstrip takes a character set, so
                # "./.github/..." would lose the dot of .github too.
                called = pathlib.Path(target.removeprefix("./"))
                if not called.is_file():
                    failures.append(f"{path}: job {name} calls {target}, which does not exist")

    for failure in failures:
        print(failure, file=sys.stderr)
    if failures:
        sys.exit(1)
    print(f"{len(files)} workflows parse, and every local `uses:` resolves")

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
# pass one or more roots as separate shell words, e.g. `just sweep-fanout-real ~/dev
# ~/dev-misc`, each expanding its own leading `~` the way a single comma-joined argument
# cannot (only the first `~` in a shell word expands, so `~/dev,~/dev-misc` typed as one
# argument silently passes the literal string `~/dev-misc` through). Opens each
# repository exactly as `dirty_counts` does and never fetches, clones or writes. `roots`
# is never defaulted or read from config or the environment, so this recipe cannot
# silently depend on any one machine's checkout; naming a repository ADR 0003 excludes
# from reading is refused rather than skipped, and a root that is not itself a readable
# directory fails the run rather than being dropped from it.
sweep-fanout-real *roots:
    cd tools/fanout-sweep && cargo run --release -- real --roots {{replace(trim(roots), " ", ",")}}

# Sweeps gix's decoded-object cache size over phase D (patch equivalence), which is the
# one phase it acts on and the one the two recipes above deliberately do not run: their
# synthetic corpus is all `Kind::Repo` with a single branch, and production never runs
# phase D for a `Kind::Repo` at all. Read-only, and eligible only for entities whose HEAD
# has actually diverged from its own default branch, which is what `landing::probe`
# answers Outstanding for. Same `roots` discipline as `sweep-fanout-real`: separate shell
# words, never defaulted, never read from config or the environment. Reports peak RSS
# alongside wall clock, since an object cache is a time-against-memory trade and a run
# that printed only one of the two could not settle it. `docs/spec/refresh.md`'s "The
# fan-out shape" and `docs/adr/0013` carry what the last run found.
sweep-landing *roots:
    cd tools/fanout-sweep && cargo run --release -- landing --roots {{replace(trim(roots), " ", ",")}} --cache-limits off,1,4,16,64
