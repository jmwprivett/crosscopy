# crosscopy

Cross-platform clipboard sync over the local network. Windows and macOS first, Linux later.

## Layout

| Crate | Purpose |
|---|---|
| `crosscopy-core` | Platform-independent types: `ClipItem`, MIME-typed payloads, content hashing |
| `crosscopy-clipboard` | Native clipboard read/write and change detection per OS |
| `crosscopy` | The CLI / daemon binary |

## Try it

```sh
cargo run -- watch            # print local clipboard changes
cargo run -- set "some text"  # write to the clipboard
```

## Design notes

- **Change detection:** Windows uses `AddClipboardFormatListener` (push). macOS polls
  `NSPasteboard.changeCount` every 250 ms (AppKit has no notification). Linux polls for now.
- **Sensitive content is never synced.** Items flagged by password managers are skipped:
  `ExcludeClipboardContentFromMonitorProcessing`, `CanUploadToCloudClipboard=0`, etc. on Windows,
  and `org.nspasteboard.ConcealedType` / `TransientType` on macOS.
- **Wire format is OS-neutral:** payloads are keyed by MIME type, and text is LF-normalized
  (converted to CRLF on write on Windows).
- **Dedup / echo suppression** uses BLAKE3 content hashes.

## Roadmap

1. [x] Native clipboard watcher (Windows, macOS)
2. [ ] Text sync between two devices on the LAN (manual peer address, encrypted QUIC)
3. [ ] mDNS discovery, pairing with key pinning, tray icon, autostart
4. [ ] Images, HTML, RTF
5. [ ] Files (eager transfer to a cache first, then lazy/promised files)
6. [ ] Linux: X11 (XFixes), Wayland (`wlr-data-control`), GNOME best-effort
