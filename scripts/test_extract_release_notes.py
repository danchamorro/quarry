#!/usr/bin/env python3
"""Unit tests for scripts/extract-release-notes.py."""

from __future__ import annotations

import importlib.util
import sys
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
