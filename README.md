# crosscopy

Cross-platform clipboard sync over the local network. Windows and macOS first, Linux later.

## Layout

| Crate | Purpose |
|---|---|
| `crosscopy-core` | Platform-independent types: `ClipItem`, `DeviceId`, wire `Message`, hashing |
| `crosscopy-clipboard` | Native clipboard read/write and change detection per OS |
| `crosscopy-net` | Device identity, mDNS discovery, pairing, pinned mutual-TLS QUIC transport |
| `crosscopy` | The CLI / daemon binary |

## Setup

Start the daemon on each device:

```sh
cargo run --release -- run
```

To pair two devices, run this on **both** at the same time. Check that both show the same
6-digit code, then answer `y` on both:

```sh
cargo run --release -- pair
```

Running daemons pick up the new pairing automatically. Devices find each other over mDNS, so
IP addresses don't matter and can change.

Allow the firewall prompt (UDP, private networks) on first run. On macOS, also allow
"find devices on local networks" for your terminal if asked.

Other commands: `id` (name, code, addresses), `peer list`, `peer remove <name>`,
`peer add <name> <address> <code>` (manual pairing), `watch` (print local changes, no network),
`set "text"`. Use `--config-dir <dir>` to run several instances on one machine.

## Design notes

- **Change detection:** Windows uses `AddClipboardFormatListener` (push). macOS polls
  `NSPasteboard.changeCount` every 250 ms (AppKit has no notification). Linux polls for now.
- **Sensitive content is never synced.** Items flagged by password managers are skipped:
  `ExcludeClipboardContentFromMonitorProcessing`, `CanUploadToCloudClipboard=0`, etc. on Windows,
  and `org.nspasteboard.ConcealedType` / `TransientType` on macOS.
- **Wire format is OS-neutral:** payloads are keyed by MIME type, and text is LF-normalized
  (converted to CRLF on write on Windows).
- **Dedup / echo suppression** uses BLAKE3 content hashes. A single thread owns the clipboard and
  tracks the hash of what is on it, so a clip written from a peer is not sent back out.
- **Trust is pinned, not networked.** A device's code is a hash of its self-signed certificate.
  Both sides of every QUIC connection must present a pinned certificate (mutual TLS 1.3, no
  session resumption, so every connection re-checks the pin). After verifying, the accepting side
  sends `Hello`, so the dialer knows it was really accepted.
- **Discovery is only a hint.** mDNS tells a device where to dial; the pin decides who it trusts.
  Addresses are ranked so LAN beats VPN (Tailscale) and virtual bridges (Hyper-V, WSL, Docker).
- **Pairing** uses a commit/reveal exchange, so the 6-digit code can't be forced to match by
  someone in the middle (1 in a million per attempt). See `crosscopy-net/src/pairing.rs`.
- **Connections:** both peers dial each other with backoff. If two connections form, both sides
  keep the one dialed by the lower device ID. Messages on a connection arrive in order.
- **Config** is reloaded live (polled every 2 s), so pairing and unpairing take effect without a
  restart.

## Roadmap

1. [x] Native clipboard watcher (Windows, macOS)
2. [x] Text sync between two devices on the LAN (encrypted QUIC)
3. [x] mDNS discovery, pairing with confirmation codes, live config reload
4. [ ] Tray icon, autostart, pause/resume
5. [ ] Images, HTML, RTF
6. [ ] Files (eager transfer to a cache first, then lazy/promised files)
7. [ ] Linux: X11 (XFixes), Wayland (`wlr-data-control`), GNOME best-effort
