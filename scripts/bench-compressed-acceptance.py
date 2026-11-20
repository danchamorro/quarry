#!/usr/bin/env python3
"""Synthetic acceptance benchmarks for gzip-compressed delimited files.

Builds the compressed_acceptance release probe from a clean macOS checkout,
generates deterministic fixtures with an independent SHA-256 oracle, runs the
probe under /usr/bin/time -l while sampling disk use, and verifies every result.

Cases:
  large    two-member .csv.gz whose uncompressed size exceeds the machine's RAM
  cancel   cancel decompression of the large fixture (reuses it with --reuse)
  limit    a small decompression bomb opened with a 64 MiB limit
  corrupt  truncated, bad-CRC and trailing-garbage gzip, plus a valid control
"""
import argparse
import datetime
import hashlib
import json
import multiprocessing
import os
from pathlib import Path
import random
import re
import shutil
import subprocess
import sys
import time
import uuid
import zlib

PROBE = "compressed_acceptance"
MiB = 1024 ** 2
GiB = 1024 ** 3
HEADER = b"id,name,note,payload\n"
TARGET = "TARGET-MATCH"
LIMIT_BYTES = 64 * MiB
CANCEL_AFTER_BYTES = 512 * MiB


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(8 * MiB), b""):
            digest.update(chunk)
    return digest.hexdigest()


