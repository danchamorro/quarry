#!/usr/bin/env python3
"""Synthetic full-result join benchmarks. Builds and verifies its release probe from a clean macOS checkout."""
import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import uuid


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(4 * 1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def payload(side, row, size):
    tag = ("%s-%08d-" % (side, row)).encode()
    return (tag * ((size + len(tag) - 1) // len(tag)))[:size]


def exclusive(path):
    return os.fdopen(os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb")


def record(fields, delimiter):
    # Every fixture value consists solely of letters, digits and hyphens.
    return delimiter.join(fields) + b"\n"


def wide_fields(side, row, columns, keys):
    matched = row % 4 == 0
    fields = [
        (("shared" if matched else side) + "-key%d-row%d" % (key, row)).encode()
        for key in range(keys)
    ]
    fields += [("%s-row%d-column%d" % (side, row, column)).encode() for column in range(keys, columns)]
    return fields


def generate(args, root, ram):
    smoke = args.smoke
    rows = (8 if smoke else int(math.ceil(18 * 1024**3 / (2 * 131072)))) if args.case == "large" else None
    if args.case == "wide":
        rows = 3 if smoke else 32
        columns, keys = (32, 2) if smoke else (32768, 16)
        counts = [rows, rows]
        delimiters = [b"\t", b","]
        headings = [
            [("%s-key%d" % (side, index)).encode() if index < keys else
             ("%s-column%d" % (side, index)).encode() for index in range(columns)]
            for side in ["main", "additional"]
        ]
        output_header = record(headings[0] + headings[1], b"\t")
        size = None
    else:
        counts = [rows, rows] if args.case == "large" else ([8, 9] if smoke else [1024, 1536])
        columns, keys = 2, 1
        delimiters = [b",", b","]
        headings = [[b"key", b"payload"], [b"key", b"payload"]]
        output_header = b"key (Main),payload (Main),key (Additional),payload (Additional)\n"
        size = (32 if smoke else 131072) if args.case == "large" else (32 if smoke else 256)
    paths = [root / "main.csv", root / "additional.csv"]
    expected = hashlib.sha256(output_header)
    expected_bytes = len(output_header)
    expected_rows = counts[0] if args.case != "skew" else counts[0] * counts[1]
    input_hashes, input_bytes = [], []
    started = time.monotonic()
    for side in [0, 1]:
        digest = hashlib.sha256()
        last = time.monotonic()
        written = 0
        with exclusive(paths[side]) as destination:
            header = record(headings[side], delimiters[side])
            destination.write(header)
            digest.update(header)
            written += len(header)
            for row in range(counts[side]):
                if args.case == "wide":
                    fields = wide_fields("main" if side == 0 else "additional", row, columns, keys)
                else:
                    key = b"k" if args.case == "skew" else (
                        ("shared" if row % 1000 == 0 else ("main" if side == 0 else "additional"))
                        + "-%08d" % row
                    ).encode()
                    fields = [key, payload("M" if side == 0 else "A", row, size)]
                data = record(fields, delimiters[side])
                destination.write(data)
                digest.update(data)
                written += len(data)
                if side == 0:
                    if args.case == "wide":
                        additional = wide_fields("additional", row, columns, keys) if row % 4 == 0 else [b""] * columns
                        expected_data = record(fields + additional, b"\t")
                        expected.update(expected_data)
                        expected_bytes += len(expected_data)
                    elif args.case == "large":
                        additional = [fields[0], payload("A", row, size)] if row % 1000 == 0 else [b"", b""]
                        expected_data = record(fields + additional, b",")
                        expected.update(expected_data)
                        expected_bytes += len(expected_data)
                    else:
                        for additional_row in range(counts[1]):
                            expected_data = record(fields + [b"k", payload("A", additional_row, size)], b",")
                            expected.update(expected_data)
                            expected_bytes += len(expected_data)
                if time.monotonic() - last >= 10:
                    print("generating_side=%d bytes=%d" % (side, written), flush=True)
                    last = time.monotonic()
            destination.flush()
            os.fsync(destination.fileno())
        input_hashes.append(digest.hexdigest())
        input_bytes.append(written)
        print("generated_side=%d bytes=%d" % (side, written), flush=True)
    if args.case == "large" and not smoke:
        assert sum(input_bytes) > ram, "the complete large fixture must exceed actual VM RAM"
    manifest = {
        "schema": 1, "fixture_id": str(uuid.uuid4()), "case": args.case,
        "smoke": smoke, "rows": counts, "columns_per_side": columns, "keys": keys,
        "payload_bytes": size, "input_hashes": input_hashes, "input_bytes": input_bytes,
        "expected_sha256": expected.hexdigest(), "expected_rows": expected_rows,
        "expected_bytes": expected_bytes, "ram_bytes": ram,
        "generation_seconds": time.monotonic() - started,
        "input_allocated_bytes": [path.stat().st_blocks * 512 for path in paths],
        "delimiters": ["tab" if value == b"\t" else "comma" for value in delimiters],
    }
    with exclusive(root / "fixture.json") as destination:
        destination.write((json.dumps(manifest, indent=2) + "\n").encode())
    return manifest


def parse_metrics(content):
    result = {}
    for line in content.splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        try:
            parsed = float(value) if "." in value else int(value)
        except ValueError:
            parsed = value
        # The probe reports one observed peak for each separate job.
        if key == "observed_peak_temporary_bytes":
            result[key] = max(result.get(key, 0), parsed)
        else:
            result[key] = parsed
    return result


def clean_revision(repo):
    revision = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
    status = subprocess.check_output(
        ["git", "-C", str(repo), "status", "--porcelain=v1", "--untracked-files=normal"], text=True
    )
    if status.strip():
        raise SystemExit("Benchmark builds require a clean checkout. Commit source changes before measuring.")
    return revision


def build_probe(repo, requested_binary):
    revision = clean_revision(repo)
    cargo = shutil.which("cargo")
    if cargo is None:
        fallback = Path.home() / ".cargo/bin/cargo"
        cargo = str(fallback) if fallback.is_file() else None
    if cargo is None:
        raise SystemExit("Cargo is required to build the measured release probe.")
    command = [cargo, "build", "-p", "quarry-core", "--release", "--locked",
               "--example", "join_acceptance", "--message-format=json-render-diagnostics"]
    built = subprocess.run(command, cwd=repo, text=True, stdout=subprocess.PIPE, check=True)
    artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith("{")]
    executables = [item["executable"] for item in artifacts
                   if item.get("reason") == "compiler-artifact" and item.get("executable")
                   and item.get("target", {}).get("name") == "join_acceptance"
                   and "example" in item.get("target", {}).get("kind", [])]
    if len(executables) != 1:
        raise SystemExit("Cargo did not identify exactly one built join_acceptance executable.")
    if clean_revision(repo) != revision:
        raise SystemExit("Checkout revision changed during the release build; no measurement was started.")
    artifact = Path(executables[0]).resolve()
    binary = Path(requested_binary).resolve() if requested_binary else artifact
    digest = sha256(artifact)
    if not binary.is_file() or sha256(binary) != digest:
        raise SystemExit("The requested binary does not match the probe just built from this clean revision.")
    return binary, revision, digest


def verify_measured_build(repo, revision, binary, digest):
    if clean_revision(repo) != revision or sha256(binary) != digest:
        raise SystemExit("Source revision or measured binary changed; refusing to publish benchmark provenance.")


def run(args):
    if sys.platform != "darwin":
        raise SystemExit("This harness records macOS time -l metrics; run it in VMPal macOS.")
    root = Path(args.root).resolve()
    repo = Path(__file__).resolve().parents[1]
    binary, source_revision, binary_digest = build_probe(repo, args.binary)
    ram = int(subprocess.check_output(["/usr/sbin/sysctl", "-n", "hw.memsize"], text=True))
    if args.reuse:
        manifest = json.loads((root / "fixture.json").read_text())
        assert manifest["case"] == args.case and manifest["smoke"] == args.smoke
    else:
        root.mkdir(mode=0o700, parents=False, exist_ok=False)
        manifest = generate(args, root, ram)
    label = "cancel" if args.cancel_after_rows is not None else "complete"
    output = root / (label + ".csv")
    if output.exists():
        raise SystemExit("Refusing to overwrite an existing benchmark output: " + str(output))
    temporary = root / ("temp-" + label)
    temporary.mkdir(mode=0o700, exist_ok=False)
    stdout_path, stderr_path = root / (label + ".stdout"), root / (label + ".stderr")
    command = [str(binary), str(root / "main.csv"), str(root / "additional.csv"),
               str(output), str(temporary), "--keys", str(manifest["keys"]),
               "--main-delimiter", manifest["delimiters"][0],
               "--additional-delimiter", manifest["delimiters"][1],
               "--acknowledge-expansion"]
    if args.case == "skew":
        command += ["--group-bytes", str(128 * 1024), "--run-bytes", str(1024 * 1024)]
    if args.cancel_after_rows is not None:
        command += ["--cancel-after-rows", str(args.cancel_after_rows)]
    peak_allocated = 0
    last_phase = None
    verify_measured_build(repo, source_revision, binary, binary_digest)
    measurement_start = time.monotonic()
    started_at = datetime.datetime.now(datetime.timezone.utc).isoformat()
    with exclusive(stdout_path) as stdout, exclusive(stderr_path) as stderr:
        process = subprocess.Popen(["/usr/bin/time", "-l"] + command, stdout=stdout, stderr=stderr)
        print("benchmark_pid=%d case=%s label=%s" % (process.pid, args.case, label), flush=True)
        while process.poll() is None:
            measured = subprocess.run(["/usr/bin/du", "-sk", str(root)], capture_output=True, text=True)
            if measured.returncode == 0:
                peak_allocated = max(peak_allocated, int(measured.stdout.split()[0]) * 1024)
            lines = stderr_path.read_text(errors="replace").splitlines()
            phase = next((line for line in reversed(lines) if line.startswith("phase=")), None)
            if phase is not None and phase != last_phase:
                print(phase, flush=True)
                last_phase = phase
            time.sleep(0.5)
        exit_code = process.wait()
    metrics = parse_metrics(stdout_path.read_text())
    stderr = stderr_path.read_text(errors="replace")
    resident = re.search(r"(\d+)\s+maximum resident set size", stderr)
    verify_measured_build(repo, source_revision, binary, binary_digest)
    report = {
        "case": args.case, "smoke": args.smoke, "label": label,
        "fixture_id": manifest["fixture_id"], "command": command, "exit_code": exit_code,
        "input_bytes": manifest["input_bytes"], "ram_bytes": ram,
        "elapsed_seconds": time.monotonic() - measurement_start,
        "peak_fixture_allocated_bytes_sampled": peak_allocated,
        "max_rss_macos_time_bytes": int(resident.group(1)) if resident else None,
        "metrics": metrics,
        "started_at_utc": started_at,
        "source_revision": source_revision,
        "source_status": "clean",
        "release_build_verified": True,
        "binary_sha256": binary_digest,
        "probe_source_sha256": sha256(repo / "crates/quarry-core/examples/join_acceptance.rs"),
        "harness_sha256": sha256(Path(__file__).resolve()),
    }
    with exclusive(root / (label + ".report.json")) as destination:
        destination.write((json.dumps(report, indent=2) + "\n").encode())
    if exit_code:
        print(stderr[-6000:])
        raise SystemExit(exit_code)
    assert report["max_rss_macos_time_bytes"] is not None, "time did not report peak RSS"
    if args.cancel_after_rows is None:
        assert metrics["result"] == "complete", metrics
        assert metrics["actual_rows"] == manifest["expected_rows"], metrics
        assert metrics["actual_bytes"] == manifest["expected_bytes"], metrics
        assert sha256(output) == manifest["expected_sha256"], "complete result differs from independent fixture oracle"
    else:
        assert metrics["result"] == "cancelled", metrics
        assert "cancel_latency_seconds" in metrics
        assert not output.exists()
    for side, name in enumerate(["main.csv", "additional.csv"]):
        assert sha256(root / name) == manifest["input_hashes"][side], "an input changed"
    assert not list(temporary.iterdir()), "unpublished join data remains"
    assert not list(root.glob(".quarry-*")), "output staging remains"
    report["all_checks_passed"] = True
    (root / (label + ".report.json")).write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--case", required=True, choices=["large", "skew", "wide"])
    parser.add_argument("--root", required=True)
    parser.add_argument("--binary", help="Optional copy; must match the probe built from the clean checkout")
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--reuse", action="store_true")
    parser.add_argument("--cancel-after-rows", type=int)
    run(parser.parse_args())
