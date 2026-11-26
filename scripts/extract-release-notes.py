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


def ref_exists(repo_root: Path, ref: str) -> bool:
    result = subprocess.run(
        ["git", "-C", str(repo_root), "rev-parse", "--verify", f"{ref}^{{commit}}"],
        text=True,
        capture_output=True,
        check=False,
    )
    return result.returncode == 0


def previous_tag_for(repo_root: Path, current_tag: str | None) -> str | None:
    """Return the nearest reachable predecessor `v*` tag before `current_tag` (or HEAD)."""
    target_ref = current_tag if (current_tag and ref_exists(repo_root, current_tag)) else "HEAD"
    parent_ref = f"{target_ref}^" if ref_exists(repo_root, f"{target_ref}^") else target_ref
    described = subprocess.run(
        ["git", "-C", str(repo_root), "describe", "--tags", "--abbrev=0", "--match", "v*", parent_ref],
        text=True,
        capture_output=True,
        check=False,
    )
    if described.returncode == 0:
        candidate = described.stdout.strip()
        if candidate and candidate != current_tag:
            return candidate

    merged = subprocess.run(
        [
            "git",
            "-C",
            str(repo_root),
            "tag",
            "--merged",
            parent_ref,
            "--list",
            "v*",
            "--sort=-creatordate",
        ],
        text=True,
        capture_output=True,
        check=False,
    )
    if merged.returncode != 0:
        return None
    for line in merged.stdout.splitlines():
        tag = line.strip()
        if tag and tag != current_tag:
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


def read_changelog_at_ref(repo_root: Path, ref: str, rel_path: str = "CHANGELOG.md") -> str | None:
    """Read `CHANGELOG.md` from a Git ref.

    Returns file contents if present at `ref`, `""` if `ref` exists and `rel_path`
    is confirmed absent at `ref`, or `None` if `ref` itself does not exist.
    Raises `RuntimeError` if Git fails while inspecting an existing ref.
    """
    if not ref_exists(repo_root, ref):
        return None

    ls_tree = subprocess.run(
        ["git", "-C", str(repo_root), "ls-tree", "--name-only", ref, "--", rel_path],
        text=True,
        capture_output=True,
        check=False,
    )
    if ls_tree.returncode != 0:
        raise RuntimeError(
            f"git ls-tree failed for {ref}:{rel_path}: {ls_tree.stderr.strip()}"
        )
    if not ls_tree.stdout.strip():
        return ""

    show = subprocess.run(
        ["git", "-C", str(repo_root), "show", f"{ref}:{rel_path}"],
        text=True,
        capture_output=True,
        check=False,
    )
    if show.returncode != 0:
        raise RuntimeError(
            f"git show failed for {ref}:{rel_path}: {show.stderr.strip()}"
        )
    return show.stdout


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


def extract_diff_since_changelog(
    current_text: str,
    previous_text: str,
) -> tuple[list[str], dict[str, list[str]]]:
    """Extract bullets added to `current_text` relative to `previous_text`, even on the same date."""
    _, prev_grouped = extract_sections(previous_text)
    prev_bullets: dict[str, set[str]] = {
        group: set(bullets) for group, bullets in prev_grouped.items()
    }

    included_dates: list[str] = []
    grouped: dict[str, list[str]] = {group: [] for group in GROUP_ORDER}
    current_date: str | None = None
    current_group: str | None = None
    pending_bullets: list[tuple[str, str, str]] = []

    for raw_line in current_text.splitlines():
        line = raw_line.rstrip()
        date_match = DATE_HEADING_RE.match(line)
        if date_match:
            current_date = date_match.group(1)
            current_group = None
            continue
        group_match = GROUP_HEADING_RE.match(line)
        if group_match:
            current_group = group_match.group(1)
            continue
        if current_date is None or current_group is None:
            continue
        if line.startswith("- "):
            pending_bullets.append((current_date, current_group, line))
        elif line.startswith("  ") and pending_bullets:
            d, g, prev_line = pending_bullets[-1]
            pending_bullets[-1] = (d, g, prev_line + " " + line.strip())

    seen_dates: set[str] = set()
    for date_str, group, bullet in pending_bullets:
        if bullet in prev_bullets[group]:
            continue
        grouped[group].append(bullet)
        if date_str not in seen_dates:
            seen_dates.add(date_str)
            included_dates.append(date_str)

    return included_dates, grouped


def format_release_notes(
    changelog_text: str,
    *,
    tag: str | None = None,
    previous_tag: str | None = None,
    previous_changelog_text: str | None = None,
    since_date: str | None = None,
    include_since_date: bool = False,
    repo: str = "danchamorro/quarry",
) -> str:
    """Format extracted CHANGELOG entries into GitHub Release Markdown."""
    if previous_changelog_text is not None and since_date is None:
        dates, grouped = extract_diff_since_changelog(changelog_text, previous_changelog_text)
    else:
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
        help="Previous release tag (auto-detected from Git ancestry when omitted)",
    )
    parser.add_argument(
        "--since-date",
        help="Explicit YYYY-MM-DD cutoff date (overrides tag-based changelog diff)",
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
    previous_changelog_text = None
    if since_date is None and previous_tag:
        previous_changelog_text = read_changelog_at_ref(ROOT, previous_tag)
        if previous_changelog_text is None:
            since_date = tag_commit_date(ROOT, previous_tag)

    text = args.changelog.read_text(encoding="utf-8")
    notes = format_release_notes(
        text,
        tag=args.tag,
        previous_tag=previous_tag,
        previous_changelog_text=previous_changelog_text,
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
