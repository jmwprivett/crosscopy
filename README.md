# crosscopy

Clipboard sync over the local network between Windows, macOS and Linux (Hyprland).

## Install

Download the latest release from the [Releases page](https://github.com/jmwprivett/crosscopy/releases):

- **Windows:** `CrossCopy-<version>-windows-x86_64-setup.exe`. Installs for your user only (no
  admin prompt).
- **macOS:** `CrossCopy-<version>-macos.dmg`. Drag CrossCopy to Applications. Signed and
  notarized; allow "find devices on your local network" when asked.
- **Linux (Hyprland):** `crosscopy-<version>-linux-x86_64.tar.gz`. Unpack and run
  `./install.sh`, which installs to `~/.local/bin` and prints the `exec-once` line for
  `hyprland.conf`. Needs `zenity`, `libayatana-appindicator` and `xdotool`
  (Arch: `sudo pacman -S zenity libayatana-appindicator xdotool`). The icon shows in Waybar's
  `tray` module; on a light bar, set `tray_icon = "black"` in the config file.

CrossCopy updates itself: the tray menu shows **Install update** when a new version is out.

## Use

CrossCopy lives in the Windows tray, macOS menu bar, or Waybar tray. From its menu:

- **Pair new device…**: choose this on both devices. Each shows a 6-digit code; check they
  match and confirm on both.
- **Pause syncing**, **Paired devices → Unpair…**, **Start at login**, **Check for updates**,
  **Open log**, **Quit**.

Devices find each other over mDNS, so IP addresses don't matter and can change.

What syncs:

- **Text**, plus **rich text** (HTML and RTF) when the app you copied from provides it, so
  formatting survives pasting into Word, Pages, Notes, Mail, etc.
- **Images** (screenshots, "Copy image" in a browser, Paint, Preview, ...), sent as PNG.
- **Files and folders** copied in Explorer, Finder or a Linux file manager. They are downloaded
  in the background to a cache folder on the other device, then put on its clipboard, so
  pasting is instant once the download finishes. The tray shows progress for large copies.
  Copies over 500 MB are skipped; change this with `max_file_mb` in the config file (`0` turns
  file sync off). Only the latest download is kept.

On Linux, everything except text needs a compositor with `wlr-data-control` (Hyprland, Sway);
elsewhere only text syncs.

The `crosscopy` command-line tool (installed alongside, or `cargo run --release -- <command>`)
does the same things: `run` (daemon in the foreground), `pair` (on both devices), `id`,
`peer list`, `peer remove <name>`, `peer add <name> <address> <code>` (manual pairing),
`watch`, `set "text"`. Only one of the tray app and `crosscopy run` can run at a time.

## Develop

```sh
cargo run --release -p crosscopy-tray   # tray app
cargo run --release -- <command>        # CLI
cargo test --workspace
# Round-trips through the real clipboard (replaces its contents):
cargo test -p crosscopy-clipboard --test system -- --ignored --test-threads=1
cargo run -p crosscopy-clipboard --example dump   # show what crosscopy reads right now
```

Development builds never update themselves; only release builds (with `CROSSCOPY_UPDATES=1`
set by CI) do.

| Crate | Purpose |
|---|---|
| `crosscopy-core` | Platform-independent types: `ClipItem`, `DeviceId`, wire `Message`, hashing |
| `crosscopy-clipboard` | Native clipboard read/write and change detection per OS |
| `crosscopy-net` | Device identity, mDNS discovery, pairing, pinned mutual-TLS QUIC transport |
| `crosscopy-update` | Signed self-updates from GitHub Releases |
| `crosscopy` | Daemon, config and pairing logic (library) plus the `crosscopy` CLI |
| `crosscopy-tray` | Tray / menu bar app that runs the daemon in the background |
| `xtask` | Release tooling (`cargo xtask --help`) |

## Releasing

1. Bump `version` in the root `Cargo.toml`, commit.
2. `git tag -a v0.2.0 -m "Release notes shown in the update prompt"` and `git push --tags`.

The `Release` workflow builds the Windows installer, the signed and notarized macOS app, and
the Linux tarball, writes `latest.json`, signs it, and publishes a GitHub Release. Running
apps pick it up within 6 hours, or immediately via **Check for updates**.

Secrets live in a `release` environment (Settings → Environments) restricted to `v*` tags:
`UPDATE_SIGNING_KEY`, `APPLE_CERTIFICATE_P12`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_ID`,
`APPLE_APP_PASSWORD`, `APPLE_TEAM_ID` (details at the top of `.github/workflows/release.yml`).

## Design notes

- **Change detection:** Windows uses `AddClipboardFormatListener` (push). macOS polls
  `NSPasteboard.changeCount` every 250 ms (AppKit has no notification). On wlroots Wayland
  (Hyprland, Sway) it uses `wlr-data-control` (push); anything else falls back to polling.
- **Sensitive content is never synced.** Items flagged by password managers are skipped:
  `ExcludeClipboardContentFromMonitorProcessing`, `CanUploadToCloudClipboard=0`, etc. on Windows,
  `org.nspasteboard.ConcealedType` / `TransientType` on macOS, `x-kde-passwordManagerHint` on
  Linux.
- **Wire format is OS-neutral:** payloads are keyed by MIME type (`text/plain`, `text/html`,
  `text/rtf`, `image/png`), and text is LF-normalized (converted to CRLF on write on Windows).
  Each backend converts to native formats on write: `CF_HTML`, PNG plus `CF_DIBV5` and
  `CF_HDROP` on Windows; PNG plus TIFF and file URLs on macOS; `text/uri-list` and
  `x-special/gnome-copied-files` on Linux. All formats of one copy are written in one update.
- **Files** travel as a manifest (relative paths, sizes, times) inside the clip. The receiver
  then fetches the contents from the copying device over a separate stream of the same
  connection, so a big transfer never holds up text. Only files that were actually offered can be
  fetched, and every received path is checked so it can't escape the download folder. A newer
  copy on either side cancels a download in progress.
- **Dedup / echo suppression** uses BLAKE3 fingerprints. A single thread owns the clipboard and
  tracks the fingerprint of what is on it (as read back after writing), so a clip written from a
  peer is not sent back out.
- **Trust is pinned, not networked.** A device's code is a hash of its self-signed certificate.
  Both sides of every QUIC connection must present a pinned certificate (mutual TLS 1.3, no
  session resumption, so every connection re-checks the pin). After verifying, the accepting side
  sends `Hello`, so the dialer knows it was really accepted.
- **Discovery is only a hint.** mDNS tells a device where to dial; the pin decides who it trusts.
  Addresses are ranked so LAN beats VPN (Tailscale) and virtual bridges (Hyper-V, WSL, Docker).
- **Pairing** uses a commit/reveal exchange, so the 6-digit code can't be forced to match by
  someone in the middle (1 in a million per attempt). See `crosscopy-net/src/pairing.rs`.
- **Updates** are only accepted if `latest.json` carries a valid Ed25519 signature from the
  release key (public half compiled into the app), the version is strictly newer, and the
  download matches the signed SHA-256. On macOS the new app must also be signed by the same
  Apple team.
- **Connections:** both peers dial each other with backoff. If two connections form, both sides
  keep the one dialed by the lower device ID. Messages on a connection arrive in order.
- **Config** is reloaded live (polled every second), so pairing and unpairing take effect
  without a restart.
- **Icons** live in `assets/`: `crosscopy.png` (white glyph) and `crosscopy-light.png` (black
  glyph) for the tray, and `app-icon.png` for the app and installers, rendered from the white
  glyph by `cargo xtask app-icon`.

## Roadmap

1. [x] Native clipboard watcher (Windows, macOS)
2. [x] Text sync between two devices on the LAN (encrypted QUIC)
3. [x] mDNS discovery, pairing with confirmation codes, live config reload
4. [x] Tray icon, autostart, pause/resume
5. [x] Installers, signed auto-updates, Linux on Hyprland
6. [x] Images, HTML, RTF
7. [x] Files (eager transfer to a cache folder)
8. [ ] Lazy files: download only on paste
