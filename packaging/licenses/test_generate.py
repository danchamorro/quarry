"""Offline packaging guard regressions. Run with python3 packaging/licenses/test_generate.py."""

import json
from pathlib import Path
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


if __name__ == "__main__":
    unittest.main()
