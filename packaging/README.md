# Distribution notes

Release and AUR builds compile `protocol/src/main.ts` into
`protocol/line-gtk-protocol`. The resulting package is self-contained and does
not download JavaScript dependencies on first launch. Deno is a build-time tool
only for packaged releases; source-tree development can still use Deno directly.

`make-prebuild.sh` creates the release archive and runs `check-package.sh` to
verify the executable, compiled protocol runtime, desktop entry, and language
catalogs are present.

## Flatpak bundle

`make-flatpak.sh` wraps the prebuilt release in a Flatpak using the GNOME 50
runtime. It grants network, display, audio, GPU, notification, tray, and the
Discord IPC socket access needed by the current client. It does not grant home
directory or host filesystem access; GTK's document portal provides selected
attachments and download destinations.

On Arch Linux, install the builder and create the bundle with:

```bash
yay -S --needed flatpak flatpak-builder
./packaging/make-flatpak.sh
```

Native x86_64 and ARM64 Linux builds are supported. ARM64 artifacts use the
Linux architecture name `aarch64`. The output is
`/tmp/line-gtk-VERSION-ARCH.flatpak`. Install the matching bundle locally with:

```bash
flatpak install --user /tmp/line-gtk-VERSION-ARCH.flatpak
flatpak run dev.linegtk.LineGtk
```

This manifest consumes the locally built, self-contained payload and is intended
for GitHub release bundles. A future Flathub submission should replace the local
payload source with reproducible, network-independent source modules.

## Complete release assets

`make-release-assets.sh` builds and verifies both supported GitHub assets:

```bash
./packaging/make-release-assets.sh
```

It builds for the current machine's architecture and produces:

```text
line-gtk-VERSION-ARCH.tar.gz
line-gtk-VERSION-ARCH.flatpak
SHA256SUMS-ARCH
```

Outputs default to `/tmp`. Set `LINE_GTK_DIST_DIR` to use another directory.
The scripts print the matching `gh release create` command, but do not commit,
tag, push, or publish anything. Rust and Deno builds enforce their lockfiles;
update and commit dependency lockfiles before releasing.

The tarball includes the compiled protocol runtime but still requires system
GTK 4.14+, Libadwaita 1.5+, ffmpeg, and compatible Linux shared libraries.
Release tarballs are built on Ubuntu 24.04; they are not fully static binaries.
The Flatpak uses the GNOME 50 runtime from Flathub.

## Automated GitHub releases

`.github/workflows/release.yml` builds on two native GitHub-hosted runners:

| Architecture | Runner | Assets |
| --- | --- | --- |
| x86_64 | `ubuntu-24.04` | `.tar.gz`, `.flatpak` |
| ARM64 (`aarch64`) | `ubuntu-24.04-arm` | `.tar.gz`, `.flatpak` |

Each runner checks Rust formatting, Clippy, tests, and the Deno protocol before
building and checking both packages. Once both architectures succeed, a final
job verifies their checksums and publishes the four packages plus a combined
`SHA256SUMS` to the repository that triggered the workflow. Publishing uses the
built-in `GITHUB_TOKEN`; no personal access token is required.

For this fork, releases are published to
<https://github.com/urguide/Line-GTK/releases>.

### First build

1. Enable GitHub Actions in the fork and push the workflow to the default branch.
2. Open **Actions → Release → Run workflow** to test both architectures.
3. Download `release-x86_64` and `release-aarch64` from the completed run.

Manual runs only upload workflow artifacts (retained for 14 days); they do not
publish a GitHub Release. Both architectures must pass before a tag-triggered
run can publish.

### Publish a version

1. Update the package version in `Cargo.toml` and the root package entry in
   `Cargo.lock`. Update the Flatpak AppStream release history and release notes.
2. Commit and push the changes.
3. Push a new tag matching the package version exactly, such as `v0.1.3` for
   version `0.1.3`. Tag pushes trigger automatic publishing; branch pushes do not.

Tags with a prerelease suffix, such as `v0.1.3-rc.1`, create GitHub prereleases.
Use an unused tag: the workflow creates a new release and does not overwrite an
existing release. For a failed build, fix the cause and rerun or publish a new
version as appropriate.

Download `SHA256SUMS` alongside the four packages and verify them with:

```bash
sha256sum --check SHA256SUMS
```

The AUR recipes in this repository still describe the upstream distribution;
this workflow publishes GitHub assets only and does not update AUR.
