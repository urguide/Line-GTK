# line-rust

Unofficial native LINE client for Linux. Rust application core and GTK4 / Libadwaita UI with a Deno protocol sidecar ([linejs](https://github.com/evex-dev/linejs)).

## Fork information

This is a modified fork of
[MidnightTale/line-gtk](https://github.com/MidnightTale/line-gtk),
maintained by [urguide](https://github.com/urguide).

As of **2026-11-01**, this fork includes Rust core restructuring, message and
cache handling improvements, protocol recovery fixes, and automated Linux
release packaging. These modifications are maintained by urguide; see the
[Git history](https://github.com/urguide/line-rust/commits/master/) and
[release notes](https://github.com/urguide/line-rust/releases) for individual
changes and their dates.

The original work is by MidnightTale and its contributors. Original copyright,
license, and warranty notices are retained. This fork remains licensed under
[GPL-3.0-or-later](LICENSE).


![main](./assets/761546283_1013837091419784_4898538377611644175_n.jpg)

---

## Implementation

Message persistence, deduplication, 20-message pagination, gap repair and media
download scheduling are owned by the Rust `line-rust-core` crate. Messages and
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

Rust selects E2EE attachment upload for user/group chats and the existing OBS
upload route for other destinations. Legacy `stickers-index.json` recents are
merged into the first signed-in account once, without deleting the source file.
Local capture/playback failures restart the affected audio process with a delay;
the PCM socket and LINE call remain connected until cancellation or socket loss.
Catalog refreshes recover independently from failed boxes, contact batches or
group APIs: Rust tries user profiles and unresolved contacts sequentially and
retains this account's cached rows and avatars. Partial results do not suppress
the next refresh. Rust also preserves the legacy postback-to-text fallback;
successful postbacks never send an extra message.
Sticker package failures are isolated: a failed download or damaged metadata
uses that account's previously rendered pack when available, otherwise skips
only that pack. Recents and healthy packs still render; partial results retry
on the next open, and corrupt metadata files are discarded for a clean refetch.
Missing sticker animations (HTTP 404/410) are negatively cached on disk and use
the static image without another animation request, including after restart.
The retry period follows the existing setting: Day 1 day, Week/Smart 7 days,
Month 30 days, Forever until cache clearing. Network errors and 403/429/5xx are
not recorded as missing. Old untyped `.anim.miss` files are revalidated once.
Animation and static CDN paths are tried sequentially using the legacy Android,
uppercase ANDROID and iPhone variants (including `@2x` names). An animation miss
is persisted only when every candidate reports 404/410; mixed transient failures
remain retryable. Older Android-only miss markers are revalidated across all paths.
Package tab icons use their own disk cache and sequential `android/main.png`,
`ANDROID/main.png`, `android/tab_on.png` fallbacks. If all fail, only that tab
uses its first sticker image; other packages and the Recent tab remain usable.
Rust resolves omitted sticker versions from the owned-package cache/API (legacy
version 1 fallback if lookup is unavailable). Explicit versions take precedence.
Ownership rejection removes the matching recent sticker and deletes ownership
snapshots atomically for that account, preventing stale catalog fallback from
restoring rejected ownership. Failed sends never persist a successful message.

All retention modes share `core/src/cache_policy.rs`; changes in Settings are
sent immediately to the Rust worker and rechecked on subsequent requests.

| Mode | Latest message page | Chat list | Friends | Owned sticker packs | Disk chats/messages |
| --- | --- | --- | --- | --- | --- |
| Smart | 20 min | 30 min | 30 min | 1 day | 360 days |
| Day | 15 min | 30 min | 30 min | 1 day | 1 day |
| Week | 30 min | 60 min | 120 min | 7 days | 7 days |
| Month | 60 min | 120 min | 360 min | 30 days | 30 days |
| Forever | 120 min | 360 min | 360 min | 7 days | Unlimited |

Freshness expiry triggers a refresh; disk retention controls how long saved
history and stale list fallbacks remain usable. Invalidating freshness does not
reset a catalog's disk age. SQLite upgrades preserve existing catalog rows.
Clearing Chats deletes the current account's chat and friend catalog rows,
including stale fallbacks; clearing All also removes its messages/pages/gaps.
Catalogs from other accounts remain intact. A failed refresh or restart cannot
resurrect the deleted lists from SQLite. Migration claims and withdrawal markers
remain so previously imported or unsent messages are not restored by clearing.
Read receipts are best-effort Rust operations: missing IDs, no linked session,
or a rejected API call return `marked: false` inside a successful response
envelope, without a UI error toast. Only an acknowledged receipt clears that
chat's SQLite unread count, preserving other chats, accounts and cache expiry.
Friend IDs are normalized in Rust (trim whitespace and remove the leading `@`).
A successful lookup is retained in the account's chat/friend catalogs even when
the friend-request API rejects an existing friend or is unavailable; the result
separately reports `requestSent`. Missing users remain errors, and the UI waits
for the lookup result before showing success and refreshing the friend list.
Mute/unmute waits for LINE acknowledgement, then updates both account-scoped
chat/friend catalogs transactionally before emitting the Rust `ChatMute` UI
event. Rejected changes leave the previous state intact; offline list fallback
and restart retain successful changes without resetting catalog freshness.
Attachment progress is emitted by Rust at preparation, upload and local-save
stages. Every upload operation emits one terminal success/failure event before
its response, including early validation failures, so the UI can close progress.
These are stage indicators rather than measured byte-transfer percentages.
Upload previews are optional: missing ffmpeg, a failed conversion, or empty output
discards the preview and still uploads and stores the original attachment. The
upload RPC receives no preview path in that case; terminal progress still reports
the outcome of sending and persisting the original.
Audio/video duration probing is optional too: a valid caller-provided duration
takes precedence, otherwise Rust uses ffprobe when available. Missing tools,
nonzero exits, invalid/nonfinite/nonpositive output fall back to the legacy
1919 ms value rather than preventing upload. This fallback is not a measurement
of the actual clip length.
E2EE attachment failures can retry once through linejs `uploadMediaByE2EE`,
selected and paced by Rust with the same staged original, preview and duration.
User/group uploads never fall back to plaintext. Session cancellation or an IPC
timeout/disconnection does not resend an upload whose server outcome is unknown.
Attachment classification and MIME selection are centralized in Rust, including
BMP/HEIC images, M4V video, AAC/OGA/FLAC audio, case-insensitive explicit types,
and the legacy `image` + `.gif` correction to animated `gif`. Recognized explicit
types take precedence; unknown types use filename detection. MIME metadata keeps
the previous LINE client's mappings and defaults; this does not transcode media.
Video cache reuse supports MP4, WebM, MOV, MKV and M4V originals, with their
existing thumbnails. Both saved paths and legacy filename discovery validate
nonempty regular files inside the media cache (including symlink resolution),
so reopening a cached non-MP4 video does not request another download.
Generic attachments retain a short ASCII basename extension (for example
`123.pdf` or `123.txt`) so existing file viewers receive a usable preview path.
Sender-controlled directory components are never used in cache paths; missing
suffixes use PDF/plain-text MIME hints or `.bin`. Older `.bin` downloads with a
known filename extension are renamed locally without re-downloading.
Downloaded image/video originals remain available even if thumbnail generation
fails: incomplete previews are removed, images can display the original, and
videos retain a playable file path. Opening them again uses the local original
instead of downloading again solely because a thumbnail is unavailable.
History decoding is isolated per message: the protocol returns either decoded
data or a fixed error code plus the original wire metadata. Rust preserves a
safe fallback row in SQLite, shows an unavailable-message label for encrypted
content, and keeps the page cursor advancing. Failed encrypted media is not
automatically downloaded; a later successful decode can replace the fallback.
Live decode and event failures are reported to Rust without clearing the client.
Per-message decoder failures produce the same fallback envelope and leave the
polling loop able to deliver the next message. Rust terminates the session only
for recognized authentication/logout codes, stops audio, aborts pending RPCs,
and directs the adapter to stop listening and clear authentication state.

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

## Install (Arch)

Use the local recipe to download and install the latest prebuilt release:

```bash
git clone https://github.com/urguide/line-rust.git
cd line-rust/aur
makepkg -si
```

## Install (Flatpak bundle)
Download the matching `x86_64` or `aarch64` (ARM64) bundle from
[this fork's releases](https://github.com/urguide/line-rust/releases).

```bash
flatpak install --user ./line-rust-VERSION-ARCH.flatpak
flatpak run io.github.urguide.LineRust
```

## Build and run

```bash
git clone https://github.com/urguide/line-rust.git
cd line-rust
cargo build --release
./target/release/line-rust
```

## Upgrading from LINE GTK

Quit the old application before starting line-rust. New installations use
`~/.local/share/line-rust` (or `$XDG_DATA_HOME/line-rust`). On an existing
installation, line-rust creates a link to the old `line-gtk` data directory
and reuses it, preserving settings, login, SQLite history and absolute media
cache paths. An existing `line-rust` profile always takes precedence.
The diagnostics page shows the actual data path; new logs are `line-rust.log`.

The Flatpak ID is now `io.github.urguide.LineRust`. Install the new bundle;
it can reuse `~/.var/app/dev.linegtk.LineGtk/data/line-gtk` through a narrowly
scoped filesystem permission. Keep that old data directory when uninstalling
the old app; do not use `flatpak uninstall --delete-data` on the old ID.
Run only one of the old and new applications at a time when sharing data.

Environment variables now use `LINE_RUST_`. The old `LINE_GTK_ROOT` and
`LINE_GTK_DISCORD_APP_ID` overrides are accepted for compatibility.
Discord's default application ID is still the upstream public ID; a separate
Discord application must be configured in Settings to change its registered branding.

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

This project, including this fork's modifications, is licensed under
[GNU GPL version 3 or any later version](LICENSE), without any warranty.
Third-party components remain subject to their respective licenses and notices.

For a published binary release, use the matching release tag's **Source code**
download on the [release page](https://github.com/urguide/line-rust/releases).
The tagged project sources include the build and packaging scripts and dependency
lockfiles. Build instructions are above and in [distribution notes](packaging/README.md).