def exclusive(path):
    return os.fdopen(os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb")


def layout(smoke):
    if smoke:
        return {"rows": 2048, "payload_bytes": 32768, "target_every": 100, "embedded_row": 777}
    # 128 KiB records; 18 GiB of data rows in total.
    return {"rows": 18 * GiB // 131072, "payload_bytes": 131072,
            "target_every": 10000, "embedded_row": 12345}


def filler(payload_bytes):
    rng = random.Random(69)
    alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
    unit = bytes(rng.choice(alphabet) for _ in range(4096))
    return unit * (payload_bytes // 4096 + 2)


def row_bytes(row, spec, fill):
    """One fixed-length record: quoted comma and Unicode in every name, one
    embedded newline, and a filter target every `target_every` rows."""
    if row % spec["target_every"] == 0:
        note = b"TARGET-MATCH row %d" % row
    elif row == spec["embedded_row"]:
        note = b'"line one, still quoted\nline two ""quoted"""'
    else:
        note = b"plain"
    name = b'"Ortiz, Zo\xc3\xab \xe6\x9d\xb1\xe4\xba\xac \xe2\x82\xac%d"' % row
    prefix = b"row-%08d,%s,%s," % (row, name, note)
    start = (row * 7) % 4096
    return prefix + fill[start:start + spec["payload_bytes"] - len(prefix) - 1] + b"\n"


def write_member(task):
    """Compress one gzip member. The members split the stream inside a record."""
    member, path, spec = task
    fill = filler(spec["payload_bytes"])
    split = spec["rows"] // 2
    compressor = zlib.compressobj(1, zlib.DEFLATED, 31)
    plain = 0
    batch = []
    batched = 0
    with exclusive(Path(path)) as destination:
        def emit(data, final=False):
            nonlocal plain, batched
            plain += len(data)
            batch.append(data)
            batched += len(data)
            if batched >= 16 * MiB or final:
                destination.write(compressor.compress(b"".join(batch)))
                batch.clear()
                batched = 0
        middle = row_bytes(split, spec, fill)
        if member == 0:
            emit(HEADER)
            for row in range(split):
                emit(row_bytes(row, spec, fill))
            emit(middle[:len(middle) // 2], final=True)
        else:
            emit(middle[len(middle) // 2:])
            for row in range(split + 1, spec["rows"]):
                emit(row_bytes(row, spec, fill))
            emit(b"", final=True)
        destination.write(compressor.flush())
        destination.flush()
        os.fsync(destination.fileno())
    return plain


def generate_large(root, smoke, ram):
    spec = layout(smoke)
    parts = [root / "member-1.gz.part", root / "member-2.gz.part"]
    started = time.monotonic()
    context = multiprocessing.get_context("spawn")
    with context.Pool(2) as pool:
        pending = pool.map_async(write_member, [(index, str(parts[index]), spec) for index in range(2)])
        # Independent oracle: the complete stream and the expected filtered export.
        fill = filler(spec["payload_bytes"])
        full, filtered = hashlib.sha256(HEADER), hashlib.sha256(HEADER)
        total = filtered_bytes = len(HEADER)
        filtered_rows = 0
        last = time.monotonic()
        for row in range(spec["rows"]):
            data = row_bytes(row, spec, fill)
            full.update(data)
            total += len(data)
            if row % spec["target_every"] == 0:
                filtered.update(data)
                filtered_bytes += len(data)
                filtered_rows += 1
            if time.monotonic() - last >= 10:
                print("hashing_bytes=%d" % total, flush=True)
                last = time.monotonic()
        member_plain = pending.get()
    assert sum(member_plain) == total, "members do not add up to the oracle stream"
    member_compressed = [part.stat().st_size for part in parts]
    fixture = root / "large.csv.gz"
    with exclusive(fixture) as destination:
        for part in parts:
            with part.open("rb") as source:
                shutil.copyfileobj(source, destination, 16 * MiB)
        destination.flush()
        os.fsync(destination.fileno())
    for part in parts:
        part.unlink()
    if not smoke:
        assert total > ram, "the uncompressed large fixture must exceed actual RAM"
        assert member_plain[0] > 4.5 * GiB, "member 1 must exceed 4 GiB to exercise ISIZE wrap"
    manifest = {
        "schema": 1, "fixture_id": str(uuid.uuid4()), "case": "large", "smoke": smoke,
        "layout": spec, "columns": 4, "data_rows": spec["rows"],
        "members": 2, "member_uncompressed_bytes": member_plain,
        "member_compressed_bytes": member_compressed,
        "member_isize_mod_2_32": [size % 2 ** 32 for size in member_plain],
        "member_split": "inside data row %d" % (spec["rows"] // 2),
        "uncompressed_bytes": total, "uncompressed_sha256": full.hexdigest(),
        "compressed_bytes": fixture.stat().st_size, "compressed_sha256": sha256(fixture),
        "filter": {"column": 2, "operator": "contains", "value": TARGET},
        "filtered_rows": filtered_rows, "filtered_bytes": filtered_bytes,
        "filtered_sha256": filtered.hexdigest(), "ram_bytes": ram,
        "generation_seconds": time.monotonic() - started,
    }
    with exclusive(root / "fixture.json") as destination:
        destination.write((json.dumps(manifest, indent=2) + "\n").encode())
    return manifest


def gzip_member(data, level):
    compressor = zlib.compressobj(level, zlib.DEFLATED, 31)
    return compressor.compress(data) + compressor.flush()


def small_csv():
    rows = [b"row-%08d,\"Doe, J\xc3\xa9r\xc3\xb4me\",note %d\n" % (row, row) for row in range(200)]
    rows[50] = b'row-%08d,"Doe, J\xc3\xa9r\xc3\xb4me","two\nlines"\n' % 50
    return b"id,name,note\n" + b"".join(rows)


def generate_small(root, case, smoke):
    fixtures = {}
    if case == "limit":
        line = b"1," + b"x" * 1021 + b"\n"
        uncompressed = (16 if smoke else 320) * MiB
        compressor = zlib.compressobj(9, zlib.DEFLATED, 31)
        path = root / "bomb.csv.gz"
        block = line * 1024  # 1 MiB
        with exclusive(path) as destination:
            destination.write(compressor.compress(b"id,value\n"))
            for _ in range(uncompressed // len(block)):
                destination.write(compressor.compress(block))
            destination.write(compressor.flush())
        total = 9 + (uncompressed // len(block)) * len(block)
        if not smoke:
            assert path.stat().st_size < MiB, "the bomb must be under 1 MiB compressed"
            assert total > 256 * MiB, "the bomb must expand past 256 MiB"
        fixtures["bomb"] = {"path": path, "uncompressed_bytes": total}
    else:
        data = small_csv()
        half = len(data) // 2
        valid = gzip_member(data[:half], 6) + gzip_member(data[half:], 6)
        crc = bytearray(valid)
        crc[-8] ^= 0xFF  # the last member's CRC-32 trailer
        variants = {
            "valid_control": valid,
            "truncated": valid[:-37],
            "bad_crc32": bytes(crc),
            "trailing_garbage": valid + b"this is not a gzip member\n",
        }
        for name, content in variants.items():
            path = root / (name + ".csv.gz")
            with exclusive(path) as destination:
                destination.write(content)
            fixtures[name] = {"path": path, "uncompressed_bytes": len(data),
                              "uncompressed_sha256": hashlib.sha256(data).hexdigest()}
    for fixture in fixtures.values():
        fixture["compressed_bytes"] = fixture["path"].stat().st_size
        fixture["compressed_sha256"] = sha256(fixture["path"])
    return fixtures


def parse_metrics(content):
    result = {}
    for line in content.splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        if key.endswith("_cell") or key.startswith("error"):
            result[key] = value
            continue
        try:
            result[key] = float(value) if "." in value else int(value)
        except ValueError:
            result[key] = value
    return result


def du_bytes(path):
    measured = subprocess.run(["/usr/bin/du", "-sk", str(path)], capture_output=True, text=True)
    if measured.returncode != 0 or not measured.stdout:
        return 0
    return int(measured.stdout.split()[0]) * 1024


def residue(*folders):
    return sorted(str(path) for folder in folders for path in folder.rglob(".quarry-*"))


def measure(command, root, label, watch, interval):
    """Run the probe under /usr/bin/time -l, sampling `du -sk` on each watched folder."""
    stdout_path, stderr_path = root / (label + ".stdout"), root / (label + ".stderr")
    peaks = {name: 0 for name in watch}
    started = time.monotonic()
    last_report = started
    with exclusive(stdout_path) as stdout, exclusive(stderr_path) as stderr:
        process = subprocess.Popen(["/usr/bin/time", "-l"] + command, stdout=stdout, stderr=stderr)
        while process.poll() is None:
            for name, folder in watch.items():
                peaks[name] = max(peaks[name], du_bytes(folder))
            if time.monotonic() - last_report >= 15:
                lines = stderr_path.read_text(errors="replace").splitlines()
                phase = next((line for line in reversed(lines) if line.startswith("phase=")), None)
                print("%s elapsed=%.0fs %s" % (label, time.monotonic() - started, phase or ""), flush=True)
                last_report = time.monotonic()
            time.sleep(interval)
        exit_code = process.wait()
    elapsed = time.monotonic() - started
    stderr = stderr_path.read_text(errors="replace")
    resident = re.search(r"(\d+)\s+maximum resident set size", stderr)
    if exit_code:
        print(stderr[-6000:])
        raise SystemExit("%s: probe exited with %d" % (label, exit_code))
    assert resident, "time did not report peak RSS"
    return {
        "command": command, "exit_code": exit_code, "elapsed_seconds": elapsed,
        "max_rss_macos_time_bytes": int(resident.group(1)),
        "peak_allocated_bytes_sampled": peaks, "du_sample_interval_seconds": interval,
        "metrics": parse_metrics(stdout_path.read_text(errors="replace")),
    }


def expected_cell(record):
    return "id" if record == 0 else "row-%08d" % (record - 1)


def run_large(binary, root, manifest):
    fixture = root / "large.csv.gz"
    temp = root / "temp-large"
    temp.mkdir(mode=0o700)
    filtered, copy = root / "filtered.csv", root / "copy.csv"
    command = [str(binary), str(fixture), str(temp), "--filter-column", "2",
               "--filter-value", TARGET, "--export", str(filtered), "--save-copy", str(copy)]
    run = measure(command, root, "large", {"fixture_root": root, "temp": temp}, 0.5)
    metrics = run["metrics"]
    records = manifest["data_rows"] + 1
    assert metrics["result"] == "complete", metrics
    assert metrics["format"] == "gzip", metrics
    assert metrics["bootstrap_ms"] < 1000, "first rows took longer than a second"
    assert metrics["bootstrap_first_cell"] == "id" and metrics["bootstrap_second_cell"] == "row-00000000"
    assert metrics["live_viewport_cell"] == expected_cell(32), metrics
    assert metrics["rows_scanned"] == records, (metrics["rows_scanned"], records)
    assert metrics["columns_found"] == manifest["columns"], metrics
    assert metrics["decompressed_bytes"] == manifest["uncompressed_bytes"], metrics
    assert metrics["compressed_bytes"] == manifest["compressed_bytes"], metrics
    assert metrics["compressed_bytes_read"] == manifest["compressed_bytes"], metrics
    for label in ["first", "second", "middle", "last"]:
        record = metrics["viewport_%s_row" % label]
        assert metrics["viewport_%s_cell" % label] == expected_cell(record), (label, metrics)
        assert metrics["viewport_%s_fields" % label] == manifest["columns"], (label, metrics)
    assert metrics["viewport_last_row"] == records - 1
    assert metrics["export_rows"] == manifest["filtered_rows"], metrics
    assert metrics["export_bytes"] == manifest["filtered_bytes"], metrics
    assert filtered.stat().st_size == manifest["filtered_bytes"]
    assert sha256(filtered) == manifest["filtered_sha256"], "filtered export differs from the oracle"
    assert metrics["save_copy_bytes"] == manifest["uncompressed_bytes"], metrics
    assert copy.stat().st_size == manifest["uncompressed_bytes"]
    assert sha256(copy) == manifest["uncompressed_sha256"], "uncompressed copy differs from the oracle"
    assert metrics["gz_destination_refused"] == "InvalidOption", metrics
    assert metrics["source_unchanged"] == "true", metrics
    assert sha256(fixture) == manifest["compressed_sha256"], "the .csv.gz source changed"
    assert metrics["residual_workspaces"] == 0 and metrics["cleanup_errors"] == 0, metrics
    assert not list(temp.iterdir()), "private decompression data remains"
    assert not residue(root), residue(root)
    # The verified outputs are large; remove them so the cancel case has room.
    filtered.unlink()
    copy.unlink()
    temp.rmdir()
    run["verified"] = ["first rows from bootstrap", "live viewport during decompression",
                       "row and column counts", "first/second/middle/last viewport",
                       "filtered export bytes and SHA-256", "uncompressed copy bytes and SHA-256",
                       ".gz destination refused", "source SHA-256 unchanged", "no private residue"]
    run["outputs_removed_after_verification"] = True
    return run


def run_cancel(binary, root, manifest):
    fixture = root / "large.csv.gz"
    temp = root / "temp-cancel"
    temp.mkdir(mode=0o700)
    threshold = MiB if manifest["smoke"] else CANCEL_AFTER_BYTES
    command = [str(binary), str(fixture), str(temp), "--cancel-after-bytes", str(threshold)]
    run = measure(command, root, "cancel", {"temp": temp}, 0.1)
    metrics = run["metrics"]
    assert metrics["result"] == "cancelled", metrics
    assert metrics["cancel_requested_after_bytes"] >= threshold, metrics
    assert metrics["cancel_latency_seconds"] < 2.0, metrics
    assert metrics["residual_workspaces"] == 0 and metrics["cleanup_errors"] == 0, metrics
    assert not list(temp.iterdir()), "private decompression data remains after cancel"
    assert not residue(root), residue(root)
    assert sha256(fixture) == manifest["compressed_sha256"], "the .csv.gz source changed"
    temp.rmdir()
    run["cancel_after_bytes"] = threshold
    return run


def run_limit(binary, root, fixtures, smoke):
    limit = MiB if smoke else LIMIT_BYTES
    bomb = fixtures["bomb"]
    temp = root / "temp-limit"
    temp.mkdir(mode=0o700)
    command = [str(binary), str(bomb["path"]), str(temp), "--max-decompressed-bytes", str(limit)]
    run = measure(command, root, "limit", {"temp": temp}, 0.01)
    metrics = run["metrics"]
    assert metrics["result"] == "error", metrics
    assert metrics["error_kind"] == "DecompressionLimitExceeded", metrics
    if not smoke:
        # A 1 KiB-row bomb passes the in-memory bootstrap; the on-disk pass must stop it.
        assert metrics["error_stage"] == "decompress", metrics
    # The bootstrap stops a smaller bomb before any private copy exists.
    assert metrics.get("peak_decompressed_bytes_observed", 0) <= limit, metrics
    assert run["peak_allocated_bytes_sampled"]["temp"] <= limit, run
    assert metrics["residual_workspaces"] == 0 and metrics["cleanup_errors"] == 0, metrics
    assert not list(temp.iterdir()), "partial decompressed data remains"
    assert not residue(root), residue(root)
    assert sha256(bomb["path"]) == bomb["compressed_sha256"], "the bomb source changed"
    temp.rmdir()
    run["limit_bytes"] = limit
    return run


def run_corrupt(binary, root, fixtures):
    runs = {}
    for name, fixture in fixtures.items():
        temp = root / ("temp-" + name)
        temp.mkdir(mode=0o700)
        command = [str(binary), str(fixture["path"]), str(temp)]
        run = measure(command, root, name, {"temp": temp}, 0.01)
        metrics = run["metrics"]
        if name == "valid_control":
            assert metrics["result"] == "complete", metrics
            assert metrics["rows_scanned"] == 201 and metrics["columns_found"] == 3, metrics
            assert metrics["decompressed_bytes"] == fixture["uncompressed_bytes"], metrics
        else:
            assert metrics["result"] == "error", (name, metrics)
            assert metrics["error_kind"] == "CompressedFile", (name, metrics)
            assert metrics["error_message"], (name, metrics)
        assert metrics["residual_workspaces"] == 0 and metrics["cleanup_errors"] == 0, metrics
        assert not list(temp.iterdir()), "private decompression data remains for " + name
        assert sha256(fixture["path"]) == fixture["compressed_sha256"], name + " source changed"
        temp.rmdir()
        runs[name] = run
    assert not residue(root), residue(root)
    return runs


def clean_revision(repo):
    revision = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
    status = subprocess.check_output(
        ["git", "-C", str(repo), "status", "--porcelain=v1", "--untracked-files=normal"], text=True
    )
    if status.strip():
        raise SystemExit("Benchmark builds require a clean checkout. Commit source changes before measuring.")
    return revision


def build_probe(repo):
    revision = clean_revision(repo)
    cargo = shutil.which("cargo") or str(Path.home() / ".cargo/bin/cargo")
    if not Path(cargo).is_file():
        raise SystemExit("Cargo is required to build the measured release probe.")
    command = [cargo, "build", "-p", "quarry-core", "--release", "--locked",
               "--example", PROBE, "--message-format=json-render-diagnostics"]
    built = subprocess.run(command, cwd=repo, text=True, stdout=subprocess.PIPE, check=True)
    artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith("{")]
    executables = [item["executable"] for item in artifacts
                   if item.get("reason") == "compiler-artifact" and item.get("executable")
                   and item.get("target", {}).get("name") == PROBE
                   and "example" in item.get("target", {}).get("kind", [])]
    if len(executables) != 1:
        raise SystemExit("Cargo did not identify exactly one built %s executable." % PROBE)
    if clean_revision(repo) != revision:
        raise SystemExit("Checkout revision changed during the release build; no measurement was started.")
    binary = Path(executables[0]).resolve()
    return binary, revision, sha256(binary)


def environment():
    def sysctl(name):
        return subprocess.check_output(["/usr/sbin/sysctl", "-n", name], text=True).strip()
    def sw_vers(flag):
        return subprocess.check_output(["/usr/bin/sw_vers", flag], text=True).strip()
    return {
        "ram_bytes": int(sysctl("hw.memsize")), "logical_cpus": int(sysctl("hw.ncpu")),
        "cpu": sysctl("machdep.cpu.brand_string"), "model": sysctl("hw.model"),
        "macos": sw_vers("-productVersion"), "macos_build": sw_vers("-buildVersion"),
    }


def run(args):
    if sys.platform != "darwin":
        raise SystemExit("This harness records macOS time -l metrics; run it in VMPal macOS.")
    root = Path(args.root).resolve()
    repo = Path(__file__).resolve().parents[1]
    binary, revision, binary_digest = build_probe(repo)
    machine = environment()
    large_case = args.case in ("large", "cancel")
    if args.reuse:
        if not large_case:
            raise SystemExit("--reuse applies only to the large fixture.")
        manifest = json.loads((root / "fixture.json").read_text())
        assert manifest["case"] == "large" and manifest["smoke"] == args.smoke
        assert sha256(root / "large.csv.gz") == manifest["compressed_sha256"], "fixture changed"
    else:
        root.mkdir(mode=0o700, parents=False, exist_ok=False)
        if large_case:
            manifest = generate_large(root, args.smoke, machine["ram_bytes"])
        else:
            fixtures = generate_small(root, args.case, args.smoke)
            manifest = {name: {key: (str(value) if key == "path" else value)
                               for key, value in fixture.items()} for name, fixture in fixtures.items()}
    if (root / (args.case + ".report.json")).exists():
        raise SystemExit("Refusing to overwrite an existing report for " + args.case)
    started_at = datetime.datetime.now(datetime.timezone.utc).isoformat()
    if args.case == "large":
        result = run_large(binary, root, manifest)
    elif args.case == "cancel":
        result = run_cancel(binary, root, manifest)
    elif args.case == "limit":
        result = run_limit(binary, root, fixtures, args.smoke)
    else:
        result = run_corrupt(binary, root, fixtures)
    if clean_revision(repo) != revision or sha256(binary) != binary_digest:
        raise SystemExit("Source revision or measured binary changed; refusing to publish provenance.")
    report = {
        "case": args.case, "smoke": args.smoke, "started_at_utc": started_at,
        "environment": machine, "fixture": manifest, "result": result,
        "source_revision": revision, "source_status": "clean", "release_build_verified": True,
        "binary_sha256": binary_digest,
        "probe_source_sha256": sha256(repo / "crates/quarry-core/examples/compressed_acceptance.rs"),
        "harness_sha256": sha256(Path(__file__).resolve()),
        "all_checks_passed": True,
    }
    content = (json.dumps(report, indent=2) + "\n").encode()
    with exclusive(root / (args.case + ".report.json")) as destination:
        destination.write(content)
    if args.report:
        with exclusive(Path(args.report)) as destination:
            destination.write(content)
    print(json.dumps(report, indent=2), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--case", required=True, choices=["large", "cancel", "limit", "corrupt"])
    parser.add_argument("--root", required=True, help="New fixture folder (existing with --reuse)")
    parser.add_argument("--report", help="Also write the verified report to this new file")
    parser.add_argument("--smoke", action="store_true", help="Small fixtures for a quick harness check")
    parser.add_argument("--reuse", action="store_true", help="Reuse the large fixture in --root")
    run(parser.parse_args())
