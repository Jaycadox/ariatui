# AriaTUI

Warning: this entire project is vibe coded. Use at your own risk.

AriaTUI is a terminal UI and background daemon for managing `aria2c`.

It provides:
- a `ratatui` download manager UI
- a daemon that spawns and owns `aria2c`
- pause, resume, cancel, and add-by-URL controls
- scheduled or manual global speed limits
- regex-based download routing rules

## Run

Start the default flow:

```bash
cargo run
```

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

## Notes

- Linux only
- `aria2c` must be installed
- the daemon communicates with the UI over a local Unix socket
