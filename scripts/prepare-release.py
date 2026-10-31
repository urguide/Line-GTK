#!/usr/bin/env python3
"""Prepare a versioned release commit without modifying the development branch."""

import os
from pathlib import Path
import re
import subprocess
import tomllib


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def stable_version(value):
    if re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", value):
        return tuple(map(int, value.split(".")))
    raise ValueError(f"Not a stable version: {value}")


def next_version(base, messages):
    major, minor, patch = base
    if re.search(r"(?m)^[a-z]+(?:\([^\n)]*\))?!:|^BREAKING[ -]CHANGE:", messages):
        return f"{major + 1}.0.0"
    if re.search(r"(?m)^feat(?:\([^\n)]*\))?:", messages):
        return f"{major}.{minor + 1}.0"
    return f"{major}.{minor}.{patch + 1}"


def update_versions(version, date):
    manifest = Path("Cargo.toml")
    text, count = re.subn(
        r'(?m)^(version\s*=\s*)"[^"]+"',
        lambda match: f'{match[1]}"{version}"',
        manifest.read_text(), count=1,
    )
    if count != 1:
        raise ValueError("Cargo.toml package version missing")
    manifest.write_text(text)

    lockfile = Path("Cargo.lock")
    text, count = re.subn(
        r'(\[\[package\]\]\nname = "line-gtk"\nversion = ")[^"]+(")',
        lambda match: f"{match[1]}{version}{match[2]}",
        lockfile.read_text(), count=1,
    )
    if count != 1:
        raise ValueError("Cargo.lock root package version missing")
    lockfile.write_text(text)

    metadata = Path("packaging/flatpak/dev.linegtk.LineGtk.metainfo.xml")
    text = metadata.read_text()
    entry = (
        f'\n    <release version="{version}" date="{date}">\n'
        '      <description><p>Automated release with improvements and fixes.</p></description>\n'
        '    </release>'
    )
    if "<releases>" not in text:
        raise ValueError("AppStream releases element missing")
    metadata.write_text(text.replace("<releases>", "<releases>" + entry, 1))


def main():
    source = git("rev-parse", "HEAD")
    current = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
    tag_mode = os.environ.get("GITHUB_REF_TYPE") == "tag"
    if tag_mode:
        tag = os.environ["GITHUB_REF_NAME"]
        if tag != f"v{current}":
            raise ValueError(f"Tag must match Cargo.toml: expected v{current}")
        version, commit = current, source
    else:
        tags = []
        for tag in git("tag", "--list", "v*").splitlines():
            try:
                tags.append((stable_version(tag[1:]), tag))
            except ValueError:
                continue
        tags.sort(reverse=True)
        # A failed publishing job can be rerun without allocating another version.
        existing = next((tag for _, tag in tags if
                         f"Release-source: {source}" in git("show", "-s", "--format=%B", tag).splitlines()), None)
        if existing:
            tag, version = existing, existing[1:]
            commit = git("rev-parse", f"{tag}^{{commit}}")
        else:
            base = max([stable_version(current)] + [value for value, _ in tags])
            previous = next((tag for _, tag in tags if subprocess.run(
                ["git", "merge-base", source, tag], capture_output=True
            ).returncode == 0), None)
            revision = f"{previous}..{source}" if previous else source
            version = next_version(base, git("log", "--format=%B", revision))
            tag = f"v{version}"
            date = git("show", "-s", "--format=%cI", source)
            update_versions(version, date[:10])
            subprocess.run(["git", "add", "Cargo.toml", "Cargo.lock",
                            "packaging/flatpak/dev.linegtk.LineGtk.metainfo.xml"], check=True)
            tree = git("write-tree")
            env = dict(os.environ, GIT_AUTHOR_NAME="github-actions[bot]",
                       GIT_AUTHOR_EMAIL="41898282+github-actions[bot]@users.noreply.github.com",
                       GIT_COMMITTER_NAME="github-actions[bot]",
                       GIT_COMMITTER_EMAIL="41898282+github-actions[bot]@users.noreply.github.com",
                       GIT_AUTHOR_DATE=date, GIT_COMMITTER_DATE=date)
            commit = subprocess.check_output(
                ["git", "commit-tree", tree, "-p", source], text=True, env=env,
                input=f"chore(release): {tag}\n\nRelease-source: {source}\n",
            ).strip()
            subprocess.run(["git", "tag", tag, commit], check=True)

    bundle = Path(os.environ["RUNNER_TEMP"]) / "release-source.bundle"
    subprocess.run(["git", "bundle", "create", str(bundle), f"refs/tags/{tag}"], check=True)
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(f"version={version}\ntag={tag}\ncommit={commit}\n")
    print(f"Prepared {tag} at {commit} from {source}")


if __name__ == "__main__":
    main()
