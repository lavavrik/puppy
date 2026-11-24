#!/bin/sh
# Installs (or updates) puppy from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/lavavrik/puppy/main/scripts/install.sh | sh
#
# Picks the archive for this machine, checks it against its published SHA-256,
# and installs the binary to ~/.local/bin (Linux and macOS), a folder puppy can
# replace itself in when it updates.
# The unpacked archive, with the browser extension and service files, is kept in
# ~/.local/share/puppy-install. Set PUPPY_VERSION=0.1.0 to pick a version, or
# PUPPY_BIN_DIR to install somewhere else (a folder you cannot write
# to is installed into with sudo, and puppy then only notifies about updates).
set -eu

repo=lavavrik/puppy

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)                target=x86_64-unknown-linux-gnu  bin=$HOME/.local/bin ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu bin=$HOME/.local/bin ;;
  Darwin-x86_64)               target=x86_64-apple-darwin       bin=$HOME/.local/bin ;;
  Darwin-arm64)                target=aarch64-apple-darwin      bin=$HOME/.local/bin ;;
  *) echo "puppy: no prebuilt binary for $(uname -sm); see the README to build from source" >&2; exit 1 ;;
esac
bin=${PUPPY_BIN_DIR:-$bin}

version=${PUPPY_VERSION:-}
if [ -z "$version" ]; then
  version=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" |
    sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p')
  [ -n "$version" ] || { echo "puppy: could not find the latest release" >&2; exit 1; }
fi

name=puppy-$version-$target
work=$HOME/.local/share/puppy-install
url=https://github.com/$repo/releases/download/v$version/$name.tar.gz

echo "Downloading puppy $version for $target"
mkdir -p "$work"
cd "$work"
curl -fL --progress-bar -o "$name.tar.gz" "$url"
curl -fsSL -o "$name.tar.gz.sha256" "$url.sha256"
if command -v sha256sum >/dev/null; then sha256sum -c "$name.tar.gz.sha256"
else shasum -a 256 -c "$name.tar.gz.sha256"; fi
rm -rf "$name"
tar xzf "$name.tar.gz"

mkdir -p "$bin" 2>/dev/null || true
if [ -w "$bin" ]; then install -m 755 "$name/puppy" "$bin/puppy"
else
  echo "Installing to $bin needs administrator rights"
  sudo mkdir -p "$bin"
  sudo install -m 755 "$name/puppy" "$bin/puppy"
fi

echo "Installed $("$bin/puppy" --version) to $bin/puppy"
echo "Browser extension and service files: $work/$name"
case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "Note: $bin is not on your PATH yet; add it in your shell's startup file." ;;
esac
