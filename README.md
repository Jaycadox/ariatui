# AriaTUI

Warning: this entire project is vibe coded. Use at your own risk.

AriaTUI is a terminal UI and background daemon for managing `aria2c`.

It provides:
- a `ratatui` download manager UI
- a daemon that spawns and owns `aria2c`
- current/history views with search, filtering, and sorting
- pause, resume, cancel, purge, and add-by-URL controls
- queue reordering and pause-all/resume-all controls
- scheduled or manual global speed limits
- usual-speed-aware ETA projections for scheduled limits
- regex-based download routing rules
- Discord webhook notifications
- an optional browser-facing web UI with PIN pairing and signed session cookies
- torrent and magnet support, including configurable streaming-biased piece priorities
- a Firefox extension for sending downloads to the web UI

## Run

Start the default flow:

```bash
cargo run
```

This opens the TUI, attaches to a matching daemon if one is already running, and can offer service setup on supported systemd setups.

Run the UI directly:

```bash
cargo run -- ui
```

Run the daemon directly:

```bash
cargo run -- daemon
```

Enable verbose startup logging:

```bash
cargo run -- --verbose
```

## Service

Install the user service:

```bash
cargo run -- service install-user
systemctl --user enable --now ariatui-daemon.service
```

Install the system service:

```bash
cargo run -- service install-system
sudo systemctl enable --now ariatui-daemon.service
```

Uninstall either one later with:

```bash
cargo run -- service uninstall-user
cargo run -- service uninstall-system
```

## Web UI

The web UI is optional and starts disabled by default.

Enable it from the TUI's `Web UI` tab, or by editing `state.toml`.

Default listener:

```text
http://0.0.0.0:39123
```

Browser login uses a 4-digit PIN approved from the terminal UI. After that, the browser keeps a signed session cookie.

## Firefox Extension

There is a Firefox-only extension under `extensions/firefox/`.

See [extensions/firefox/README.md](extensions/firefox/README.md) for the full flow. The short version is:

```bash
./scripts/package_firefox_extension.sh
```

Then load or install the generated `.xpi`, sign into the AriatUI web UI in a normal tab, and use the extension popup or context menu to send links to AriatUI.

## Files

On first run, AriaTUI writes XDG config/state files. On a typical Linux setup these end up at:

```text
~/.config/ariatui/config.toml
~/.local/state/ariatui/state.toml
```

`config.toml` holds app defaults like `aria2c` path, download directory, and polling intervals.

`state.toml` holds live app settings like scheduler ranges, routing rules, webhooks, web UI settings, and torrent behavior.

## Notes

- Linux only
- `aria2c` must be installed
- the daemon communicates with the UI over a local Unix socket
