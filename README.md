Unofficial native LINE client for Linux. GTK4 / Libadwaita UI with a Deno protocol sidecar ([linejs](https://github.com/evex-dev/linejs)).


![main](./assets/761546283_1013837091419784_4898538377611644175_n.jpg)

---

## Implementation

- [x] QR login (additional device)
- [x] Chats, messages, stickers, friends
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
