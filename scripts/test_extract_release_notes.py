#!/usr/bin/env python3
"""Unit tests for scripts/extract-release-notes.py."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True

SCRIPT_PATH = Path(__file__).resolve().parent / "extract-release-notes.py"
SPEC = importlib.util.spec_from_file_location("extract_release_notes", SCRIPT_PATH)
assert SPEC and SPEC.loader
MOD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MOD)

SAMPLE_CHANGELOG = """# Changelog

## [2026-11-26]

### Added

- Added adaptive word wrap. (#221)

### Changed

- Deduplicated CI runs. (#222)

## [2026-11-25]

### Added

- Added row bookmarks. (#215)

### Fixed

- Fixed modal confirm clicks
  on wrapped lines. (#216)

## [2026-11-21]

### Added

- Old beta 3 feature. (#147)
"""


class ExtractReleaseNotesTests(unittest.TestCase):
    def test_extracts_strictly_after_since_date_and_merges_groups(self) -> None:
        dates, grouped = MOD.extract_sections(SAMPLE_CHANGELOG, since_date="2026-11-21")
        self.assertEqual(dates, ["2026-11-26", "2026-11-25"])
        self.assertEqual(
            grouped["Added"],
            ["- Added adaptive word wrap. (#221)", "- Added row bookmarks. (#215)"],
        )
        self.assertEqual(grouped["Changed"], ["- Deduplicated CI runs. (#222)"])
        self.assertEqual(
            grouped["Fixed"],
            ["- Fixed modal confirm clicks on wrapped lines. (#216)"],
        )
        self.assertEqual(grouped["Removed"], [])

    def test_include_since_date_flag_includes_boundary_date(self) -> None:
        dates, grouped = MOD.extract_sections(
            SAMPLE_CHANGELOG, since_date="2026-11-21", include_since_date=True
        )
        self.assertEqual(dates, ["2026-11-26", "2026-11-25", "2026-11-21"])
        self.assertIn("- Old beta 3 feature. (#147)", grouped["Added"])

    def test_diff_since_previous_changelog_handles_same_day_releases(self) -> None:
        previous = """# Changelog

## [2026-11-26]

### Added

- Earlier same-day entry. (#220)
"""
        current = """# Changelog

## [2026-11-26]

### Added

- Newer same-day entry. (#224)
- Earlier same-day entry. (#220)
"""
        notes = MOD.format_release_notes(
            current,
            tag="v0.2.0-beta.2",
            previous_tag="v0.2.0-beta.1",
            previous_changelog_text=previous,
        )
        self.assertIn("- Newer same-day entry. (#224)", notes)
        self.assertNotIn("Earlier same-day entry", notes)
        self.assertIn("(2026-11-26)", notes)

    def test_previous_tag_for_selects_ancestor_not_newer_tag(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quarry-tag-test-") as tmp:
            repo = Path(tmp)
            env = {
                "GIT_AUTHOR_NAME": "Test",
                "GIT_AUTHOR_EMAIL": "test@example.com",
                "GIT_COMMITTER_NAME": "Test",
                "GIT_COMMITTER_EMAIL": "test@example.com",
            }
            subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
            for idx, tag in enumerate(["v0.1.0", "v0.2.0", "v0.3.0"], start=1):
                (repo / "f.txt").write_text(f"{idx}\n", encoding="utf-8")
                subprocess.run(["git", "add", "f.txt"], cwd=repo, check=True)
                subprocess.run(
                    ["git", "commit", "-q", "-m", f"commit {tag}"],
                    cwd=repo,
                    env=env,
                    check=True,
                )
                subprocess.run(["git", "tag", tag], cwd=repo, env=env, check=True)

            self.assertEqual(MOD.previous_tag_for(repo, "v0.2.0"), "v0.1.0")
            self.assertEqual(MOD.previous_tag_for(repo, "v0.3.0"), "v0.2.0")
            self.assertIsNone(MOD.previous_tag_for(repo, "v0.1.0"))

    def test_format_release_notes_includes_compare_link_and_groups(self) -> None:
        notes = MOD.format_release_notes(
            SAMPLE_CHANGELOG,
            tag="v0.2.0-beta.1",
            previous_tag="v0.1.0-beta.3",
            since_date="2026-11-21",
        )
        self.assertIn("## What's Changed Since `v0.1.0-beta.3`", notes)
        self.assertIn("2026-11-25 through 2026-11-26", notes)
        self.assertIn("### Added", notes)
        self.assertIn("### Changed", notes)
        self.assertIn("### Fixed", notes)
        self.assertNotIn("Old beta 3 feature", notes)
        self.assertIn(
            "https://github.com/danchamorro/quarry/compare/v0.1.0-beta.3...v0.2.0-beta.1",
            notes,
        )


if __name__ == "__main__":
    unittest.main()
