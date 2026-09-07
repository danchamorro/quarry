"""Offline packaging guard regressions. Run with python3 packaging/licenses/test_generate.py."""

import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class NoticeChecks(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="quarry-notice-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        manifest = json.loads((ROOT / "packaging/licenses/manifest.json").read_text())
        files = list(manifest["inputs"]) + [
            "packaging/licenses/manifest.json",
            "packaging/licenses/THIRD_PARTY_NOTICES.html",
        ]
        for relative in files:
            destination = self.root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, destination)

    def run_check(self, *args):
        return subprocess.run(
            [sys.executable, str(self.root / "packaging/licenses/generate.py"), *args],
            text=True, capture_output=True, check=False,
        )

    def test_current_inventory_passes_offline(self):
        result = self.run_check("--check", "--target", "aarch64-apple-darwin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Legal completeness remains unresolved", result.stdout)

    def test_dependency_input_change_rejects_stale_inventory(self):
        for relative in ("Cargo.lock", "apps/quarry-egui/Cargo.toml"):
            with self.subTest(relative=relative):
                path = self.root / relative
                original = path.read_bytes()
                path.write_bytes(original + b"\n# changed dependency input\n")
                result = self.run_check("--check")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("stale or modified", result.stderr)
                path.write_bytes(original)

    def test_artifact_tampering_rejects_inventory(self):
        path = self.root / "packaging/licenses/THIRD_PARTY_NOTICES.html"
        path.write_text("Truncated notices\n")
        result = self.run_check("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stale or modified", result.stderr)

    def test_other_target_cannot_use_arm_inventory(self):
        result = self.run_check("--check", "--target", "x86_64-apple-darwin")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No notice inventory", result.stderr)

    def test_fresh_inventory_does_not_clear_release_gate(self):
        result = self.run_check("--release-check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Release notice audit is unresolved", result.stderr)

    def test_url_notice_text_hash_is_checked_before_rendering_and_offline(self):
        path = self.root / "packaging/licenses/supplemental.json"
        data = json.loads(path.read_text())
        notice = next(n for n in data["notices"] if n["source"].startswith("https://"))
        self.assertNotIn("crate_file", notice)
        self.assertNotIn("path", notice)
        notice["text"] += "Changed notice text\n"
        path.write_text(json.dumps(data))
        generator = runpy.run_path(str(self.root / "packaging/licenses/generate.py"))
        with self.assertRaisesRegex(ValueError, "Supplemental notice hash mismatch"):
            generator["render"]({"crates": [], "licenses": []})
        result = self.run_check("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Supplemental notice hash mismatch", result.stderr)

    def test_font_size_and_hash_are_verified_during_rendering(self):
        generator = runpy.run_path(str(self.root / "packaging/licenses/generate.py"))
        source = self.root / "font.ttf"
        source.write_bytes(b"test font bytes")
        asset = {"package": "font@1", "path": source.name,
                 "bytes": source.stat().st_size, "sha256": generator["digest"](source.read_bytes())}
        raw = {"crates": [{"package": {
            "name": "font", "version": "1", "license": "MIT",
            "manifest_path": str(self.root / "Cargo.toml"),
        }}], "licenses": []}
        path = self.root / "packaging/licenses/supplemental.json"
        path.write_text(json.dumps({"notices": [], "assets": [asset]}))
        self.assertIn(b"font@1/font.ttf: 15 bytes", generator["render"](raw))
        for field, bad_value, error in (
            ("bytes", 16, "size mismatch"),
            ("sha256", "0" * 64, "supplement changed"),
        ):
            with self.subTest(field=field):
                changed = {**asset, field: bad_value}
                path.write_text(json.dumps({"notices": [], "assets": [changed]}))
                with self.assertRaisesRegex(ValueError, error):
                    generator["render"](raw)


if __name__ == "__main__":
    unittest.main()
