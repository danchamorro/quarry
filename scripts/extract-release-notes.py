#!/usr/bin/env python3
"""Extract GitHub Release notes directly from CHANGELOG.md between tags or dates."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CHANGELOG = ROOT / "CHANGELOG.md"
DATE_HEADING_RE = re.compile(r"^## \[(\d{4}-\d{2}-\d{2})\]\s*$")
GROUP_HEADING_RE = re.compile(r"^### (Added|Changed|Removed|Fixed|Security)\s*$")
GROUP_ORDER = ("Added", "Changed", "Removed", "Fixed", "Security")


def previous_tag_for(repo_root: Path, current_tag: str | None) -> str | None:
    """Return the most recent `v*` tag before `current_tag` (or latest `v*` tag)."""
    result = subprocess.run(
        ["git", "-C", str(repo_root), "tag", "--list", "v*", "--sort=-creatordate"],
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    tags = [line.strip() for line in result.stdout.splitlines() if line.strip()]
    for tag in tags:
        if current_tag and tag == current_tag:
            continue
        return tag
    return None


def tag_commit_date(repo_root: Path, tag: str) -> str | None:
    """Return YYYY-MM-DD commit date for `tag`, or None if tag does not exist."""
    result = subprocess.run(
        ["git", "-C", str(repo_root), "log", "-1", "--format=%cs", tag],
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    date_str = result.stdout.strip()
    return date_str if re.fullmatch(r"\d{4}-\d{2}-\d{2}", date_str) else None


def extract_sections(
    changelog_text: str,
    *,
    since_date: str | None = None,
    include_since_date: bool = False,
) -> tuple[list[str], dict[str, list[str]]]:
    """Parse Keep a Changelog dated sections newer than `since_date`."""
    included_dates: list[str] = []
    grouped: dict[str, list[str]] = {group: [] for group in GROUP_ORDER}
    current_date: str | None = None
    in_range = False
    current_group: str | None = None

    for raw_line in changelog_text.splitlines():
        line = raw_line.rstrip()
        date_match = DATE_HEADING_RE.match(line)
        if date_match:
            current_date = date_match.group(1)
            current_group = None
            if since_date is None:
                in_range = True
            elif include_since_date:
                in_range = current_date >= since_date
            else:
                in_range = current_date > since_date
            if in_range:
                included_dates.append(current_date)
            continue

        if not in_range:
            continue

        group_match = GROUP_HEADING_RE.match(line)
        if group_match:
            current_group = group_match.group(1)
            continue

        if current_group is None:
            continue

        if line.startswith("- "):
            grouped[current_group].append(line)
        elif line.startswith("  ") and grouped[current_group]:
            grouped[current_group][-1] += " " + line.strip()

    return included_dates, grouped


def format_release_notes(
    changelog_text: str,
    *,
    tag: str | None = None,
    previous_tag: str | None = None,
    since_date: str | None = None,
    include_since_date: bool = False,
    repo: str = "danchamorro/quarry",
) -> str:
    """Format extracted CHANGELOG entries into GitHub Release Markdown."""
    dates, grouped = extract_sections(
        changelog_text,
        since_date=since_date,
        include_since_date=include_since_date,
    )

    lines: list[str] = []
    if tag and previous_tag:
        lines.append(f"## What's Changed Since `{previous_tag}`")
    elif tag:
        lines.append(f"## What's Changed in `{tag}`")
    else:
        lines.append("## What's Changed")

    if dates:
        if len(dates) == 1:
            lines.append(f"\n_Compiled from `CHANGELOG.md` ({dates[0]})._")
        else:
            lines.append(f"\n_Compiled from `CHANGELOG.md` ({dates[-1]} through {dates[0]})._")

    any_bullets = False
    for group in GROUP_ORDER:
        bullets = grouped[group]
        if not bullets:
            continue
        any_bullets = True
        lines.append(f"\n### {group}\n")
        lines.extend(bullets)

    if not any_bullets:
        lines.append("\nNo new `CHANGELOG.md` entries found in the selected range.")

    lines.append("\n---")
    lines.append("\n### Platform & Verification Notes")
    lines.append(
        "- **macOS (Apple Silicon `arm64`):** Signed with Developer ID, Apple-notarized, and stapled (`.dmg` and `.zip`)."
    )
    lines.append(
        "- **Linux & Omarchy (`x86_64` / `arm64`):** Build from source via [`docs/LINUX_AND_OMARCHY.md`](https://github.com/"
        + repo
        + "/blob/main/docs/LINUX_AND_OMARCHY.md) while multi-target Linux packaging completes for v1.0."
    )
    lines.append(
        "- **Verify downloads:** Check `SHA256SUMS-macos-arm64.txt` with `shasum -a 256 -c SHA256SUMS-macos-arm64.txt`."
    )
    if tag and previous_tag:
        lines.append(
            f"\n**Full Changelog:** https://github.com/{repo}/compare/{previous_tag}...{tag}"
        )

    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--changelog", type=Path, default=DEFAULT_CHANGELOG)
    parser.add_argument("--tag", help="Release tag being prepared (e.g. v0.2.0-beta.1)")
    parser.add_argument(
        "--since-tag",
        help="Previous release tag (auto-detected from Git tags when omitted)",
    )
    parser.add_argument(
        "--since-date",
        help="Explicit YYYY-MM-DD cutoff date (overrides --since-tag lookup)",
    )
    parser.add_argument(
        "--include-since-date",
        action="store_true",
        help="Include entries on --since-date itself instead of strictly after it",
    )
    parser.add_argument("--repo", default="danchamorro/quarry")
    parser.add_argument("--output", type=Path, help="Write Markdown output to a file")
    args = parser.parse_args(argv)

    previous_tag = args.since_tag or previous_tag_for(ROOT, args.tag)
    since_date = args.since_date
    if since_date is None and previous_tag:
        since_date = tag_commit_date(ROOT, previous_tag)

    text = args.changelog.read_text(encoding="utf-8")
    notes = format_release_notes(
        text,
        tag=args.tag,
        previous_tag=previous_tag,
        since_date=since_date,
        include_since_date=args.include_since_date,
        repo=args.repo,
    )
    if args.output:
        args.output.write_text(notes, encoding="utf-8")
    else:
        sys.stdout.write(notes)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
