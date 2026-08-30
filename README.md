# AriaTUI

Warning: this entire project is vibe coded. Use at your own risk.

AriaTUI is a terminal UI and background daemon for managing downloads.

It provides:
- a `ratatui` download manager UI
- a daemon that spawns and owns `aria2c` for non-torrent downloads
- current/history views with search, filtering, and sorting
- pause, resume, cancel, purge, and add-by-URL controls
- queue reordering and pause-all/resume-all controls
- numbered download batches with a queue that holds later batches back
- scheduled or manual global speed limits
- usual-speed-aware ETA projections for scheduled limits
- regex-based download routing rules
- Discord webhook notifications
- an optional browser-facing web UI with PIN pairing and signed session cookies
- torrent and magnet support through `librqbit`, including sequential-read media mode
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

## CLI and agent automation

AriaTUI also provides a structured local CLI covering downloads, queues, history, speed limits, schedules, routing, torrents, the Web UI, webhooks, and service management:

```bash
ariatui status
ariatui download list --json
ariatui download add 'https://example.com/file.iso' \
  --dir "$HOME/Downloads" \
  --idempotency-key example-file \
  --json
ariatui download wait --gid 2089b05ecca3d829 --until complete,error --json
```

Run `ariatui capabilities --json` for machine-readable discovery and see [ariatui-skill.md](ariatui-skill.md) for the full agent-oriented guide.

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

## Queue & Batches

Every download can carry a batch number from `0` to `9999`, or be unassigned.

Batches run in ascending order and only one batch is *in play* at a time:
downloads in later batches are paused by the scheduler and marked `held`. When a
batch has nothing left to run, the next held batch starts on its own. Unassigned
downloads sort after every numbered batch. `queue_slots` in `state.toml` (1-16,
default `3`) decides how many downloads in the batch in play run at once, and is
applied to `aria2c` as `--max-concurrent-downloads`.

Holding is not the same as pausing: a scheduler-held download is started again
when its batch comes up, while a download you paused yourself stays paused.

In the TUI, on the `Current` tab:

- `a` add links; `Tab` moves to the batch field of the add form
- `b` set the batch of the selected download (blank clears it)
- `[` and `]` nudge the selected download's batch down or up
- `H` hold the selected download's batch: it stays paused until you start it, so
  the following batches get their turn
- `S` start the selected download's batch now and park the others; parked
  batches resume on their own turn
- `Q` change the download slot count
- `f` filter down to `held`, `s` sort by `batch`

The web UI shows the batch of every row with an inline batch field, plus a queue
panel with the slot count and per-batch `Hold`/`Start` buttons. The Firefox
extension has a per-server `default batch number` option so links it sends land
in a batch of your choosing.

### Speed and ETA estimation

Displayed transfer speeds come from byte-counter deltas rather than directly
from aria2's instantaneous sample. AriaTUI rejects isolated outliers, combines
fast and slow exponential estimates, limits abrupt display changes, and treats
sustained changes as a new baseline. Short stalls decay smoothly; long stalls
clear the ETA. Pauses and batch holds reset sampling boundaries so idle time is
never averaged into a resumed transfer.

In scheduled mode, ETA is a queue simulation rather than a simple
`remaining / current speed` calculation. It accounts for hourly caps, usual
Internet speed, measured utilization, active sharing ratios, slot backfilling,
numbered and unassigned batch order, scheduler-held downloads, and bandwidth
redistribution as peers finish. Manually paused downloads are never assumed to
resume. Schedule transitions use local-time boundaries, including DST skips and
repeated hours.

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

`state.toml` holds live app settings like scheduler ranges, routing rules, webhooks, web UI settings, torrent behavior, and the download slot count.

`queue-state.json` remembers each queued download's batch number and whether the scheduler is holding it, so batches survive a daemon restart.

## Notes

- Linux only
- `aria2c` must be installed
- the daemon communicates with the UI over a local Unix socket
