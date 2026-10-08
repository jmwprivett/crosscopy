# crosscopy

Cross-platform clipboard sync over the local network. Windows and macOS first, Linux later.

## Layout

| Crate | Purpose |
|---|---|
| `crosscopy-core` | Platform-independent types: `ClipItem`, `DeviceId`, wire `Message`, hashing |
| `crosscopy-clipboard` | Native clipboard read/write and change detection per OS |
| `crosscopy-net` | Device identity, pinned mutual-TLS QUIC transport |
| `crosscopy` | The CLI / daemon binary |

## Setup (two devices)

On **each** device, print its code and address:

```sh
cargo run --release -- id
```

On **each** device, pair with the other one, using the address and code the *other* device printed:

```sh
cargo run --release -- peer add mac 192.168.1.20 ABCD-EFGH-...   # on Windows
cargo run --release -- peer add pc  192.168.1.10 WXYZ-...        # on the Mac
```

Then run the daemon on both:

```sh
cargo run --release -- run
```

Allow the firewall prompt (UDP port 47800, private networks) on first run.

Other commands: `peer list`, `peer remove <name>`, `watch` (print local changes, no network),
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
  Both sides of every QUIC connection must present a pinned certificate (mutual TLS 1.3). After
  verifying, the accepting side sends `Hello`, so the dialer knows it was really accepted.
- **Connections:** both peers dial each other with backoff. If two connections form, both sides
  keep the one dialed by the lower device ID. Messages on a connection arrive in order.

## Roadmap

1. [x] Native clipboard watcher (Windows, macOS)
2. [x] Text sync between two devices on the LAN (manual peer address, encrypted QUIC)
3. [ ] mDNS discovery, pairing with key pinning, tray icon, autostart
4. [ ] Images, HTML, RTF
5. [ ] Files (eager transfer to a cache first, then lazy/promised files)
6. [ ] Linux: X11 (XFixes), Wayland (`wlr-data-control`), GNOME best-effort
