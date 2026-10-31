"""Exercise release versioning and source/tag identity in disposable Git repos."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
import xml.etree.ElementTree as ET


SCRIPT = Path(__file__).resolve().parents[1] / "prepare-release.py"
spec = importlib.util.spec_from_file_location("prepare_release", SCRIPT)
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-b", "master")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.invalid")
        (self.repo / "Cargo.toml").write_text('[package]\nname = "line-gtk"\nversion = "0.1.2"\n')
        (self.repo / "Cargo.lock").write_text(
            'version = 4\n[[package]]\nname = "line-gtk"\nversion = "0.1.2"\n'
            '[[package]]\nname = "other"\nversion = "9.0.0"\n'
        )
        self.metadata = self.repo / "packaging/flatpak/dev.linegtk.LineGtk.metainfo.xml"
        self.metadata.parent.mkdir(parents=True)
        self.metadata.write_text('<component><releases>\n</releases></component>\n')
        self.commit("chore: initial")

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.repo, text=True,
                                       stderr=subprocess.PIPE).strip()

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def prepare(self, ref_type="branch", ref_name="master", success=True):
        output = self.root / "outputs"
        output.write_text("")
        env = dict(os.environ, GITHUB_REF_TYPE=ref_type, GITHUB_REF_NAME=ref_name,
                   GITHUB_OUTPUT=str(output), RUNNER_TEMP=str(self.root))
        result = subprocess.run(["python3", str(SCRIPT)], cwd=self.repo, env=env,
                                text=True, capture_output=True)
        if not success:
            self.assertNotEqual(result.returncode, 0)
            return
        self.assertEqual(result.returncode, 0, result.stderr)
        return dict(line.split("=", 1) for line in output.read_text().splitlines())

    def test_bump_rules(self):
        for message, expected in [
            ("fix: bug", "0.1.3"), ("update readme", "0.1.3"),
            ("feat(ui): add button", "0.2.0"), ("fix!: incompatible", "1.0.0"),
            ("feat: add\n\nBREAKING CHANGE: incompatible", "1.0.0"),
            ("fix: change\n\nBREAKING-CHANGE: incompatible", "1.0.0"),
        ]:
            with self.subTest(message=message):
                self.assertEqual(release.next_version((0, 1, 2), message), expected)

    def test_versioned_commit_bundle_and_retry(self):
        source = self.git("rev-parse", "HEAD")
        first = self.prepare()
        self.assertEqual(first["version"], "0.1.3")
        self.assertEqual(self.git("rev-parse", "master"), source)
        self.assertEqual(self.git("rev-parse", "v0.1.3^"), source)
        self.assertEqual(first["commit"], self.git("rev-parse", "v0.1.3"))
        self.assertEqual(tomllib.loads((self.repo / "Cargo.toml").read_text())["package"]["version"], "0.1.3")
        packages = tomllib.loads((self.repo / "Cargo.lock").read_text())["package"]
        self.assertEqual([p["version"] for p in packages], ["0.1.3", "9.0.0"])
        self.assertEqual(ET.parse(self.metadata).find("releases/release").get("version"), "0.1.3")
        clone = self.root / "clone"
        subprocess.run(["git", "clone", str(self.root / "release-source.bundle"), str(clone)],
                       check=True, capture_output=True)
        bundled = subprocess.check_output(["git", "rev-parse", "v0.1.3"], cwd=clone, text=True).strip()
        self.assertEqual(bundled, first["commit"])
        self.git("reset", "--hard", source)
        self.assertEqual(self.prepare(), first)
        # Without a published tag, rerunning produces the identical commit too.
        self.git("tag", "-d", first["tag"])
        self.assertEqual(self.prepare(), first)

    def test_next_push_uses_only_new_commits(self):
        self.commit("feat: feature")
        source = self.git("rev-parse", "HEAD")
        self.assertEqual(self.prepare()["version"], "0.2.0")
        self.git("reset", "--hard", source)
        self.commit("fix: next")
        self.assertEqual(self.prepare()["version"], "0.2.1")

    def test_explicit_tags(self):
        self.git("tag", "v0.1.2")
        self.assertEqual(self.prepare("tag", "v0.1.2")["commit"], self.git("rev-parse", "HEAD"))
        self.prepare("tag", "v0.9.0", success=False)


if __name__ == "__main__":
    unittest.main()
