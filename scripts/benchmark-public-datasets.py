#!/usr/bin/env python3
"""Build the Rust CLI from a clean checkout and run the public CSV capability suite."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import shutil
import subprocess
import sys
import threading
import time


DATASETS = {
    "weather": ("daily_weather_preprocessed_1896_2023.csv", "5", "1,2", "2", "1,2", "C0X100", "C0X101"),
    "hacker-news": ("hacknernews.csv", "12", "1", "5", "3,4", "ClickHouse", "CLICKHOUSE"),
}

PUBLISHED_INPUT_SHA256 = {
    "weather": "b775c8b7e6d39643bc1e10874deecc9b067e2866c79a8e647a6c93550412d47f",
    "hacker-news": "165d9336b6266519da4e99d0348576537f80337e40d1794aefb00258a49a0373",
}

# Raw-byte vs decoded-record output comparison contract per output-producing workload.
# Save As / transform operations normalize quoting (e.g. HN header quoting) while preserving
# decoded records; export, sort, and duplicate removal copy matching source records verbatim.
OUTPUT_EQUALITY = {
    "export": "raw-byte",
    "edit-save-as": "decoded-record",
    "replace-all": "decoded-record",
    "split-auto": "decoded-record",
    "combine": "decoded-record",
    "sort-number": "raw-byte",
    "duplicates-remove": "raw-byte",
}


def identity(path: Path) -> dict:
    before = path.stat()
    sha = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(16 * 1024 * 1024), b""):
            sha.update(chunk)
    after = path.stat()
    if stamp(before) != stamp(after):
        raise RuntimeError(f"Source changed while hashing: {path}")
    return {"file": path.name, "bytes": before.st_size, "sha256": sha.hexdigest()}


def inspect_csv_output(path: Path) -> dict:
    """Stream SHA-256, byte count, and quote-aware logical CSV row count outside timed intervals."""
    sha = hashlib.sha256()
    total_bytes = 0
    rows = 0
    in_quote = False
    last_byte = None
    first_three = bytearray()

    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(16 * 1024 * 1024), b""):
            sha.update(chunk)
            chunk_len = len(chunk)
            if len(first_three) < 3:
                need = 3 - len(first_three)
                first_three.extend(chunk[:need])
            total_bytes += chunk_len
            last_byte = chunk[-1]

            if not in_quote and b'"' not in chunk:
                rows += chunk.count(b"\n")
                continue

            pos = 0
            while pos < chunk_len:
                quote_pos = chunk.find(b'"', pos)
                if quote_pos == -1:
                    if not in_quote:
                        rows += chunk.count(b"\n", pos)
                    break
                if not in_quote:
                    rows += chunk.count(b"\n", pos, quote_pos)
                in_quote = not in_quote
                pos = quote_pos + 1

    bom_only = total_bytes == 3 and bytes(first_three) == b"\xef\xbb\xbf"
    if total_bytes > 0 and not bom_only and last_byte != 0x0A:
        rows += 1

    return {
        "output_sha256": sha.hexdigest(),
        "output_bytes": total_bytes,
        "output_rows": rows,
        "unterminated_quote": in_quote,
    }


def stamp(stat):
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns


def clean_revision(repo: Path) -> str:
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=repo):
        raise RuntimeError("Benchmark requires a clean checkout, including staged and untracked files")
    return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()


def directory_bytes(path: Path) -> int:
    total = 0
    try:
        for root, _, files in os.walk(path):
            root_path = Path(root)
            for name in files:
                try:
                    total += (root_path / name).stat().st_size
                except FileNotFoundError:
                    pass
    except FileNotFoundError:
        pass
    return total


def ru_maxrss_to_bytes(raw_maxrss: int) -> int:
    # Darwin reports ru_maxrss in bytes; Linux reports KiB.
    if sys.platform == "darwin":
        return int(raw_maxrss)
    return int(raw_maxrss) * 1024


def run_with_metrics(command: list[str], log_path: Path, scratch_dir: Path, sample_interval_s: float = 0.05) -> tuple[int, float, int, int]:
    """Execute one benchmark command, capturing wall time, per-child peak RSS, and peak scratch bytes."""
    peak_scratch = [0]
    stop_sampling = threading.Event()

    def sample_scratch():
        while not stop_sampling.is_set():
            current = directory_bytes(scratch_dir)
            if current > peak_scratch[0]:
                peak_scratch[0] = current
            stop_sampling.wait(sample_interval_s)

    sampler = threading.Thread(target=sample_scratch, daemon=True)
    sampler.start()
    started = time.monotonic()
    try:
        with log_path.open("w") as stream:
            proc = subprocess.Popen(command, stdout=stream, stderr=subprocess.STDOUT)
            if hasattr(os, "wait4"):
                _, wait_status, child_rusage = os.wait4(proc.pid, 0)
                if os.WIFEXITED(wait_status):
                    returncode = os.WEXITSTATUS(wait_status)
                elif os.WIFSIGNALED(wait_status):
                    returncode = -os.WTERMSIG(wait_status)
                else:
                    returncode = 1
                proc.returncode = returncode
                child_max_rss_bytes = ru_maxrss_to_bytes(child_rusage.ru_maxrss)
            else:
                returncode = proc.wait()
                child_max_rss_bytes = ru_maxrss_to_bytes(resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss)
    finally:
        command_seconds = time.monotonic() - started
        stop_sampling.set()
        sampler.join()
        final_scratch = directory_bytes(scratch_dir)
        if final_scratch > peak_scratch[0]:
            peak_scratch[0] = final_scratch

    return returncode, command_seconds, child_max_rss_bytes, peak_scratch[0]


def workloads(dataset: str, source: str, output: str) -> list[tuple[str, list[str]]]:
    _, numeric, duplicates, date, join, query, replacement = DATASETS[dataset]
    predicate = (
        ["--column", "5", "--operator", "between", "--value", "70", "--upper-bound", "80"]
        if dataset == "weather"
        else ["--column", "12", "--operator", "gte", "--value", "100"]
    )
    conjunction = ["--and", "1", "equals", "C0X100"] if dataset == "weather" else ["--and", "3", "equals", "story"]
    sort = [
        "sort-save-as",
        source,
        output,
        "--column",
        numeric,
        "--mode",
        "number",
        "--order",
        "asc",
        "--header",
        "first-row",
        "--temp-dir",
        str(Path(output).parent),
    ]
    duplicates_base = [
        "duplicates",
        source,
        "--columns",
        duplicates,
        "--match-case",
        "--header",
        "first-row",
        "--temp-dir",
        str(Path(output).parent),
    ]
    return [
        ("open", ["open", source, "--rows", "100", "--metrics-only"]),
        ("viewport", ["viewport", source, "--iterations", "500", "--rows", "100", "--seed", "20260911"]),
        ("find-early", ["search", source, "--query", "C0X100" if dataset == "weather" else "callmeed"]),
        ("find-absent", ["search", source, "--query", "QUARRY_NOT_PRESENT_20260911"]),
        ("filter-numeric", ["filter", source] + predicate),
        ("filter-and", ["filter", source] + predicate + conjunction),
        ("export", ["export", source, "--output", output] + predicate + conjunction),
        (
            "edit-save-as",
            [
                "edit-save-as",
                source,
                "--output",
                output,
                "--edit",
                "1",
                "1",
                "QUARRY_BENCH_FIRST",
                "--edit",
                "100000",
                "1",
                "QUARRY_BENCH_LATER",
            ],
        ),
        ("replace-all", ["replace-all-save-as", source, "--output", output, "--query", query, "--replacement", replacement]),
        ("split-auto", ["transform-save-as", source, "--output", output, "--split-auto", date, " "]),
        ("combine", ["transform-save-as", source, "--output", output, "--join", join, "|", "--output-header", "Combined"]),
        ("sort-number", sort),
        ("duplicates", duplicates_base),
        ("duplicates-remove", duplicates_base + ["--output", output]),
        ("sort-cancel", sort + ["--cancel-after-bytes", "67108864"]),
    ]


def verify_expected_input_hashes(before: dict, expected_hashes: dict) -> None:
    for name, expected in expected_hashes.items():
        if name not in before:
            continue
        actual = before[name]["sha256"]
        if actual != expected:
            raise RuntimeError(
                f"Input SHA-256 mismatch for {name} ({before[name]['file']}): expected {expected}, got {actual}"
            )


def verify_output_entry(case_key: str, entry: dict, expected_outputs: dict | None, expects_output: bool) -> str | None:
    if entry["exit_code"] != 0:
        return None
    if expects_output:
        if entry.get("output_sha256") is None:
            return f"{case_key}: expected output.csv was not produced"
        if entry.get("output_unterminated_quote"):
            return f"{case_key}: output.csv ended inside an unterminated quoted field"
    if expected_outputs and case_key in expected_outputs:
        expected = expected_outputs[case_key]
        for field in ("output_sha256", "output_bytes", "output_rows"):
            if field in expected and entry.get(field) != expected[field]:
                return (
                    f"{case_key}: {field} mismatch (expected {expected[field]!r}, got {entry.get(field)!r})"
                )
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data_dir", type=Path, help="Directory containing both original extracted CSVs")
    parser.add_argument("results_dir", type=Path, help="New directory for logs and evidence; must not exist")
    parser.add_argument(
        "--expected-hashes",
        type=Path,
        default=None,
        help="Optional JSON map overriding the published dataset SHA-256 hashes (for synthetic/smoke checks)",
    )
    parser.add_argument(
        "--expected-outputs",
        type=Path,
        default=None,
        help="Optional JSON map of '<dataset>/<case>' -> {output_sha256, output_bytes, output_rows} golden expectations",
    )
    parser.add_argument(
        "--threads",
        type=int,
        default=None,
        help="Optional worker thread count forwarded to quarry-bench via --threads (supported in Phases 2/3)",
    )
    parser.add_argument(
        "--repeats",
        type=int,
        default=1,
        help="Number of repeated runs per workload (default: 1)",
    )
    parser.add_argument(
        "--cache-state",
        choices=("warm", "cold"),
        default="warm",
        help="Cache protocol label forwarded to quarry-bench (default: warm)",
    )
    args = parser.parse_args()
    if args.repeats < 1:
        raise ValueError("--repeats must be >= 1")
    if args.threads is not None and args.threads < 1:
        raise ValueError("--threads must be >= 1")

    expected_hashes = (
        json.loads(args.expected_hashes.read_text(encoding="utf-8"))
        if args.expected_hashes
        else PUBLISHED_INPUT_SHA256
    )
    expected_outputs = (
        json.loads(args.expected_outputs.read_text(encoding="utf-8"))
        if args.expected_outputs
        else None
    )

    repo = Path(__file__).resolve().parent.parent
    sources = {name: (args.data_dir / config[0]).resolve(strict=True) for name, config in DATASETS.items()}
    results = args.results_dir.resolve()
    if results.exists():
        raise FileExistsError(f"Results directory already exists: {results}")

    revision = clean_revision(repo)
    before = {name: identity(source) for name, source in sources.items()}
    verify_expected_input_hashes(before, expected_hashes)
    source_stamps = {name: stamp(source.stat()) for name, source in sources.items()}

    build_command = [
        "cargo",
        "build",
        "--release",
        "--locked",
        "-p",
        "quarry-cli",
        "--bin",
        "quarry-bench",
        "--message-format=json",
    ]
    build = subprocess.run(build_command, cwd=repo, stdout=subprocess.PIPE, text=True, check=True)
    if clean_revision(repo) != revision:
        raise RuntimeError("Checkout revision changed during build")
    artifacts = [json.loads(line) for line in build.stdout.splitlines()]
    binary = next(
        Path(artifact["executable"])
        for artifact in artifacts
        if artifact.get("reason") == "compiler-artifact"
        and artifact["target"]["name"] == "quarry-bench"
        and artifact.get("executable")
    )

    results.mkdir(parents=True, exist_ok=False)
    frozen_binary = results / "quarry-bench"
    shutil.copy2(binary, frozen_binary)

    cache_note = (
        "Full SHA-256 read before measurements; warm label only, no OS cache eviction or control."
        if args.cache_state == "warm"
        else "Cold label explicitly selected by operator; caller is responsible for OS cache eviction."
    )
    evidence = {
        "started_utc": datetime.now(timezone.utc).isoformat(),
        "engine_revision": revision,
        "engine_source_status": "clean",
        "build_command": build_command,
        "platform": platform.platform(),
        "machine": platform.machine(),
        "binary_sha256": identity(frozen_binary)["sha256"],
        "free_bytes_before": shutil.disk_usage(results).free,
        "cache_state": args.cache_state,
        "cache": cache_note,
        "repeats": args.repeats,
        "threads": args.threads,
        "expected_input_sha256": expected_hashes,
        "sources_before": before,
        "runs": [],
    }
    manifest = results / "results.json"
    manifest.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")

    for repeat in range(1, args.repeats + 1):
        for dataset, source in sources.items():
            for name, _ in workloads(dataset, str(source), "placeholder.csv"):
                run_suffix = f"-r{repeat}" if args.repeats > 1 else ""
                directory = results / f"{dataset}-{name}{run_suffix}"
                directory.mkdir(mode=0o700)
                output = directory / "output.csv"
                command_args = dict(workloads(dataset, str(source), str(output)))[name]
                extra_args = ["--cache-state", args.cache_state]
                if args.threads is not None:
                    extra_args.extend(["--threads", str(args.threads)])
                command = [str(frozen_binary), *command_args, *extra_args]
                log = results / f"{dataset}-{name}{run_suffix}.log"
                print(f"START {dataset}/{name} (repeat {repeat}/{args.repeats})", flush=True)

                returncode, command_seconds, child_max_rss_bytes, peak_scratch_bytes = run_with_metrics(
                    command, log, directory
                )

                for source_name, path in sources.items():
                    if stamp(path.stat()) != source_stamps[source_name]:
                        raise RuntimeError(f"Source changed during {dataset}/{name}: {path}")

                # Compute output SHA-256, bytes, and row count BEFORE cleanup and OUTSIDE timed intervals.
                output_stats = inspect_csv_output(output) if output.exists() else None
                case_key = f"{dataset}/{name}"
                expects_output = name in OUTPUT_EQUALITY
                entry = {
                    "dataset": dataset,
                    "case": name,
                    "repeat": repeat,
                    "command": command_args + extra_args,
                    "exit_code": returncode,
                    "command_seconds": round(command_seconds, 6),
                    "child_max_rss_bytes": child_max_rss_bytes,
                    "peak_scratch_bytes": peak_scratch_bytes,
                    "log": log.name,
                    "output_equality": OUTPUT_EQUALITY.get(name),
                    "output_bytes": output_stats["output_bytes"] if output_stats else None,
                    "output_rows": output_stats["output_rows"] if output_stats else None,
                    "output_sha256": output_stats["output_sha256"] if output_stats else None,
                    "output_unterminated_quote": output_stats["unterminated_quote"] if output_stats else None,
                }
                if name == "sort-cancel" and any(directory.iterdir()):
                    entry["cleanup_error"] = "Cancellation left output or temporary files"

                verification_error = verify_output_entry(case_key, entry, expected_outputs, expects_output)
                if verification_error:
                    entry["output_verification_error"] = verification_error

                # Only remove output in this newly created run directory after successful validation.
                if (
                    returncode == 0
                    and "cleanup_error" not in entry
                    and "output_verification_error" not in entry
                ):
                    shutil.rmtree(directory)

                evidence["runs"].append(entry)
                manifest.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
                print(
                    f"END {dataset}/{name}: exit={returncode}, command={entry['command_seconds']:.3f}s, "
                    f"rss={child_max_rss_bytes}, scratch={peak_scratch_bytes}",
                    flush=True,
                )

    evidence["rusage_children_max_rss_bytes"] = ru_maxrss_to_bytes(
        resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    )
    evidence["sources_after"] = {name: identity(source) for name, source in sources.items()}
    evidence["sources_unchanged"] = evidence["sources_after"] == before
    evidence["finished_utc"] = datetime.now(timezone.utc).isoformat()
    manifest.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")

    ok = evidence["sources_unchanged"] and all(
        run["exit_code"] == 0
        and "cleanup_error" not in run
        and "output_verification_error" not in run
        for run in evidence["runs"]
    )
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
