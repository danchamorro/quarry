#!/usr/bin/env python3
"""Synthetic fixtures and independent byte oracles for date/time sort review.

Run with a new output directory. Python's datetime supplies chronology; no
Quarry parser or sort code is used. Large expected outputs are represented by
SHA-256 hashes computed in bounded batches rather than materialized in RAM.
"""

import argparse
import csv
import datetime as dt
import hashlib
import io
import json
from pathlib import Path


def record(fields):
    """Encode a complete UTF-8 CSV record, including quoting and embedded newlines."""
    stream = io.StringIO(newline="")
    csv.writer(stream, lineterminator="\n").writerow(fields)
    return stream.getvalue().encode("utf-8")


def digest(path):
    """Hash source bytes in fixed-size blocks without loading the fixture into RAM."""
    result = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def small(directory):
    """Write all five grammars and stable ascending/descending datetime oracles."""
    dates = [dt.datetime(2026, 3, 4), dt.datetime(2026, 4, 3),
             dt.datetime(2000, 2, 29), dt.datetime(1900, 3, 1),
             dt.datetime(1, 1, 1), dt.datetime(9999, 12, 31),
             dt.datetime(2026, 3, 4), None]
    for name in ["ymd", "mdy", "dmy", "datetime", "rfc3339"]:
        rows = []
        for index, value in enumerate(dates):
            if value is None:
                text = " \t"
            elif name == "ymd":
                text = value.date().isoformat()
            elif name == "mdy":
                text = f"{value.month:02}/{value.day:02}/{value.year:04}"
            elif name == "dmy":
                text = f"{value.day:02}/{value.month:02}/{value.year:04}"
            else:
                text = value.isoformat() + ("Z" if name == "rfc3339" else "")
            rows.append(record([text, str(index), '猫, "quoted"\nnext line']))
        header = b"date,id,note\n"
        (directory / f"{name}.csv").write_bytes(header + b"".join(rows))
        for order in ["asc", "desc"]:
            positions = sorted(range(len(rows)), key=lambda i: (dates[i] is not None, dates[i] or dt.datetime.min), reverse=order == "desc")
            (directory / f"{name}-{order}-expected.csv").write_bytes(header + b"".join(rows[i] for i in positions))
    (directory / "invalid.csv").write_bytes(b"date,id\n2000-02-29,valid\n1900-02-29,invalid\n")


def large(directory, count):
    """Generate repeated UTC keys and hash whole expected outputs in bounded batches."""
    utc = dt.timezone.utc
    base = dt.datetime(2025, 12, 31, 23, 0, tzinfo=utc)
    templates, keys = [], []
    for residue in range(64):
        variant = (residue * 17) % 64
        bucket = variant // 2
        instant = base + dt.timedelta(seconds=bucket * 1379, microseconds=bucket * 31)
        local = instant.astimezone(dt.timezone(dt.timedelta(hours=2 if variant % 2 else -5)))
        text = local.isoformat(timespec="microseconds") if bucket else ""
        templates.append(record([text, "IDPLACEHOLDER", 'α, "quoted"\n' + "x" * 170]))
        keys.append(instant if bucket else dt.datetime.min.replace(tzinfo=utc))
    header = b"date,id,note\n"

    def row(index):
        """Keep a source ordinal in each full row so equal-key stability is observable."""
        return templates[index % 64].replace(b"IDPLACEHOLDER", f"{index:010}".encode())

    path = directory / "large.csv"
    with path.open("xb") as output:
        output.write(header)
        for start in range(0, count, 4096):
            output.write(b"".join(row(i) for i in range(start, min(start + 4096, count))))
    metadata = {"rows": count, "bytes": path.stat().st_size, "source_sha256": digest(path)}
    for order in ["asc", "desc"]:
        expected = hashlib.sha256(header)
        # Merge equal-key residues by original ordinal, preserving stable ties.
        for key in sorted(set(keys), reverse=order == "desc"):
            residues = [i for i, candidate in enumerate(keys) if candidate == key]
            batch = []
            for start in range(0, count, 64):
                for residue in residues:
                    index = start + residue
                    if index < count:
                        batch.append(row(index))
                if len(batch) >= 4096:
                    expected.update(b"".join(batch))
                    batch.clear()
            expected.update(b"".join(batch))
        metadata[f"{order}_sha256"] = expected.hexdigest()
    (directory / "large-oracle.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(json.dumps(metadata))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--large-rows", type=int, default=0)
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=False)
    small(args.directory)
    if args.large_rows:
        if args.large_rows < 1:
            parser.error("--large-rows must be positive")
        large(args.directory, args.large_rows)
