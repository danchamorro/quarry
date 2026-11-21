#!/usr/bin/env python3
"""Generate a deterministic seeded CSV torture corpus with digests and expectations."""

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import random
import sys


DEFAULT_SEED = 20261121
LIMIT_64_MIB = 64 * 1024 * 1024


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_fixture(output_dir: Path, name: str, payload: bytes, expect: str, notes: str) -> dict:
    path = output_dir / name
    path.write_bytes(payload)
    return {
        "file": name,
        "bytes": len(payload),
        "sha256": hashlib.sha256(payload).hexdigest(),
        "expect": expect,
        "notes": notes,
    }


def write_oversize_fixture(output_dir: Path, name: str) -> dict:
    path = output_dir / name
    chunk = b"x" * (1024 * 1024)
    with path.open("wb") as handle:
        handle.write(b"id,blob\n1,")
        for _ in range(64):
            handle.write(chunk)
        handle.write(b"OVERFLOW\n")
    return {
        "file": name,
        "bytes": path.stat().st_size,
        "sha256": sha256_file(path),
        "expect": "reject(RecordTooLarge)",
        "notes": "Second record exceeds 64 MiB DEFAULT_MAX_RECORD_BYTES limit.",
    }


def build_seeded_valid_csv(seed: int) -> bytes:
    rng = random.Random(seed)
    lines = ["\ufeffid,label,payload,flag\r\n"]
    tokens = ["alpha", "beta,gamma", 'say "hi"', "line1\nline2", "", "42.5", "NULL"]
    for row_id in range(1, 201):
        label = rng.choice(tokens)
        payload = rng.choice(tokens)
        flag = rng.choice(["true", "false", ""])
        quoted_label = '"' + label.replace('"', '""') + '"'
        quoted_payload = '"' + payload.replace('"', '""') + '"'
        ending = "\r\n" if row_id % 2 == 0 else "\n"
        lines.append(f"{row_id},{quoted_label},{quoted_payload},{flag}{ending}")
    return "".join(lines).encode("utf-8")


def generate(output_dir: Path, seed: int, include_oversize: bool) -> dict:
    output_dir.mkdir(parents=True, exist_ok=True)
    entries = []

    valid_seeded = build_seeded_valid_csv(seed)
    entries.append(
        write_fixture(
            output_dir,
            "valid-seeded-utf8-bom.csv",
            valid_seeded,
            "accept",
            "Seeded UTF-8 BOM CSV with escaped quotes, multiline fields, mixed LF/CRLF.",
        )
    )

    gz_bytes = gzip.compress(b"id,value\n1,\"ok\"\n2,done\n", compresslevel=6, mtime=0)
    entries.append(
        write_fixture(
            output_dir,
            "valid-gzip.csv.gz",
            gz_bytes,
            "accept",
            "Valid deterministic gzip-compressed CSV (open via CompressedBootstrap).",
        )
    )

    zst_stub = bytes([0x28, 0xB5, 0x2F, 0xFD, 0x20, 0x00, 0x01, 0x00, 0x00])
    entries.append(
        write_fixture(
            output_dir,
            "unsupported-zstd.csv.zst",
            zst_stub,
            "reject(CompressedFile)",
            "Zstd magic header; explicitly rejected until Phase 7 .csv.zst support lands.",
        )
    )

    entries.append(
        write_fixture(
            output_dir,
            "malformed-bare-quote.csv",
            b'a,b\nbad"quote,2\n',
            "reject(Parse(UnexpectedQuote))",
            "Unescaped bare quote inside an unquoted field.",
        )
    )

    entries.append(
        write_fixture(
            output_dir,
            "malformed-after-quote.csv",
            b'a,b\n"ok"x,2\n',
            "reject(Parse(UnexpectedAfterQuote))",
            "Unexpected byte immediately after a closing quote.",
        )
    )

    entries.append(
        write_fixture(
            output_dir,
            "malformed-unterminated-quote.csv",
            b'a,b\n"still open,2',
            "reject(Parse(UnterminatedQuote))",
            "EOF reached while inside a quoted field.",
        )
    )

    entries.append(
        write_fixture(
            output_dir,
            "malformed-bare-cr-after-quote.csv",
            b'a,b\n"ok"\rx,2\n',
            "reject(Parse(ExpectedLfAfterCr))",
            "Carriage return after closing quote not followed by LF.",
        )
    )

    entries.append(
        write_fixture(
            output_dir,
            "unsupported-utf32-bom.csv",
            b"\xff\xfe\x00\x00a\x00\x00\x00,\x00\x00\x00b\x00\x00\x00",
            "reject(Encoding)",
            "UTF-32 LE BOM explicitly rejected.",
        )
    )

    entries.append(
        write_fixture(
            output_dir,
            "needs-confirm-latin1-no-bom.csv",
            b"name,city\ncaf\xe9,na\xefve\n",
            "needs-confirmation",
            "ISO-8859-1 bytes without BOM; Auto must not silently guess legacy encoding.",
        )
    )

    entries.append(
        write_fixture(
            output_dir,
            "needs-confirm-utf16le-no-bom.csv",
            "a,b\n1,2\n".encode("utf-16-le"),
            "needs-confirmation",
            "UTF-16 LE without BOM; requires explicit encoding confirmation.",
        )
    )

    if include_oversize:
        entries.append(write_oversize_fixture(output_dir, "oversize-over-64mib.csv"))

    manifest = {
        "seed": seed,
        "limit_bytes": LIMIT_64_MIB,
        "fixtures": entries,
    }
    manifest_path = output_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return manifest


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    repo_root = Path(__file__).resolve().parent.parent
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=repo_root / "fixtures" / "generated" / "torture-corpus",
        help="Destination directory (default: gitignored fixtures/generated/torture-corpus)",
    )
    parser.add_argument("--seed", type=int, default=DEFAULT_SEED)
    parser.add_argument(
        "--skip-oversize",
        action="store_true",
        help="Skip generating the >64 MiB oversize record fixture",
    )
    parser.add_argument(
        "--verify",
        action="store_true",
        help="Verify every file in an existing manifest matches its recorded SHA-256",
    )
    args = parser.parse_args()

    if args.verify:
        manifest_path = args.output_dir / "manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        for entry in manifest["fixtures"]:
            target = args.output_dir / entry["file"]
            actual_sha = sha256_file(target)
            actual_bytes = target.stat().st_size
            if actual_sha != entry["sha256"] or actual_bytes != entry["bytes"]:
                print(f"MISMATCH {entry['file']}: expected {entry['sha256']}, got {actual_sha}", file=sys.stderr)
                return 1
        print(f"Verified {len(manifest['fixtures'])} torture corpus fixtures in {args.output_dir}.")
        return 0

    manifest = generate(args.output_dir, args.seed, include_oversize=not args.skip_oversize)
    print(f"Generated {len(manifest['fixtures'])} torture corpus fixtures in {args.output_dir}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
