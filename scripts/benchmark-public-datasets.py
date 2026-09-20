#!/usr/bin/env python3
"""Run the public CSV capability suite sequentially with the existing Rust CLI."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time


DATASETS = {
    "weather": ("daily_weather_preprocessed_1896_2023.csv", "5", "1,2", "2", "1,2", "C0X100", "C0X101"),
    "hacker-news": ("hacknernews.csv", "12", "1", "5", "3,4", "ClickHouse", "CLICKHOUSE"),
}


def identity(path):
    before = path.stat()
    sha = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(16 * 1024 * 1024), b""):
            sha.update(chunk)
    after = path.stat()
    if stamp(before) != stamp(after):
        raise RuntimeError(f"Source changed while hashing: {path}")
    return {"file": path.name, "bytes": before.st_size, "sha256": sha.hexdigest()}


def stamp(stat):
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns


def workloads(dataset, source, output):
    _, numeric, duplicates, date, join, query, replacement = DATASETS[dataset]
    predicate = (["--column", "5", "--operator", "between", "--value", "70", "--upper-bound", "80"]
                 if dataset == "weather" else
                 ["--column", "12", "--operator", "gte", "--value", "100"])
    conjunction = ["--and", "1", "equals", "C0X100"] if dataset == "weather" else ["--and", "3", "equals", "story"]
    sort = ["sort-save-as", source, output, "--column", numeric, "--mode", "number", "--order", "asc", "--header", "first-row", "--temp-dir", str(Path(output).parent)]
    return [
        ("open", ["open", source, "--rows", "100", "--metrics-only"]),
        ("viewport", ["viewport", source, "--iterations", "500", "--rows", "100", "--seed", "20260911"]),
        ("find-early", ["search", source, "--query", "C0X100" if dataset == "weather" else "callmeed"]),
        ("find-absent", ["search", source, "--query", "QUARRY_NOT_PRESENT_20260911"]),
        ("filter-numeric", ["filter", source] + predicate),
        ("filter-and", ["filter", source] + predicate + conjunction),
        ("export", ["export", source, "--output", output] + predicate + conjunction),
        ("edit-save-as", ["edit-save-as", source, "--output", output, "--edit", "1", "1", "QUARRY_BENCH_FIRST", "--edit", "100000", "1", "QUARRY_BENCH_LATER"]),
        ("replace-all", ["replace-all-save-as", source, "--output", output, "--query", query, "--replacement", replacement]),
        ("split-auto", ["transform-save-as", source, "--output", output, "--split-auto", date, " "]),
        ("combine", ["transform-save-as", source, "--output", output, "--join", join, "|", "--output-header", "Combined"]),
        ("sort-number", sort),
        ("duplicates", ["duplicates", source, "--columns", duplicates, "--match-case", "--header", "first-row", "--temp-dir", str(Path(output).parent)]),
        ("sort-cancel", sort + ["--cancel-after-bytes", "67108864"]),
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data_dir", type=Path, help="Directory containing both original extracted CSVs")
    parser.add_argument("results_dir", type=Path, help="New directory for logs and evidence; must not exist")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    binary = repo / "target/release/quarry-bench"
    if not binary.is_file():
        parser.error("Build first: cargo build --release --locked -p quarry-cli --bin quarry-bench")
    sources = {name: (args.data_dir / config[0]).resolve(strict=True) for name, config in DATASETS.items()}
    results = args.results_dir.resolve()
    results.mkdir(parents=True, exist_ok=False)
    frozen_binary = results / "quarry-bench"
    shutil.copy2(binary, frozen_binary)
    before = {name: identity(source) for name, source in sources.items()}
    source_stamps = {name: stamp(source.stat()) for name, source in sources.items()}
    evidence = {
        "started_utc": datetime.now(timezone.utc).isoformat(),
        "engine_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "platform": platform.platform(),
        "binary_sha256": identity(frozen_binary)["sha256"],
        "free_bytes_before": shutil.disk_usage(results).free,
        "cache": "Full SHA-256 read before measurements; warm label only, no OS cache eviction or control.",
        "sources_before": before,
        "runs": [],
    }
    (results / "benchmark-cli.patch").write_bytes(subprocess.check_output(["git", "diff", "HEAD", "--", "apps/quarry-cli/src/lib.rs"], cwd=repo))
    manifest = results / "results.json"
    manifest.write_text(json.dumps(evidence, indent=2) + "\n")
    for dataset, source in sources.items():
        for name, _ in workloads(dataset, str(source), "placeholder.csv"):
            directory = results / f"{dataset}-{name}"
            directory.mkdir(mode=0o700)
            output = directory / "output.csv"
            command_args = dict(workloads(dataset, str(source), str(output)))[name]
            command = [str(frozen_binary), *command_args, "--cache-state", "warm"]
            log = results / f"{dataset}-{name}.log"
            print(f"START {dataset}/{name}", flush=True)
            started = time.monotonic()
            with log.open("w") as stream:
                completed = subprocess.run(command, stdout=stream, stderr=subprocess.STDOUT)
            command_seconds = time.monotonic() - started
            for source_name, path in sources.items():
                if stamp(path.stat()) != source_stamps[source_name]:
                    raise RuntimeError(f"Source changed during {dataset}/{name}: {path}")
            entry = {"dataset": dataset, "case": name, "command": command_args + ["--cache-state", "warm"], "exit_code": completed.returncode,
                     "command_seconds": round(command_seconds, 6), "log": log.name,
                     "output_bytes": output.stat().st_size if output.exists() else None}
            if name == "sort-cancel" and any(directory.iterdir()):
                entry["cleanup_error"] = "Cancellation left output or temporary files"
            # Only remove output in this newly created run directory after successful CLI validation.
            if completed.returncode == 0 and "cleanup_error" not in entry:
                shutil.rmtree(directory)
            evidence["runs"].append(entry)
            manifest.write_text(json.dumps(evidence, indent=2) + "\n")
            print(f"END {dataset}/{name}: exit={completed.returncode}, command={entry['command_seconds']:.3f}s", flush=True)
    evidence["sources_after"] = {name: identity(source) for name, source in sources.items()}
    evidence["sources_unchanged"] = evidence["sources_after"] == before
    evidence["finished_utc"] = datetime.now(timezone.utc).isoformat()
    manifest.write_text(json.dumps(evidence, indent=2) + "\n")
    return 0 if evidence["sources_unchanged"] and all(run["exit_code"] == 0 and "cleanup_error" not in run for run in evidence["runs"]) else 1


if __name__ == "__main__":
    sys.exit(main())
