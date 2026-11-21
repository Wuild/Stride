#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
topdir=$(realpath -m -- "${1:-"${TMPDIR:-/tmp}/stride-rpmbuild"}")
version=$(awk -F'"' '/^version = "/ {print $2; exit}' "$project/Cargo.toml")
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    printf 'Expected a stable version in Cargo.toml, found %s\n' "$version" >&2
    exit 1
fi

stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
source="$stage/stride-$version"
mkdir -p -- "$source" "$topdir/SOURCES"
cp -R -- "$project"/{Cargo.toml,Cargo.lock,README.md,src,packaging} "$source"
tar -czf "$topdir/SOURCES/stride-$version.tar.gz" -C "$stage" "stride-$version"

rpmbuild -bb --define "_topdir $topdir" --define "stride_version $version" \
    "${@:2}" "$project/packaging/stride.spec"
