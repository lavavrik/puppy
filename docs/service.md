# Running the puppy daemon as a login service

puppy keeps all downloads in a background daemon (`puppy daemon`). If none is
running, `puppy` starts one itself; that daemon exits once it has been idle (no
queued or active downloads and no connected clients) for 30 seconds.

For downloads that resume after a reboot without opening puppy, run the daemon
as a login service instead. A daemon started this way never idle-exits. Only one
daemon runs per user: if the service starts while another daemon is already
running, the new one prints a note and exits with status 0, which none of the
service managers below treat as a failure to restart.

The daemon shuts down gracefully on `SIGTERM` (Linux, macOS) or a `shutdown`
request: it pauses active downloads so their progress is saved, writes its
state and removes its socket. Its log is `puppyd.log` in the `logs` folder of
the data directory:

| OS      | Data directory                          |
|---------|-----------------------------------------|
| Linux   | `~/.local/share/puppy`                  |
| macOS   | `~/Library/Application Support/puppy`   |
| Windows | `%APPDATA%\puppy`                       |

The examples assume the install locations from the README's
[Install](../README.md#install) section: `~/.local/bin/puppy` on Linux,
`/usr/local/bin/puppy` on macOS and `%LOCALAPPDATA%\Programs\puppy\puppy.exe` on
Windows. If you installed with `cargo install`, the binary is in `~/.cargo/bin`
instead; adjust the paths.

## Linux (systemd user unit)

The unit is [`packaging/systemd/puppy.service`](../packaging/systemd/puppy.service).
It runs `%h/.local/bin/puppy daemon` (`%h` is your home directory) and restarts
it if it fails.

```sh
mkdir -p ~/.config/systemd/user
cp packaging/systemd/puppy.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now puppy
```

Check on it with `systemctl --user status puppy` or `journalctl --user -u puppy`.

The unit starts when you log in and stops when your last session ends. To keep
the daemon running while you are logged out (e.g. on a server), enable
lingering: `loginctl enable-linger "$USER"`.

To remove it:

```sh
systemctl --user disable --now puppy
rm ~/.config/systemd/user/puppy.service
systemctl --user daemon-reload
```

## macOS (launchd LaunchAgent)

The agent is [`packaging/launchd/dev.puppy.daemon.plist`](../packaging/launchd/dev.puppy.daemon.plist).
It runs `/usr/local/bin/puppy daemon` at login and restarts it if it exits with
an error. If puppy lives elsewhere (e.g. `~/.cargo/bin/puppy`), edit the first
`ProgramArguments` entry; launchd needs an absolute path and does not expand `~`.

```sh
cp packaging/launchd/dev.puppy.daemon.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.puppy.daemon.plist
```

Check on it with `launchctl print gui/$(id -u)/dev.puppy.daemon`.

To remove it:

```sh
launchctl bootout gui/$(id -u)/dev.puppy.daemon
rm ~/Library/LaunchAgents/dev.puppy.daemon.plist
```

## Windows (Task Scheduler)

Create a task that starts the daemon when you log on, with normal (not
elevated) rights. In `cmd.exe`:

```bat
schtasks /Create /SC ONLOGON /TN puppy /TR "\"%LOCALAPPDATA%\Programs\puppy\puppy.exe\" daemon" /RL LIMITED
```

`%LOCALAPPDATA%` is expanded when the task is created. Windows may only allow
creating log-on tasks from an elevated prompt ("Run as administrator"); the
task still runs as you, with limited rights. To start it now without logging
off: `schtasks /Run /TN puppy`.

Tasks created by `schtasks /Create` keep Task Scheduler's default settings:
they are stopped after running for 72 hours, and they do not start (or are
stopped) on battery power. Both are wrong for a daemon. Either untick these in
the task's Settings and Conditions tabs (`taskschd.msc`), or create the task with
PowerShell instead, which lets you turn them off up front:

```powershell
$action   = New-ScheduledTaskAction -Execute "$env:LOCALAPPDATA\Programs\puppy\puppy.exe" -Argument "daemon"
$trigger  = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit 0 -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName puppy -Action $action -Trigger $trigger -Settings $settings -RunLevel Limited
```

`-ExecutionTimeLimit 0` means no time limit. Remove it with
`Unregister-ScheduledTask -TaskName puppy -Confirm:$false`, or with the
`schtasks` commands below.

Without administrator rights, use the Startup folder instead: press Win+R, open
`shell:startup`, and create a shortcut there with the target
`"%LOCALAPPDATA%\Programs\puppy\puppy.exe" daemon` and Run set to "Minimized" (the
daemon is a console program and keeps its window open while it runs).

To remove it:

```bat
schtasks /End /TN puppy
schtasks /Delete /TN puppy /F
```

`schtasks /End` terminates the daemon without a graceful shutdown; downloads
resume from their last saved progress (at most a few seconds old) next time.
Task Scheduler does not restart the daemon if it fails; run `puppy` and it will
start one on demand.
