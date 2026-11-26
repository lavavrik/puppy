# Regenerate with: vhs docs/demo.tape   (needs vhs, python3 and `puppy` on PATH)
#
# Everything is isolated: PUPPY_HOME (config, data, socket, daemon) and the download
# directory (set through a throwaway config.toml) live in temp dirs that are deleted at
# the end. The files come from docs/demo_server.py, a throttled (about 4 MiB/s per
# connection), range-capable server on 127.0.0.1:8642 (a fixed port: the URLs are typed into puppy, where
# no shell expands variables), so there is time to pause and
# resume. The recording ends by deleting both downloads in the dashboard and quitting;
# the daemon (started with PUPPY_IDLE_EXIT=1) then idle-exits on its own, and the
# cleanup waits for that before removing the temp dirs.

Output docs/demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1400
Set Height 800
Set Padding 12
Set TypingSpeed 60ms

Hide
Type "export PUPPY_HOME=$(mktemp -d) DEMO=$(mktemp -d) PUPPY_IDLE_EXIT=1" Enter
Type `mkdir -p $PUPPY_HOME/config && printf 'download_dir = "%s"\n' $DEMO/out > $PUPPY_HOME/config/config.toml` Enter
Type "python3 docs/demo_server.py 8642 ubuntu-24.04-desktop.iso=1G dataset.tar.zst=768M & sleep 1" Enter
Type "kill -0 $! || { echo 'demo server failed to start (port 8642 busy?)'; exit 1; }; clear" Enter
Show

Type "puppy" Enter
Sleep 1.5s

# Add two downloads.
Type "a"
Sleep 500ms
Type "http://127.0.0.1:8642/ubuntu-24.04-desktop.iso"
Sleep 500ms
Enter
Sleep 1s
Type "a"
Sleep 500ms
Type "http://127.0.0.1:8642/dataset.tar.zst"
Sleep 500ms
Enter
Sleep 3s

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

# Help.
Type "?"
Sleep 3s
Escape
Sleep 500ms

# Delete both downloads (confirm each), then quit; the daemon idle-exits afterwards.
Type "d"
Sleep 800ms
Enter
Sleep 1s
Type "d"
Sleep 800ms
Enter
Sleep 1s
Type "q"
Sleep 500ms

Hide
Type "sleep 3; kill %1; rm -rf $PUPPY_HOME $DEMO; clear" Enter
