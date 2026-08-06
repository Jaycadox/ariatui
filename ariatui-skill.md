# AriaTUI CLI skill

Use the `ariatui` CLI to inspect and control the local AriaTUI download daemon. It is designed for both people and automation. Prefer it over editing `config.toml`, `state.toml`, or aria2 session files directly.

## Golden rules for agents

1. Use `--json` for one-shot commands and `--format jsonl` for streams.
2. Discover the installed surface with `ariatui capabilities --json` and `ariatui schema --json`.
3. Select downloads by full `--gid` for mutations. A name selector fails if it is ambiguous.
4. Give every retried `download add` operation a stable `--idempotency-key`.
5. Use `download wait` instead of repeatedly polling `download list`.
6. `download cancel` keeps files unless `--delete-files --yes` is explicitly supplied.
7. Preview an add with `--dry-run` when choosing a destination or advanced options.
8. Never parse human output and never modify daemon-owned state files while it is running.
9. Check the process exit status as well as the structured `ok` field.

## Connection and output

The CLI first uses the current user's daemon socket, then `/run/ariatui/daemon.sock`. Override discovery with `--socket PATH` or `ARIATUI_SOCKET`.

```bash
ariatui status --json
ariatui doctor --json
ariatui capabilities --json
ariatui schema download.add --json
```

Successful JSON output uses this stable envelope:

```json
{"api_version":"1","ok":true,"command":"status","data":{}}
```

Errors use an `error.code`, descriptive message, and `retryable` boolean. Useful exit statuses are: `2` invalid CLI input, `3` daemon unavailable, `4` not found, `5` ambiguous/conflict, `6` permission denied, `7` timeout, `8` operation failure, `9` unsupported feature, and `10` partial batch failure.

## Inspect downloads

```bash
ariatui download list --json
ariatui download list --status active,waiting --name iso --json
ariatui download list --history --json
ariatui download show --gid 2089b05ecca3d829 --json
ariatui download files --gid 2089b05ecca3d829 --json
ariatui torrent peers --gid torrent:3 --json
ariatui torrent files --gid torrent:3 --json
```

## Add downloads

Basic HTTP and magnet downloads:

```bash
ariatui download add 'https://example.com/archive.iso' \
  --dir "$HOME/Downloads/ISOs" \
  --output-name archive.iso \
  --idempotency-key task-482-download-1 \
  --json

ariatui download add 'magnet:?xt=urn:btih:...' \
  --dir "$HOME/Downloads/Torrents" \
  --idempotency-key task-482-torrent-1 \
  --json
```

Resolve redirects, suggested filenames, and remote torrent detection before adding:

```bash
ariatui download resolve 'https://example.com/latest' --json
```

Curated aria2 options:

```bash
ariatui download add "$URL" \
  --header 'Authorization: Bearer ...' \
  --referer 'https://example.com/' \
  --user-agent 'automation/1.0' \
  --checksum 'sha-256=0123abcd...' \
  --connections 8 \
  --split 8 \
  --max-download-limit '12 MiB/s' \
  --paused \
  --idempotency-key task-123 \
  --json
```

For an aria2 option without a curated flag, use the documented escape hatch. `dir` and `out` cannot be overridden this way.

```bash
ariatui download add "$URL" \
  --aria2-option continue=true \
  --aria2-option auto-file-renaming=false \
  --dry-run --json
```

## Control and wait

```bash
ariatui download pause --gid "$GID" --json
ariatui download resume --gid "$GID" --json
ariatui download move --gid "$GID" --offset -1 --json
ariatui queue pause --json
ariatui queue resume --json

ariatui download wait --gid "$GID" \
  --until complete,error \
  --wait-timeout 2h \
  --interval 1s \
  --json
```

Safe cancellation and history cleanup:

```bash
ariatui download cancel --gid "$GID" --json
ariatui download cancel --gid "$GID" --delete-files --yes --json
ariatui history remove --gid "$GID" --json
ariatui history purge --yes --json
```

## Events

Events are derived from daemon snapshot changes. JSON output is automatically emitted as JSONL for a stream.

```bash
ariatui events --format jsonl
ariatui events --count 1 --interval 500ms --format jsonl
```

Possible event types currently include `download.added`, `download.changed`, `download.completed`, and `download.failed`.

## Speed and schedules

```bash
ariatui speed show --json
ariatui speed set '10 MiB/s' --json
ariatui speed unlimited --json
ariatui speed usual '100 MiB/s' --json
ariatui speed mode scheduled --json

ariatui schedule show --json
ariatui schedule set-range --from 8 --to 18 --limit '10 MiB/s' --json
ariatui schedule clear --json
```

`schedule set` accepts exactly 24 comma-separated hourly values. Each value can be a speed or `unlimited`.

## Routing

Rules are ordered regular expressions. The fallback directory is managed separately.

```bash
ariatui route list --json
ariatui route test 'linux-image.iso' --json
ariatui route add --pattern '\.(iso|img)$' \
  --directory "$HOME/Downloads/Images" --before 0 --json
ariatui route update 0 --directory "$HOME/Downloads/ISOs" --json
ariatui route move 1 --offset -1 --json
ariatui route remove 0 --json
ariatui route set-default "$HOME/Downloads" --json
```

## Torrent streaming

```bash
ariatui torrent streaming show --json
ariatui torrent streaming set start-first --head-mib 32 --json
ariatui torrent streaming set start-and-end-first \
  --head-mib 32 --tail-mib 4 --json
ariatui torrent streaming set off --json
```

## Web UI and webhook

```bash
ariatui web status --json
ariatui web enable --json
ariatui web configure --bind 127.0.0.1 --port 39123 --cookie-days 30 --json
ariatui web pairing list --json
ariatui web pairing approve 1234 --json
ariatui web session list --json
ariatui web session revoke-all --yes --json
ariatui web disable --json

ariatui webhook show --json
ariatui webhook configure --url "$DISCORD_WEBHOOK" \
  --ping-mode specific-id --ping-id 123456789 --json
ariatui webhook test --json
ariatui webhook disable --json
```

Webhook URLs are redacted when read back.

Validate or inspect the daemon's combined effective configuration without reading its files directly:

```bash
ariatui config show --json
ariatui config validate --json
```

## Batch and raw API

Batch input is a JSON array of tagged daemon requests. By default all operations are attempted; use `--stop-on-error` to stop at the first failure.

```bash
ariatui batch --file - --json <<'JSON'
[
  {"method":"pause_all"},
  {"method":"set_manual_limit","params":{"limit_bps":10485760}},
  {"method":"resume_all"}
]
JSON
```

For a single advanced operation:

```bash
ariatui api request '{"method":"get_snapshot"}' --json
```

Raw API calls are version-sensitive. Prefer the normal commands whenever one exists.

## Local root/system daemon policy

There is no PIN, token, or account authentication on the Unix control socket. The daemon obtains the caller UID, GID, PID, and supplementary groups from kernel peer credentials. Any connected local user may alter global settings and control downloads.

Commands that create or delete download files receive additional path checks. A destination must be inside the invoking user's home directory or below a directory writable by that user. Relative CLI paths are made absolute before being sent, symlinks are resolved at the nearest existing ancestor, and `--delete-files` is checked against the affected path.

## Recovery patterns

- `daemon_unavailable`: run `ariatui doctor`; then inspect `ariatui service status`.
- `ambiguous_selector`: list matches and retry with a full GID.
- `path_not_permitted`: choose a path under the caller's home or repair directory permissions.
- `timeout`: retry read-only operations; retry adds only with the same idempotency key.
- `operation_failed`: inspect the structured message and current download record before retrying.
