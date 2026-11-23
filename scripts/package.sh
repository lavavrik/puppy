#!/usr/bin/env bash
# Packages a built puppy binary into a release archive in dist/:
#   puppy-<version>-<target>.tar.gz (.zip for Windows) plus a .sha256 file.
#
#   scripts/package.sh <target> [profile]
#
# Expects `cargo build --profile <profile> --target <target> -p puppy` to have run
# (profile defaults to dist). Honours CARGO_TARGET_DIR. Runs on Linux, macOS and
# the Git Bash shell of GitHub's Windows runners.
set -euo pipefail

target=${1:?usage: scripts/package.sh <target> [profile]}
profile=${2:-dist}
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

id=$(cargo pkgid -p puppy)
version=${id##*#}
version=${version##*@}

exe=puppy
case $target in *windows*) exe=puppy.exe ;; esac
bin=${CARGO_TARGET_DIR:-target}/$target/$profile/$exe
[[ -f $bin ]] || { echo "error: $bin not found; build it first" >&2; exit 1; }

name=puppy-$version-$target
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir=$stage/$name
mkdir -p "$dir/extension"

cp "$bin" "$dir/"
cp README.md LICENSE-MIT LICENSE-APACHE "$dir/"
cp -R packaging "$dir/"
# The browser extension is loaded unpacked from this folder; its tests stay behind.
for f in extension/*; do
  case ${f##*/} in tests | package.json) ;; *) cp -R "$f" "$dir/extension/" ;; esac
done

mkdir -p dist
case $target in
  *windows*)
    archive=$name.zip
    rm -f "dist/$archive"
    if command -v 7z >/dev/null; then
      (cd "$stage" && 7z a -tzip -bso0 -bsp0 "$root/dist/$archive" "$name")
    else
      (cd "$stage" && zip -qr "$root/dist/$archive" "$name")
    fi
    ;;
  *)
    archive=$name.tar.gz
    tar -C "$stage" -czf "dist/$archive" "$name"
    ;;
esac

if command -v sha256sum >/dev/null; then sum=(sha256sum); else sum=(shasum -a 256); fi
(cd dist && "${sum[@]}" "$archive" >"$archive.sha256")

echo "dist/$archive"
if [[ -n ${GITHUB_OUTPUT:-} ]]; then echo "archive=dist/$archive" >>"$GITHUB_OUTPUT"; fi
