#!/usr/bin/env python3
"""Copy a stopped LINE GTK profile into an independent line-rust profile."""

import argparse
from contextlib import closing
from datetime import datetime
import json
import os
from pathlib import Path
import shutil
import sqlite3
import stat
import sys
import tempfile
import uuid


AUTH_FILES = ("auth-token.txt", "auth-device.txt", "linejs-storage.json")
PROFILE_ENTRIES = ("settings.json", "cache", "protocol-state", *AUTH_FILES)


def absolute(path):
    # Do not resolve the last component: the destination may be the app's
    # compatibility symlink, which must be replaced rather than followed.
    path = Path(os.path.abspath(Path(path).expanduser()))
    return path.parent.resolve() / path.name


def is_profile(path):
    return path.is_dir() and any((path / name).exists() for name in PROFILE_ENTRIES)


def choose_paths(args):
    base = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
    flatpak = Path.home() / ".var/app"
    default_destination = (
        flatpak / "io.github.urguide.LineRust/data/line-rust"
        if args.flatpak else base / "line-rust"
    )
    destination = absolute(args.destination or default_destination)
    if args.source:
        source = absolute(args.source)
    elif destination.is_symlink() and is_profile(destination):
        source = destination.resolve()
    else:
        candidates = [base / "line-gtk", flatpak / "dev.linegtk.LineGtk/data/line-gtk"]
        found = {path.resolve(): absolute(path) for path in candidates if is_profile(path)}
        if len(found) != 1:
            raise ValueError("無法唯一判定舊版資料目錄，請用 --source 指定。")
        source = next(iter(found.values()))
    if not is_profile(source):
        raise ValueError(f"找不到有效的舊版資料目錄：{source}")
    return source, destination


def rebase_path(value, roots, destination):
    for root in roots:
        prefix = str(root)
        if value == prefix or value.startswith(prefix + "/"):
            return str(destination) + value[len(prefix):]
    return value


def rewrite_json(value, roots, destination, key=""):
    # Only rewrite path fields, never message text, IDs, URLs or opaque keys.
    if isinstance(value, dict):
        return {k: rewrite_json(v, roots, destination, k) for k, v in value.items()}
    if isinstance(value, list):
        return [rewrite_json(v, roots, destination, key) for v in value]
    if isinstance(value, str) and (
        key == "path" or key.endswith("Path") or key.endswith("_path")
        or key.startswith("download_dir_")
    ):
        return rebase_path(value, roots, destination)
    return value


def rewrite_json_file(path, roots, destination):
    original = json.loads(path.read_text(encoding="utf-8"))
    updated = rewrite_json(original, roots, destination)
    if updated != original:
        times = path.stat()
        path.write_text(json.dumps(updated, ensure_ascii=False, indent=2) + "\n",
                        encoding="utf-8")
        os.utime(path, ns=(times.st_atime_ns, times.st_mtime_ns))


def rewrite_database(path, roots, destination):
    # Work only on the copied DB. Its copied WAL is recovered by SQLite, so
    # committed messages not yet checkpointed into the main file are included.
    with closing(sqlite3.connect(path)) as db:
        if db.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
            raise ValueError(f"SQLite 完整性檢查失敗：{path.name}")
        tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        with db:
            for table in ("messages", "pages", "catalogs"):
                if table not in tables:
                    continue
                # The application's tables have rowids. Fetch in batches rather
                # than loading the entire message history into memory.
                cursor = db.execute(f'SELECT rowid, body FROM "{table}"')
                while rows := cursor.fetchmany(500):
                    for rowid, body in rows:
                        original = json.loads(body)
                        updated = rewrite_json(original, roots, destination)
                        if updated != original:
                            db.execute(f'UPDATE "{table}" SET body=? WHERE rowid=?',
                                       (json.dumps(updated, ensure_ascii=False), rowid))
            if "migrations" in tables:
                for rowid, old in db.execute("SELECT rowid, path FROM migrations").fetchall():
                    new = rebase_path(old, roots, destination)
                    if new != old:
                        db.execute("UPDATE migrations SET path=? WHERE rowid=?", (new, rowid))
        # Keep migration claims aligned with the relocated legacy JSON files;
        # otherwise deleted history could be imported again after login.
        if db.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
            raise ValueError(f"遷移後 SQLite 完整性檢查失敗：{path.name}")
        if db.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()[0] != 0:
            raise ValueError("SQLite WAL checkpoint 未完成。")


def copied_entries(root):
    for directory, dirs, files in os.walk(root, followlinks=False):
        for name in dirs + files:
            yield Path(directory) / name


