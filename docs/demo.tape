# Regenerate with: vhs docs/demo.tape   (needs vhs, python3, and `puppy` on PATH)
#
# Serves two large sparse files from a throwaway directory on localhost:8000 and
# runs the dashboard against an isolated PUPPY_HOME so the recording never touches
# real downloads.

Output docs/demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1400
Set Height 800
Set Padding 12
Set TypingSpeed 60ms

Hide
Type "export PUPPY_HOME=$(mktemp -d) DEMO=$(mktemp -d) && clear" Enter
Type "truncate -s 4G $DEMO/ubuntu-24.04-desktop.iso && truncate -s 2G $DEMO/dataset.tar.zst" Enter
Type "(cd $DEMO && python3 -m http.server 8000 --bind 127.0.0.1 >/dev/null 2>&1 &) ; sleep 1; clear" Enter
Show

Type "puppy" Enter
Sleep 1.5s

# Add two downloads.
Type "a"
Sleep 500ms
Type "http://127.0.0.1:8000/ubuntu-24.04-desktop.iso"
Sleep 500ms
Enter
Sleep 1s
Type "a"
Sleep 500ms
Type "http://127.0.0.1:8000/dataset.tar.zst"
Sleep 500ms
Enter
Sleep 2s

# Move around; the detail pane follows the selection.
Type "j"
Sleep 1.5s
Type "k"
Sleep 1.5s

# Pause and resume the selected download.
Type "p"
Sleep 2s
Type "r"
Sleep 2s

# Help, then quit (the daemon keeps running).
Type "?"
Sleep 3s
Escape
Sleep 500ms
Type "q"
Sleep 500ms

Hide
Type "pkill -f 'http.server 8000'; puppy daemon --idle-exit 1 >/dev/null 2>&1; rm -rf $DEMO" Enter
