# Firefox Extension

This directory contains a Firefox-only WebExtension for AriatUI.

## Temporary Load

Use this only while iterating on the extension. Firefox removes temporary add-ons when the browser exits.

1. Open `about:debugging#/runtime/this-firefox`.
2. Click `Load Temporary Add-on`.
3. Select [manifest.json](/home/jayphen/oss/ariatui/extensions/firefox/manifest.json).

## Persistent Local Install

For Firefox Nightly or Developer Edition, the practical local-install flow is:

1. Open `about:config`.
2. Set `xpinstall.signatures.required` to `false`.
3. Package the extension:

```bash
./scripts/package_firefox_extension.sh
```

4. Open `about:addons`.
5. Click the gear menu.
6. Choose `Install Add-on From File...`.
7. Select the generated `.xpi` from `dist/`.

The build script prints the exact output path, usually:

```text
dist/download-via-ariatui-0.1.0.xpi
```

That install survives browser and system restarts.

## LibreWolf

The same `.xpi` is what you want for LibreWolf too. I have not verified a current official unsigned-extension installation path for LibreWolf, so treat this as best-effort:

1. Build the `.xpi` with the same packaging script.
2. If your LibreWolf build allows local unsigned XPI installs, use `Install Add-on From File...`.
3. If it rejects unsigned add-ons, you will need a LibreWolf-specific policy or signing workaround outside this repo.

## Updating After Local Install

Unsigned local installs do not get automatic store updates.

When you change the extension:

1. Rebuild the `.xpi`.
2. Reinstall it from `about:addons`.

## Use It

1. Open the extension options page.
2. Save a local or remote AriatUI base URL such as `http://127.0.0.1:39123`.
3. Click `Pair` for that remote.
4. Approve the shown 4-digit PIN from AriatUI’s terminal `Web UI` tab.
5. Right-click a link in Firefox and choose `Download via AriatUI`.

## Notes

- Remotes are stored only in browser extension storage.
- The extension talks to AriatUI through the `/api/*` routes.
- Raw `http://` remotes are supported, but pairing and session traffic are visible to the network unless you use a trusted path or external TLS.