def prepare_copy(source, stage, destination, roots):
    def ignore_sockets(directory, names):
        # Stale audio Unix sockets are runtime endpoints, not persistent data.
        return [name for name in names
                if stat.S_ISSOCK((Path(directory) / name).lstat().st_mode)]

    shutil.copytree(source, stage, symlinks=True, ignore=ignore_sockets)
    stage.chmod(0o700)
    for path in copied_entries(stage):
        if path.is_symlink():
            target = os.readlink(path)
            # Resolve relative links lexically against the original profile;
            # external targets must keep working even across Flatpak layouts.
            original_parent = source / path.relative_to(stage).parent
            old_target = os.path.normpath(os.path.join(original_parent, target))
            rebased = rebase_path(old_target, roots, destination)
            if rebased != old_target:
                relative = os.path.relpath(rebased, destination / path.relative_to(stage).parent)
                path.unlink()
                path.symlink_to(relative)
            elif not os.path.isabs(target):
                path.unlink()
                path.symlink_to(old_target)
            continue
        if path.is_dir():
            path.chmod(0o700)
        elif path.is_file():
            path.chmod(0o600)

    for path in copied_entries(stage):
        if path.is_symlink() or not path.is_file() or path.suffix != ".json":
            continue
        relative = path.relative_to(stage)
        is_cache = relative.parts[0] == "cache" and len(relative.parts) > 1
        # Downloaded documents may themselves be JSON. They are attachments,
        # not cache metadata; authentication/E2EE storage is opaque as well.
        if relative.as_posix() in ("settings.json", "stickers-index.json") or (
            is_cache and relative.parts[1] not in ("media", "avatars", "transfer")
        ):
            rewrite_json_file(path, roots, destination)

    database = stage / "cache/messages.sqlite3"
    linked_database = any(Path(str(database) + suffix).is_symlink()
                          for suffix in ("", "-wal", "-shm", "-journal"))
    if linked_database or (stage / "cache").is_symlink():
        raise ValueError("cache 或 messages.sqlite3 是符號連結，請先將它實體化再遷移。")
    if database.exists():
        rewrite_database(database, roots, destination)

    # Match sidecar.rs exactly: an existing import marker must not resurrect
    # logged-out credentials from the retained legacy files.
    protocol = stage / "protocol-state"
    if protocol.is_symlink():
        raise ValueError("protocol-state 是符號連結，請先將它實體化再遷移。")
    protocol.mkdir(mode=0o700, exist_ok=True)
    marker = protocol / "legacy-imported"
    if not os.path.lexists(marker):
        for name in AUTH_FILES:
            old, new = stage / name, protocol / name
            if old.is_file() and not os.path.lexists(new):
                shutil.copyfile(old, new)
                new.chmod(0o600)
        marker.write_text("1", encoding="utf-8")
        marker.chmod(0o600)


def migrate(source, destination, replace=False, dry_run=False):
    source_alias = absolute(source)
    source = source_alias.resolve(strict=True)
    destination = absolute(destination)
    if not is_profile(source):
        raise ValueError(f"找不到有效的舊版資料目錄：{source}")
    if source == destination or source in destination.parents or destination in source.parents:
        raise ValueError("來源與目標不可相同，也不可互相包含。")
    exists = os.path.lexists(destination)
    compatibility_link = destination.is_symlink() and destination.resolve() == source
    if exists and not compatibility_link and not replace:
        raise ValueError(f"新版目錄已存在：{destination}；如要備份後替換，請加 --replace。")
    roots = {source, source_alias}
    if compatibility_link:
        roots.add(destination)
    roots = sorted(roots, key=lambda path: len(str(path)), reverse=True)
    print(f"來源：{source}")
    print(f"目標：{destination}")
    print("原始資料會保留；既有目標會先改名備份。" if exists else "原始資料會保留。")
    if dry_run:
        print("預覽完成，未寫入任何檔案。")
        return

    destination.parent.mkdir(parents=True, exist_ok=True)
    backup = None
    with tempfile.TemporaryDirectory(prefix=f".{destination.name}-migrate-",
                                     dir=destination.parent) as temporary:
        stage = Path(temporary) / "profile"
        prepare_copy(source, stage, destination, roots)
        # Publish only after the entire copy, path rewrites and DB checks succeed.
        if exists:
            suffix = datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + uuid.uuid4().hex[:8]
            backup = destination.with_name(f"{destination.name}.backup-{suffix}")
            destination.rename(backup)
        elif os.path.lexists(destination):
            raise ValueError("複製期間目標目錄出現，已中止。請確認 App 已完全結束。")
        try:
            stage.rename(destination)
        except OSError:
            if backup is not None:
                backup.rename(destination)
            raise
    print(f"遷移完成：{destination}")
    if backup:
        print(f"原目標備份：{backup}")
    print("可啟動新版 App。舊版 JSON 歷史會在登入後依原保留期限匯入 SQLite。")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, help="舊版資料目錄（自動偵測 native／Flatpak）")
    parser.add_argument("--destination", type=Path, help="新版資料目錄（預設 XDG_DATA_HOME/line-rust）")
    parser.add_argument("--flatpak", action="store_true", help="使用新版 Flatpak 的預設目標目錄")
    parser.add_argument("--replace", action="store_true", help="先備份既有新版目錄，再以舊版完整副本替換")
    parser.add_argument("--dry-run", action="store_true", help="只預覽來源、目標與替換計畫")
    args = parser.parse_args()
    os.umask(0o077)
    try:
        source, destination = choose_paths(args)
        migrate(source, destination, args.replace, args.dry_run)
    except (OSError, ValueError, sqlite3.Error, shutil.Error) as error:
        print(f"遷移失敗：{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
