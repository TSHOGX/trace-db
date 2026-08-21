# TraceDB watch deployment

`trace-db watch` is intentionally a foreground process. A service manager
owns restart policy, stdout/stderr capture, and environment setup while
TraceDB owns discovery, debounce, stability checks, and periodic fallback.

Use an explicit config file so the service does not depend on a login shell:

```bash
trace-db --config "$HOME/.config/trace-db/config.toml" watch --json
```

## launchd (macOS)

Save a user agent as `~/Library/LaunchAgents/com.tracedb.watch.plist`, replacing
the binary path and home directory as needed:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.tracedb.watch</string>
  <key>ProgramArguments</key><array>
    <string>/usr/local/bin/trace-db</string>
    <string>--config</string><string>/Users/you/.config/trace-db/config.toml</string>
    <string>watch</string><string>--json</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/Users/you/Library/Logs/tracedb-watch.jsonl</string>
  <key>StandardErrorPath</key><string>/Users/you/Library/Logs/tracedb-watch.err</string>
</dict></plist>
```

Load and inspect it with:

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.tracedb.watch.plist
launchctl kickstart -k gui/$(id -u)/com.tracedb.watch
launchctl print gui/$(id -u)/com.tracedb.watch
```

## systemd user service (Linux)

Save `~/.config/systemd/user/tracedb-watch.service`:

```ini
[Unit]
Description=TraceDB native-session watcher
After=default.target

[Service]
ExecStart=%h/.cargo/bin/trace-db --config %h/.config/trace-db/config.toml watch --json
Restart=on-failure
RestartSec=5
Environment=RUST_BACKTRACE=1

[Install]
WantedBy=default.target
```

Enable it for the user account:

```bash
systemctl --user daemon-reload
systemctl --user enable --now tracedb-watch.service
systemctl --user status tracedb-watch.service
journalctl --user -u tracedb-watch.service -f
```

Enable lingering if the watcher must run without an interactive login:

```bash
loginctl enable-linger "$USER"
```

## Windows Task Scheduler

Create a task that starts at logon and restarts on failure. The action can be
equivalent to:

```powershell
schtasks /Create /TN TraceDB-Watch /SC ONLOGON /RL LIMITED /F `
  /TR '"C:\\Tools\\trace-db.exe" --config "C:\\Users\\you\\.config\\trace-db\\config.toml" watch --json'
```

Use Task Scheduler's *Restart the task if it fails* policy for recovery. Keep
JSON stdout and diagnostics stderr in separate redirected files if the task is
launched through a wrapper script.

## Operational notes

- `watch_interval_seconds` is the maximum quiet period before a fallback scan;
  it is not a sleep inserted after filesystem runs.
- `watch_debounce_ms` coalesces bursts and bounds the initial file-stability
  check. The latest file contents are ingested even when a writer remains
  active, with a structured stability issue emitted.
- The archive uses WAL and transactional upserts, so a process restart can
  safely repeat the startup scan.
- The built-in macOS daemon uses launchd `KeepAlive`; Linux uses systemd
  `Restart=on-failure`. Windows Task Scheduler requires enabling its restart-on-
  failure policy in the task properties because `schtasks` does not expose that
  policy on the command line used by TraceDB.
- Use `trace-db watch --once --json` as a deployment health probe before
  enabling a long-lived service.
