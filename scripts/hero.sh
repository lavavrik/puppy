#!/usr/bin/env bash
# Regenerates the README screenshots docs/hero-dark.png and docs/hero-light.png.
#
#   scripts/hero.sh        (run from anywhere; needs cargo, vhs and ffmpeg)
#
# The frame is not a live recording: the ignored `hero_ansi` test in
# crates/puppy-tui/tests/render.rs draws the dashboard with fixed sample data (every
# download state, an 8-connection HTTP download selected) and writes it as ANSI text
# sized to the vhs terminal. vhs then prints it and takes a screenshot, so the image is
# the same on every run and uses the real theme colors.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo test -q -p puppy-tui --test render --no-run

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

shoot() {
    local name=$1 theme=$2
    cat >"$tmp/hero-$name.tape" <<EOF
Output "$tmp/$name.gif"
Set Shell "bash"
Set FontSize 16
Set Width 1400
Set Height 610
Set Padding 16
Set WindowBar Colorful
Set BorderRadius 8
Set Theme $theme

Hide
Type \`HERO_THEME=$name HERO_SIZE="\$(stty size)" cargo test -q -p puppy-tui --test render hero_ansi -- --ignored >/dev/null 2>&1 || exit 1\` Enter
Sleep 4s
Type \`clear; printf '\\e[?25l'; cat target/hero-$name.ansi; sleep 30\` Enter
Sleep 2s
Show
Sleep 500ms
EOF
    # The GIF's last frame, which has the window bar and padding. (vhs's Screenshot
    # command does not reliably write its file, and its PNG frame output has no window.)
    vhs "$tmp/hero-$name.tape" >/dev/null
    ffmpeg -v error -y -sseof -0.3 -i "$tmp/$name.gif" -frames:v 1 -update 1 "docs/hero-$name.png"
    echo "wrote docs/hero-$name.png"
}

# The terminal colors match puppy's own palettes (src/theme.rs), so the padding around
# the frame blends in.
shoot dark '{"name":"puppy-dark","background":"#1e1e2e","foreground":"#cdd6f4","cursor":"#1e1e2e","selection":"#313244","black":"#45475a","red":"#f38ba8","green":"#a6e3a1","yellow":"#f9e2af","blue":"#89b4fa","magenta":"#f5c2e7","cyan":"#94e2d5","white":"#bac2de","brightBlack":"#585b70","brightRed":"#f38ba8","brightGreen":"#a6e3a1","brightYellow":"#f9e2af","brightBlue":"#89b4fa","brightMagenta":"#f5c2e7","brightCyan":"#94e2d5","brightWhite":"#a6adc8"}'
shoot light '{"name":"puppy-light","background":"#eff1f5","foreground":"#4c4f69","cursor":"#eff1f5","selection":"#ccd0da","black":"#5c5f77","red":"#d20f39","green":"#40a02b","yellow":"#df8e1d","blue":"#1e66f5","magenta":"#ea76cb","cyan":"#179299","white":"#acb0be","brightBlack":"#6c6f85","brightRed":"#d20f39","brightGreen":"#40a02b","brightYellow":"#df8e1d","brightBlue":"#1e66f5","brightMagenta":"#ea76cb","brightCyan":"#179299","brightWhite":"#bcc0cc"}'
