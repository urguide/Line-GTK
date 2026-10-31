Unofficial native LINE client for Linux. GTK4 / Libadwaita UI with a Deno protocol sidecar ([linejs](https://github.com/evex-dev/linejs)).


![main](./assets/761546283_1013837091419784_4898538377611644175_n.jpg)

---

## Implementation

Message persistence, deduplication, 20-message pagination, gap repair and media
download scheduling are owned by the Rust `line-gtk-core` crate. Messages and
checkpoints use SQLite transactions; legacy JSON histories are imported once
after login and retained as backups. The TypeScript bridge performs LINE API
requests and E2EE decoding; decoded attachments cross IPC as temporary file paths,
not JSON/base64 payloads. Rust validates those paths, stores media and generates
thumbnails with ffmpeg. Rust also owns chat/contact catalog caches, sticker
catalogs and CDN caching, outgoing attachment staging/previews/durations, message
presentation, and PulseAudio capture/playback process lifecycles.

The remaining TypeScript is a narrow protocol adapter: authentication and token
storage, individual LINE RPC calls, E2EE, LINE events, and the experimental
PLANET/Opus wire protocol. It runs inside a permission-restricted Deno worker:
only `protocol-state/` and `cache/transfer/` are readable/writable; subprocess,
FFI and system-query permissions are denied. Audio uses a private Unix socket
to Rust, and attachments use file paths instead of JSON/base64 payloads.

- [x] QR login (additional device)
- [x] Chats, messages, stickers, friends
- [x] 20-message history pages on upward scroll, merged disk cache (360 days in Smart mode)
- [x] Background gap repair on opening a chat, with persisted progress and serial 20-message requests
- [x] Shared serial media download queue with a 500 ms gap between tasks
- [x] Right-click Reply with a quoted-message preview
- [x] Unsend your messages; sync unsends from other devices and participants
- [x] Photos, video, voice messages, files
- [x] Paste and drag-and-drop attachments
- [x] In-app viewers (image, video, PDF, text)
- [x] Downloads, tray, Thai / English, themes
- [x] Discord RPC
- [ ] Voice call (WIP)
- [ ] Account switcher

---

## Requirements

| Dependency | Notes |
| --- | --- |
| Rust | stable toolchain |
| GTK4 + Libadwaita | `pacman -S gtk4 libadwaita` |
| Deno | Build-time protocol compiler; source-tree runs use `deno` or `DENO` |
| ffmpeg / ffprobe | voice record, video thumbs |
| Optional | `pdftoppm`, `pdftotext` for PDF preview |

Wayland and X11 both work. Tested mainly on Arch-based desktops.

---

## Install (Arch / AUR)

```bash
# Development snapshot
yay -S line-gtk-git

# Tagged release
yay -S line-gtk
```

## Install (Flatpak bundle)
Download the matching `x86_64` or `aarch64` (ARM64) bundle from
[this fork's releases](https://github.com/urguide/Line-GTK/releases).

```bash
flatpak install --user ./line-gtk-VERSION-ARCH.flatpak
flatpak run dev.linegtk.LineGtk
```

## Build and run

```bash
git clone https://github.com/urguide/Line-GTK.git
cd Line-GTK
cargo build --release
./target/release/line-gtk
```

## Reply to messages

Right-click a message or open its **⋮** menu and choose **Reply**. The original
message appears above the text input; type your reply and press Enter or Send.
Replies use LINE's native message relation, so other LINE clients can display
the quote too. Click **×** or press Escape to cancel. Switching chats clears the
reply target. Quoted messages outside the loaded history show a placeholder.

## Unsend messages

To unsend a sent message, right-click its bubble or open the **⋮** menu next
to its timestamp and choose **Unsend**. LINE enforces ownership and its current
unsend time limit; if it rejects the request, the message remains visible and
the error is shown. Successful unsends are also applied to local message caches.

## Development checks

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
deno task --config protocol/deno.json check
```

## Releases

Pushing to `master` automatically determines the next version, builds Linux
x86_64 and ARM64 `.tar.gz` and `.flatpak` packages, and publishes a tag and
GitHub Release with `SHA256SUMS` after all checks pass. Breaking changes bump
major, `feat:` commits bump minor, and other changes bump patch. No manual
version edits, tags, or release approval PRs are needed.
**Actions → Release → Run workflow** previews the next release as downloadable
artifacts without publishing. See [distribution notes](packaging/README.md).

---

## Warning

This is an unofficial client. Using it may violate LINE Terms of Service. Prefer a secondary account or device session. Expect breakage when LINE changes login or crypto.

---

## Acknowledgments

Protocol and E2EE talk to LINE through [linejs](https://github.com/evex-dev/linejs) by [evex-dev](https://github.com/evex-dev). This app is a GTK frontend around that library; it is not affiliated with LINE Corporation or evex-dev.

---

## License

[GPL-3.0-or-later](LICENSE)
