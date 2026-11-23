#!/usr/bin/env python3
"""Deterministic synthetic 200-file corpus generator for Quarry Phase 4A/4B verification.

Generates a reproducible mixed corpus (clean CSVs, dirty-edit candidates, active-filter
candidates, and gzip-compressed `.csv.gz` files) and writes a JSON manifest declaring
file sizes, dirty overlay targets, gzip entries, filter queries, and window count.

By default writes to gitignored `fixtures/generated/phase4a-200-corpus/`.
"""

from __future__ import annotations

import argparse
import gzip
import json
from pathlib import Path


def build_csv_bytes(file_index: int, rows: int, payload_width: int) -> bytes:
    lines = ["id,category,region,amount,notes\n"]
    categories = ("alpha", "beta", "gamma", "delta")
    regions = ("north", "south", "east", "west")
    pad = f"p{file_index:03d}_" + ("x" * max(0, payload_width - 5))
    for row in range(1, rows + 1):
        cat = categories[(file_index + row) % len(categories)]
        reg = regions[(file_index * 3 + row) % len(regions)]
        amount = (file_index * 1000 + row * 17) % 100000
        lines.append(f"{row},{cat},{reg},{amount},{pad}_{row}\n")
    return "".join(lines).encode("utf-8")


def generate_corpus(
    output_dir: Path,
    total_files: int,
    gzip_count: int,
    dirty_count: int,
    filtered_count: int,
    window_count: int,
    rows_per_file: int,
    payload_width: int,
) -> dict:
    output_dir.mkdir(parents=True, exist_ok=True)
    files = []
    plain_count = total_files - gzip_count
    for idx in range(total_files):
        is_gzip = idx >= plain_count
        # Deterministic small/medium variation across files while staying fast to generate.
        rows = rows_per_file + (idx % 5) * 25
        raw = build_csv_bytes(idx, rows, payload_width)
        role = "clean"
        dirty_overlay_cells = 0
        filter_rule = None
        if is_gzip:
            name = f"compressed_{idx:03d}.csv.gz"
            path = output_dir / name
            with path.open("wb") as raw_fh:
                with gzip.GzipFile(
                    filename="", mode="wb", compresslevel=1, fileobj=raw_fh, mtime=0
                ) as fh:
                    fh.write(raw)
            role = "gzip_complete" if (idx - plain_count) % 2 == 0 else "gzip_preview"
        else:
            name = f"table_{idx:03d}.csv"
            path = output_dir / name
            path.write_bytes(raw)
            if idx < dirty_count:
                role = "dirty"
                dirty_overlay_cells = 16
            elif idx < dirty_count + filtered_count:
                role = "filtered"
                filter_rule = {"column": 1, "operator": "equals", "value": "alpha"}

        files.append(
            {
                "index": idx,
                "name": name,
                "path": str(path),
                "window": (idx % window_count) + 1,
                "role": role,
                "rows": rows,
                "uncompressed_bytes": len(raw),
                "disk_bytes": path.stat().st_size,
                "dirty_overlay_cells": dirty_overlay_cells,
                "filter_rule": filter_rule,
            }
        )

    manifest = {
        "schema_version": 1,
        "seed": "quarry-phase4-200-file-v1",
        "total_files": total_files,
        "plain_files": plain_count,
        "gzip_files": gzip_count,
        "dirty_files": dirty_count,
        "filtered_files": filtered_count,
        "window_count": window_count,
        "total_disk_bytes": sum(item["disk_bytes"] for item in files),
        "files": files,
    }
    manifest_path = output_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("fixtures/generated/phase4a-200-corpus"),
        help="Destination directory (default: fixtures/generated/phase4a-200-corpus)",
    )
    parser.add_argument("--total-files", type=int, default=200)
    parser.add_argument("--gzip-count", type=int, default=6)
    parser.add_argument("--dirty-count", type=int, default=6)
    parser.add_argument("--filtered-count", type=int, default=6)
    parser.add_argument("--window-count", type=int, default=4)
    parser.add_argument("--rows-per-file", type=int, default=400)
    parser.add_argument("--payload-width", type=int, default=32)
    args = parser.parse_args()

    manifest = generate_corpus(
        output_dir=args.output_dir,
        total_files=args.total_files,
        gzip_count=args.gzip_count,
        dirty_count=args.dirty_count,
        filtered_count=args.filtered_count,
        window_count=args.window_count,
        rows_per_file=args.rows_per_file,
        payload_width=args.payload_width,
    )
    print(
        f"Generated {manifest['total_files']} files "
        f"({manifest['plain_files']} plain, {manifest['gzip_files']} gzip, "
        f"{manifest['dirty_files']} dirty, {manifest['filtered_files']} filtered, "
        f"{manifest['window_count']} windows, {manifest['total_disk_bytes']} bytes) "
        f"at {args.output_dir}"
    )


if __name__ == "__main__":
    main()
