# Distribution notes

Release and AUR builds compile `protocol/src/launcher.ts` (with its sandboxed
`runtime.ts` worker) into
`protocol/line-rust-protocol`. The resulting package is self-contained and does
not download JavaScript dependencies on first launch. Deno is a build-time tool
only for packaged releases; source-tree development can still use Deno directly.

`make-prebuild.sh` creates the release archive and runs `check-package.sh` to
verify the executable, compiled protocol runtime, desktop entry, and language
catalogs are present.

## Flatpak bundle

`make-flatpak.sh` wraps the prebuilt release in a Flatpak using the GNOME 50
runtime. It grants network, display, audio, GPU, notification, tray, and the
Discord IPC socket access needed by the current client. A scoped permission for
`~/.var/app/dev.linegtk.LineGtk/data/line-gtk` preserves old installation data
after the application ID rename. GTK's document portal provides selected
attachments and download destinations; broad home/host access is not granted.

On Arch Linux, install the builder and create the bundle with:

```bash
yay -S --needed flatpak flatpak-builder
./packaging/make-flatpak.sh
```

Native x86_64 and ARM64 Linux builds are supported. ARM64 artifacts use the
Linux architecture name `aarch64`. The output is
`/tmp/line-rust-VERSION-ARCH.flatpak`. Install the matching bundle locally with:

```bash
flatpak install --user /tmp/line-rust-VERSION-ARCH.flatpak
flatpak run io.github.urguide.LineRust
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
line-rust-VERSION-ARCH.tar.gz
line-rust-VERSION-ARCH.flatpak
SHA256SUMS-ARCH
```

Outputs default to `/tmp`. Set `LINE_RUST_DIST_DIR` to use another directory.
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
<https://github.com/urguide/line-rust/releases>.

### License and matching sources

Arch local installs use `aur/PKGBUILD`; `packaging/aur/line-rust/PKGBUILD`
delegates to that recipe and resolves the latest release and its checksums.
Generate `.SRCINFO` with `makepkg --printsrcinfo` when preparing an AUR submission.
The development recipe is under `packaging/aur/line-rust-git`. Renaming this
repository does not publish or rename any package on the AUR service.

This fork retains the upstream GPL-3.0-or-later license and credits
[MidnightTale/line-gtk](https://github.com/MidnightTale/line-gtk).
Both the prebuilt tarball and Flatpak include `LICENSE` and `README.md`, including
the fork attribution and modification notice.

The publishing workflow adds a direct source archive link for the exact release
tag to the release notes, alongside the binary downloads. Those tagged project
sources contain the Rust and protocol code, dependency lockfiles, and build and
packaging scripts. Third-party components retain their own licenses and notices.
For manual releases, also include the matching source link in the release notes;
do not substitute the upstream repository or a moving development branch.

### First build

1. Enable GitHub Actions in the fork and push the workflow to the default branch.
2. Open **Actions → Release → Run workflow** to test both architectures.
3. Download `release-x86_64` and `release-aarch64` from the completed run.

Manual runs prepare the next version and upload workflow artifacts (retained
for 14 days), without publishing a tag or GitHub Release. Pushes to `master`
publish automatically after both architectures pass.

### Publish a version

Commit and push to `master`. The workflow automatically:

1. Reads the highest stable `vMAJOR.MINOR.PATCH` tag and the package version.
2. Inspects commits since the previous release: `type!:` or a
   `BREAKING CHANGE:` / `BREAKING-CHANGE:` footer bumps major; `feat:` or
   `feat(scope):` bumps minor; everything else bumps patch. Without a previous
   release tag, it inspects the available history. The highest bump wins.
3. Updates `Cargo.toml`, the root package in `Cargo.lock`, and the Flatpak
   AppStream release history in a dedicated release commit.
4. Builds and checks that exact commit on both native architectures.
5. Pushes the version tag and publishes all four packages with `SHA256SUMS`.

Release commits belong to the tags; the workflow does not push version-only
commits back to `master`. Development branch versions may therefore lag behind
releases. The tagged sources and shipped binaries have the generated version.
This avoids branch-write requirements and recursive release builds.

Version selection and publishing are serialized. When pushes arrive rapidly,
GitHub can replace a pending run with the newest one; the later release includes
the accumulated changes. Failed builds do not publish their version tag. Reruns
of a tagged release source reuse the same version; a failed upload resumes its
draft release, while an already published release is left intact. Drafts are
published automatically once upload succeeds, without manual approval.

Explicit version tags remain supported: they must match the tagged
`Cargo.toml`. Prerelease tags such as `v0.1.3-rc.1` create prereleases.
The built-in `GITHUB_TOKEN` needs `contents: write` for the publish job, and
repository rules must permit it to create version tags. No personal token is
needed; publishing happens in the same workflow rather than relying on a
bot-created tag to trigger another workflow.

Download `SHA256SUMS` alongside the four packages and verify them with:

```bash
sha256sum --check SHA256SUMS
```

The AUR recipes in this repository still describe the upstream distribution;
this workflow publishes GitHub assets only and does not update AUR.
