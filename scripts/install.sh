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
# On Linux the glibc build is picked when the system has glibc 2.35 or newer, and
# the static (musl) build otherwise; set PUPPY_LIBC=gnu or PUPPY_LIBC=musl to choose.
set -eu

repo=lavavrik/puppy
min_glibc=2.35

# The system's glibc version (2.35), or nothing when it has no glibc (Alpine, ...).
glibc_version() {
  v=$(getconf GNU_LIBC_VERSION 2>/dev/null) || v=
  case $v in
    "glibc "*) v=${v#glibc } ;;
    *)
      # "ldd (GNU libc) 2.35", "ldd (Ubuntu GLIBC 2.35-0ubuntu3) 2.35"; musl's
      # ldd prints "musl libc" instead.
      v=$(ldd --version 2>&1 | head -n 1) || v=
      case $v in
        *"GNU libc"* | *GLIBC* | *"GNU C Library"*) v=${v##* } ;;
        *) v= ;;
      esac
      ;;
  esac
  case $v in
    [0-9]*.[0-9]*) echo "$v" ;;
  esac
}

# Whether version $1 (major.minor) is at least $2.
version_at_least() {
  a=${1%%.*} b=${1#*.} c=${2%%.*} d=${2#*.}
  b=${b%%.*} d=${d%%.*}
  [ "$a" -gt "$c" ] || { [ "$a" -eq "$c" ] && [ "$b" -ge "$d" ]; }
}

# Which Linux build to install: gnu or musl (PUPPY_LIBC, else from the glibc version).
linux_libc() {
  case ${PUPPY_LIBC:-} in
    gnu | musl) echo "$PUPPY_LIBC"; return ;;
    "") ;;
    *) echo "puppy: PUPPY_LIBC must be gnu or musl, not '$PUPPY_LIBC'" >&2; return 1 ;;
  esac
  v=$(glibc_version)
  if [ -n "$v" ] && version_at_least "$v" "$min_glibc"; then echo gnu; else echo musl; fi
}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)                libc=$(linux_libc); target=x86_64-unknown-linux-$libc  bin=$HOME/.local/bin ;;
  Linux-aarch64 | Linux-arm64) libc=$(linux_libc); target=aarch64-unknown-linux-$libc bin=$HOME/.local/bin ;;
  Darwin-x86_64)               target=x86_64-apple-darwin       bin=$HOME/.local/bin ;;
  Darwin-arm64)                target=aarch64-apple-darwin      bin=$HOME/.local/bin ;;
  *) echo "puppy: no prebuilt binary for $(uname -sm); see the README to build from source" >&2; exit 1 ;;
esac
bin=${PUPPY_BIN_DIR:-$bin}
if [ "${libc:-}" = musl ] && [ -z "${PUPPY_LIBC:-}" ]; then
  echo "No glibc $min_glibc or newer found: installing the static (musl) build."
  echo "Set PUPPY_LIBC=gnu to install the glibc build instead."
fi

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

# Earlier macOS installs went to /usr/local/bin, where puppy cannot update itself.
# Left in place (it may be another install the user wants); only pointed out.
old=/usr/local/bin/puppy
if [ "$(uname -s)" = Darwin ] && [ -e "$old" ] && [ "$old" != "$bin/puppy" ]; then
  echo
  echo "Note: an older puppy is still installed at $old. That copy will not update"
  echo "itself, and it may come first on your PATH. Remove it with:"
  echo "  sudo rm $old"
  echo "If you installed the launchd LaunchAgent, copy the new one over it and load it"
  echo "again (see docs/service.md); it starts $HOME/.local/bin/puppy:"
  echo "  cp $work/$name/packaging/launchd/dev.puppy.daemon.plist ~/Library/LaunchAgents/"
fi
