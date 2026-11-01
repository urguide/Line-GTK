"""Verify profile migration using disposable data, including uncheckpointed WAL."""

from contextlib import closing, redirect_stdout
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "migrate-line-gtk.py"
spec = importlib.util.spec_from_file_location("migration", SCRIPT)
migration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(migration)


class MigrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.old = self.root / "line-gtk"
        self.new = self.root / "line-rust"
        (self.old / "cache/media").mkdir(parents=True)
        self.write_json("settings.json", {"theme": "dark", "cache_retention": "forever",
                                        "download_dir_image": str(self.old / "downloads")})

    def write_json(self, relative, data):
        path = self.old / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data), encoding="utf-8")

    def migrate(self, **kwargs):
        with redirect_stdout(io.StringIO()):
            migration.migrate(self.old, self.new, **kwargs)

    def snapshot(self, root):
        return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
                for p in root.rglob("*") if p.is_file()}

    def test_complete_profile_wal_paths_and_import_claims(self):
        media = self.old / "cache/media/123.json"
        # An attached JSON document must remain byte-for-byte unchanged.
        media.write_text(json.dumps({"filePath": str(media)}))
        avatar = self.old / "cache/avatars/user.jpg"
        avatar.parent.mkdir()
        avatar.write_bytes(b"avatar")
        row = {"id": "123", "text": str(media), "filePath": str(media),
               "rawMessage": {"text": str(media)},
               "quoted": {"imagePath": str(avatar)},
               "external": {"path": str(self.root / "line-gtk-other/photo.jpg")}}
        self.write_json("cache/messages/chat.json", {"at": 1234, "messages": [row]})
        self.write_json("stickers-index.json", [{"imagePath": str(avatar)}])
        self.write_json("linejs-storage.json", {"key": "opaque", "filePath": str(media)})
        (self.old / "auth-token.txt").write_text("test-token")
        (self.old / "auth-device.txt").write_text("test-device")
        (self.old / "cache/media/link").symlink_to(media)

        db_path = self.old / "cache/messages.sqlite3"
        with closing(sqlite3.connect(db_path)) as db:
            db.executescript("""
                PRAGMA journal_mode=WAL;
                PRAGMA wal_autocheckpoint=0;
                CREATE TABLE messages(account TEXT, chat TEXT, id TEXT, created INTEGER,
                    updated INTEGER, body TEXT, PRIMARY KEY(account,chat,id));
                CREATE TABLE pages(account TEXT, chat TEXT, boundary TEXT, updated INTEGER,
                    body TEXT, PRIMARY KEY(account,chat,boundary));
                CREATE TABLE catalogs(account TEXT, kind TEXT, updated INTEGER, body TEXT,
                    PRIMARY KEY(account,kind));
                CREATE TABLE migrations(path TEXT PRIMARY KEY, account TEXT);
                CREATE TABLE tombstones(account TEXT, id TEXT, PRIMARY KEY(account,id));
            """)
            for account in ("first", "second"):
                db.execute("INSERT INTO messages VALUES(?,?,?,?,?,?)",
                           (account, "chat", "123", 10, 20, json.dumps(row)))
            db.execute("INSERT INTO pages VALUES(?,?,?,?,?)",
                       ("first", "chat", "latest", 20, json.dumps({"messages": [row]})))
            db.execute("INSERT INTO catalogs VALUES(?,?,?,?)",
                       ("first", "list_chats", 20, json.dumps({"avatarPath": str(avatar)})))
            db.execute("INSERT INTO migrations VALUES(?,?)",
                       (str(self.old / "cache/messages/chat.json"), "first"))
            db.execute("INSERT INTO migrations VALUES(?,?)",
                       (str(self.old / "stickers-index.json"), "first"))
            db.execute("INSERT INTO tombstones VALUES('first','removed')")
            db.commit()
            self.assertGreater(Path(str(db_path) + "-wal").stat().st_size, 0)
            before = self.snapshot(self.old)
            self.migrate()
            self.assertEqual(self.snapshot(self.old), before)

        with closing(sqlite3.connect(self.new / "cache/messages.sqlite3")) as db:
            self.assertEqual(db.execute("PRAGMA integrity_check").fetchall(), [("ok",)])
            rows = db.execute("SELECT created, updated, body FROM messages").fetchall()
            self.assertEqual(len(rows), 2)
            for created, updated, body in rows:
                migrated = json.loads(body)
                self.assertEqual((created, updated), (10, 20))
                self.assertEqual(migrated["filePath"], str(self.new / "cache/media/123.json"))
                self.assertEqual(migrated["text"], row["text"])
                self.assertEqual(migrated["rawMessage"], row["rawMessage"])
                self.assertEqual(migrated["external"], row["external"])
                self.assertEqual(migrated["quoted"]["imagePath"], str(self.new / "cache/avatars/user.jpg"))
            claims = {p for p, in db.execute("SELECT path FROM migrations")}
            self.assertEqual(claims, {str(self.new / "cache/messages/chat.json"),
                                      str(self.new / "stickers-index.json")})
            self.assertEqual(db.execute("SELECT * FROM tombstones").fetchall(), [("first", "removed")])
            catalog = json.loads(db.execute("SELECT body FROM catalogs").fetchone()[0])
            self.assertEqual(catalog["avatarPath"], str(self.new / "cache/avatars/user.jpg"))
            page = json.loads(db.execute("SELECT body FROM pages").fetchone()[0])
            self.assertEqual(page["messages"][0]["filePath"], str(self.new / "cache/media/123.json"))

        self.assertEqual((self.new / "cache/media/123.json").read_bytes(), media.read_bytes())
        self.assertEqual((self.new / "cache/media/link").resolve(), self.new / "cache/media/123.json")
        for name in migration.AUTH_FILES:
            self.assertEqual((self.new / "protocol-state" / name).read_bytes(), (self.old / name).read_bytes())
        settings = json.loads((self.new / "settings.json").read_text())
        self.assertEqual(settings["download_dir_image"], str(self.new / "downloads"))
        self.assertEqual((self.new / "settings.json").stat().st_mode & 0o777, 0o600)
        legacy = json.loads((self.new / "cache/messages/chat.json").read_text())
        self.assertEqual(legacy["messages"][0]["filePath"], str(self.new / "cache/media/123.json"))

    def test_compatibility_symlink_becomes_independent_directory(self):
        self.new.symlink_to(self.old, target_is_directory=True)
        self.migrate()
        self.assertFalse(self.new.is_symlink())
        backups = list(self.root.glob("line-rust.backup-*"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].resolve(), self.old)
        (self.new / "settings.json").write_text("new settings")
        self.assertNotEqual((self.old / "settings.json").read_text(), "new settings")

    def test_existing_profile_requires_replace_and_is_backed_up(self):
        self.new.mkdir()
        (self.new / "settings.json").write_text("existing-new-profile")
        with self.assertRaises(ValueError):
            self.migrate()
        self.migrate(replace=True)
        backups = list(self.root.glob("line-rust.backup-*"))
        self.assertEqual(len(backups), 1)
        self.assertEqual((backups[0] / "settings.json").read_text(), "existing-new-profile")

    def test_invalid_database_leaves_destination_and_source_untouched(self):
        (self.old / "cache/messages.sqlite3").write_bytes(b"not a database")
        self.new.symlink_to(self.old, target_is_directory=True)
        before = self.snapshot(self.old)
        with self.assertRaises(sqlite3.DatabaseError):
            self.migrate()
        self.assertEqual(self.snapshot(self.old), before)
        self.assertTrue(self.new.is_symlink())
        self.assertEqual(list(self.root.glob("line-rust.backup-*")), [])
        self.assertEqual(list(self.root.glob(".line-rust-migrate-*")), [])

    def test_marker_prevents_resurrection_of_logged_out_credentials(self):
        (self.old / "protocol-state").mkdir()
        (self.old / "protocol-state/legacy-imported").write_text("1")
        (self.old / "auth-token.txt").write_text("stale-token")
        self.migrate()
        self.assertFalse((self.new / "protocol-state/auth-token.txt").exists())

    def test_existing_protocol_credentials_take_precedence(self):
        (self.old / "protocol-state").mkdir()
        (self.old / "protocol-state/auth-token.txt").write_text("current-token")
        (self.old / "auth-token.txt").write_text("stale-token")
        self.migrate()
        self.assertEqual((self.new / "protocol-state/auth-token.txt").read_text(), "current-token")

    def test_publish_failure_restores_original_symlink(self):
        self.new.symlink_to(self.old, target_is_directory=True)
        rename = Path.rename

        def fail_publish(path, target):
            if path.name == "profile":
                raise OSError("simulated rename failure")
            return rename(path, target)

        with patch.object(Path, "rename", fail_publish), self.assertRaises(OSError):
            self.migrate()
        self.assertTrue(self.new.is_symlink())
        self.assertEqual(self.new.resolve(), self.old)

    def test_dry_run_cli_uses_xdg_without_writing(self):
        before = self.snapshot(self.root)
        result = subprocess.run([sys.executable, str(SCRIPT), "--dry-run"],
                                env=dict(os.environ, XDG_DATA_HOME=str(self.root), HOME=str(self.root)),
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(str(self.new), result.stdout)
        self.assertFalse(self.new.exists())
        self.assertEqual(self.snapshot(self.root), before)

    def test_nested_paths_rejected(self):
        with self.assertRaises(ValueError):
            migration.migrate(self.old, self.old / "new")

    def test_flatpak_destination_and_external_relative_link(self):
        external = self.root / "external-image.jpg"
        external.write_bytes(b"external")
        (self.old / "external-link").symlink_to("../external-image.jpg")
        destination = self.root / ".var/app/io.github.urguide.LineRust/data/line-rust"
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--flatpak", "--source", str(self.old)],
            env=dict(os.environ, HOME=str(self.root)), text=True, capture_output=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((destination / "settings.json").is_file())
        self.assertEqual((destination / "external-link").resolve(), external)

    def test_sqlite_symlink_does_not_modify_external_database(self):
        external = self.root / "external.sqlite3"
        with closing(sqlite3.connect(external)) as db:
            db.execute("CREATE TABLE data(value TEXT)")
            db.commit()
        before = external.read_bytes()
        (self.old / "cache/messages.sqlite3").symlink_to(external)
        with self.assertRaises(ValueError):
            self.migrate()
        self.assertEqual(external.read_bytes(), before)
        self.assertFalse(self.new.exists())


if __name__ == "__main__":
    unittest.main()
