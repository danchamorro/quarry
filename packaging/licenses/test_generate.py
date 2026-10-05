"""Offline packaging guard regressions. Run with python3 packaging/licenses/test_generate.py."""

import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class NoticeChecks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        metadata = json.loads(subprocess.check_output([
            "cargo", "metadata", "--no-deps", "--format-version", "1",
            "--offline", "--locked",
        ], cwd=ROOT, text=True))
        cls.target_sources = [
            Path(target["src_path"]).relative_to(ROOT)
            for package in metadata["packages"] for target in package["targets"]
        ]

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="quarry-notice-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        manifest = json.loads((ROOT / "packaging/licenses/manifest.json").read_text())
        files = list(manifest["inputs"]) + [
            "packaging/licenses/manifest.json",
            "packaging/licenses/reviewed.sha256",
            "packaging/licenses/THIRD_PARTY_NOTICES.html",
        ]
        for relative in files:
            destination = self.root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, destination)
        for relative in self.target_sources:
            destination = self.root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text("// Metadata-only fixture target.\n")

    def run_check(self, *args, env=None):
        return subprocess.run(
            [sys.executable, str(self.root / "packaging/licenses/generate.py"), *args],
            text=True, capture_output=True, check=False, env=env,
        )

    def test_current_inventory_passes_offline(self):
        env = dict(os.environ, CARGO_HOME=str(self.root / "empty-cargo-cache"))
        result = self.run_check("--check", "--target", "aarch64-apple-darwin", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Review is checked separately", result.stdout)

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

    def test_unrelated_local_app_does_not_change_inventory(self):
        path = self.root / "apps/local-experiment/Cargo.toml"
        path.parent.mkdir(parents=True)
        path.write_text("This is not a workspace manifest and must not be read.\n")
        result = self.run_check("--release-check")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_member_globs_and_exclusions_select_only_declared_manifests(self):
        generator = runpy.run_path(str(self.root / "packaging/licenses/generate.py"))
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["fixtures/*"]\nexclude = ["fixtures/excluded"]\n')
        for name in ["included", "excluded"]:
            self.make_package("fixtures/" + name, name)
        selected = {str(p.relative_to(self.root.resolve())) for p in generator["workspace_manifests"]()}
        self.assertEqual(selected, {"fixtures/included/Cargo.toml"})

    def make_package(self, relative, name, extra=""):
        directory = self.root / relative
        (directory / "src").mkdir(parents=True)
        (directory / "src/lib.rs").write_text("// Metadata-only fixture target.\n")
        manifest = directory / "Cargo.toml"
        manifest.write_text(f'[package]\nname = "{name}"\nversion = "0.1.0"\n' + extra)
        return manifest

    def test_implicit_path_member_change_invalidates_review_offline(self):
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = ["fixtures/app"]\n')
        self.make_package("fixtures/app", "app",
                          '[dependencies]\nhelper = { path = "../../support/helper" }\n')
        helper = self.make_package("support/helper", "helper", 'license = "MIT"\n')
        generator = runpy.run_path(str(self.root / "packaging/licenses/generate.py"))
        manifest_path = self.root / "packaging/licenses/manifest.json"
        manifest = json.loads(manifest_path.read_text())
        manifest["inputs"] = generator["inputs"]()
        self.assertIn("support/helper/Cargo.toml", manifest["inputs"])
        manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        (self.root / "packaging/licenses/reviewed.sha256").write_text(
            generator["digest"](manifest_path.read_bytes()) + "\n")
        env = dict(os.environ, CARGO_HOME=str(self.root / "empty-cargo-cache"))
        result = self.run_check("--release-check", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        helper.write_text(helper.read_text().replace('license = "MIT"', 'license = "Apache-2.0"'))
        for mode in ["--check", "--release-check"]:
            result = self.run_check(mode, env=env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("stale or modified", result.stderr)

    def test_missing_workspace_member_fails_explicitly(self):
        (self.root / "apps/quarry-egui/Cargo.toml").unlink()
        result = self.run_check("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cannot read Cargo workspace", result.stderr)

    def test_other_target_cannot_use_arm_inventory(self):
        result = self.run_check("--check", "--target", "x86_64-apple-darwin")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No notice inventory", result.stderr)

    def test_other_compiler_cannot_use_runtime_inventory(self):
        result = self.run_check("--check", "--rust-release", "1.89.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No runtime notice inventory", result.stderr)

    def test_runtime_notice_change_rejects_inventory(self):
        path = self.root / "packaging/licenses/rust/COPYRIGHT-library.html"
        path.write_bytes(path.read_bytes() + b"changed runtime notice")
        result = self.run_check("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stale or modified", result.stderr)

    def test_reviewed_inventory_passes_review_check(self):
        result = self.run_check("--release-check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Reviewed notice inventory matches", result.stdout)

    def test_missing_review_rejects_inventory(self):
        (self.root / "packaging/licenses/reviewed.sha256").unlink()
        result = self.run_check("--release-check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Notice inventory review required", result.stderr)

    def test_refreshed_inventory_does_not_renew_review(self):
        path = self.root / "Cargo.lock"
        path.write_bytes(path.read_bytes() + b"\n# new input\n")
        generator = runpy.run_path(str(self.root / "packaging/licenses/generate.py"))
        manifest_path = self.root / "packaging/licenses/manifest.json"
        manifest = json.loads(manifest_path.read_text())
        manifest["inputs"] = generator["inputs"]()
        manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        result = self.run_check("--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_check("--release-check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Notice inventory review required", result.stderr)

    def test_selected_alternative_must_be_offered(self):
        generator = runpy.run_path(str(self.root / "packaging/licenses/generate.py"))
        notice = {"packages": ["example@1"], "source": "https://example.invalid/Zlib",
                  "text": "Zlib terms", "sha256": generator["digest"](b"Zlib terms"),
                  "selected_license": "Zlib"}
        (self.root / "packaging/licenses/supplemental.json").write_text(
            json.dumps({"notices": [notice], "assets": []}))
        package = {"name": "example", "version": "1", "license": "MIT OR Zlib", "source": "registry"}
        raw = {"crates": [{"package": package}], "licenses": [
            {"id": "MIT", "source_path": None, "used_by": [{"crate": package}]}]}
        rendered = generator["render"](raw)
        self.assertIn(b"Selected license: Zlib", rendered)
        self.assertNotIn(b"example@1: MIT", rendered)
        package["license"] = "MIT"
        with self.assertRaisesRegex(ValueError, "Selected license is not offered"):
            generator["render"](raw)

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
