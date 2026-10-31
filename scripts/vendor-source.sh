#!/usr/bin/env bash
# Build a source archive from the exact committed revision, including locked crates.
set -euo pipefail
repo=$(git rev-parse --show-toplevel)
cd "$repo"
version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml | head -n 1)
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Expected a stable x.y.z package version in Cargo.toml" >&2
    exit 1
fi
if ! git diff --quiet HEAD -- Cargo.toml Cargo.lock scripts debian packaging; then
    echo "Commit packaging, scripts, Cargo.toml and Cargo.lock before building an archive." >&2
    exit 1
fi
output_dir=${1:-"$repo/dist"}
mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
prefix="p4ui-$version"
git archive --format=tar --prefix="$prefix/" HEAD | tar -xf - -C "$stage"
(
    cd "$stage/$prefix"
    mkdir -p .cargo
    cargo vendor --locked --versioned-dirs vendor > .cargo/config.toml
    "${PYTHON:-python3}" scripts/third-party-notices.py vendor > THIRD_PARTY_NOTICES
    # Prove the packaged source resolves independently of the caller's cache.
    CARGO_HOME="$stage/empty-cargo-home" cargo metadata --frozen --format-version 1 > /dev/null
)
tar -czf "$output_dir/$prefix.tar.gz" -C "$stage" "$prefix"
printf 'Source archive: %s/%s.tar.gz\n' "$output_dir" "$prefix"
