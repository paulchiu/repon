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
# being one nobody thought to ban.
check-core-isolation:
    #!/usr/bin/env bash
    set -euo pipefail
    allowed="crossbeam-channel gix rayon"
    actual=$(cargo tree -p repon-core --edges normal --depth 1 --prefix none \
        | awk 'NR > 1 && NF { print $1 }' | sort -u | tr '\n' ' ' | sed 's/ $//')
    if [ "$actual" != "$allowed" ]; then
        echo "repon-core's direct dependencies changed" >&2
        echo "  allowed: $allowed" >&2
        echo "  actual:  $actual" >&2
        echo "Add the new crate here only if it belongs in a crate no frontend can avoid" >&2
        exit 1
    fi
    echo "repon-core depends on nothing beyond: $allowed"

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

# The single definition of CI: the GitHub workflow runs this recipe, so a green
# run here is a green pipeline
ci: fmt-check lint test docs check-core-isolation build

# Rehearse the release: package and verify both crates without uploading
#
# `--workspace` resolves repon-core from the workspace rather than the registry,
# which is what makes the binary's publish rehearsable before the library exists
# on crates.io. Kept out of `ci` because it refuses a dirty tree, and `ci` is the
# recipe you run with work in progress.
publish-check:
    cargo publish --workspace --dry-run --locked
