#!/usr/bin/env bash
# Preserve relative links in the documentation installed by native packages.
set -euo pipefail
if [[ $# != 1 ]]; then
    echo "Usage: $0 DESTINATION" >&2
    exit 1
fi
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$1"
destination=$(cd "$1" && pwd)
cd "$repo"
for doc in README.md CONTRIBUTING.md CHANGELOG.md docs/*.md docs/*.svg \
    debian/control packaging/rpm/p4ui.spec packaging/arch/PKGBUILD.in; do
    mkdir -p "$(dirname "$destination/$doc")"
    install -m644 "$doc" "$destination/$doc"
done
