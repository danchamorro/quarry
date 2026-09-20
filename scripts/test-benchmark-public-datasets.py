#!/usr/bin/env python3
"""Smoke-check provenance and cleanup with real Git, a fake build, and tiny inputs."""

import contextlib
import hashlib
import io
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
from unittest.mock import patch


def main():
    with tempfile.TemporaryDirectory(prefix="quarry-runner-smoke-") as temporary:
        repo = Path(temporary).resolve()
        script = repo / "scripts/benchmark-public-datasets.py"
        script.parent.mkdir()
        shutil.copyfile(Path(__file__).with_name(script.name), script)
        runner = runpy.run_path(str(script))
        (repo / ".gitignore").write_text("/target/\n/data/\n/results/\n")
        tracked = repo / "Cargo.toml"
        tracked.write_text("committed source\n")
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        subprocess.run(["git", "add", "."], cwd=repo, check=True)
        subprocess.run(["git", "-c", "user.name=Runner Smoke", "-c", "user.email=smoke@example.invalid",
                        "commit", "-qm", "Initial fixture"], cwd=repo, check=True)
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
        stale_binary = repo / "target/release/quarry-bench"
        stale_binary.parent.mkdir(parents=True)
        stale_binary.write_text(f"#!{sys.executable}\nraise SystemExit('stale binary executed')\n")
        stale_binary.chmod(0o700)
        binary = repo / "target/custom/release/quarry-bench"
        binary.parent.mkdir(parents=True)
        cli = f"#!{sys.executable}\n" + '''
import os, sys
from pathlib import Path
args = sys.argv[1:]
assert "--join" not in args or args.count("--output-header") == 1, "join requires exactly one --output-header value"
output = (Path(args[args.index("--output") + 1]) if "--output" in args else
          Path(args[2]) if args[0] == "sort-save-as" else None)
if output and "--cancel-after-bytes" not in args:
    output.write_bytes(b"generated output\\n")
if args[0] == "edit-save-as" and os.environ.get("QUARRY_RUNNER_SMOKE_FAIL"):
    output.with_name("failure.tmp").write_bytes(b"failure evidence")
    print("mock CLI failure", file=sys.stderr)
    sys.exit(7)
print("mock CLI success")
'''
        data = repo / "data"
        data.mkdir()
        original = b"id,value\n1,tiny\n"
        for config in runner["DATASETS"].values():
            (data / config[0]).write_bytes(original)
        original_run = subprocess.run
        builds = []
        def run(results, fail=False, during_build=None):
            def build_or_run(command, **kwargs):
                if command[0] != "cargo":
                    return original_run(command, **kwargs)
                assert command == ["cargo", "build", "--release", "--locked", "-p", "quarry-cli", "--bin", "quarry-bench", "--message-format=json"]
                assert kwargs["cwd"] == repo and kwargs["check"]
                builds.append(command)
                binary.write_text(cli)
                binary.chmod(0o700)
                if during_build:
                    during_build()
                return subprocess.CompletedProcess(command, 0, json.dumps({
                    "reason": "compiler-artifact", "target": {"name": "quarry-bench"}, "executable": str(binary),
                }) + "\n")
            with patch("sys.argv", [str(script), str(data), str(results)]), \
                 patch.dict("os.environ", {"QUARRY_RUNNER_SMOKE_FAIL": "1" if fail else ""}), \
                 patch("subprocess.run", side_effect=build_or_run), \
                 contextlib.redirect_stdout(io.StringIO()):
                return runner["main"]()
        rejected = repo / "results/rejected"
        for dirty in ("unstaged", "staged", "untracked"):
            changed = repo / "untracked.txt" if dirty == "untracked" else tracked
            changed.write_text("uncommitted source\n")
            if dirty == "staged":
                subprocess.run(["git", "add", str(changed)], cwd=repo, check=True)
            try:
                run(rejected)
            except RuntimeError as error:
                assert "clean checkout" in str(error)
            else:
                raise AssertionError(f"Runner accepted {dirty} source")
            assert not builds and not rejected.exists()
            if dirty == "untracked":
                changed.unlink()
            else:
                subprocess.run(["git", "restore", "--staged", "--worktree", str(changed)], cwd=repo, check=True)
        success = repo / "results/success"
        assert run(success) == 0
        assert len(builds) == 1
        manifest = (success / "results.json").read_bytes()
        evidence = json.loads(manifest)
        assert evidence["engine_revision"] == revision and evidence["engine_source_status"] == "clean"
        assert evidence["build_command"] == builds[0]
        assert (success / "quarry-bench").read_text() == cli
        assert evidence["binary_sha256"] == hashlib.sha256(cli.encode()).hexdigest()
        assert not (success / "benchmark-cli.patch").exists()
        assert len(evidence["runs"]) == 28 and evidence["sources_unchanged"]
        assert evidence["sources_before"] == evidence["sources_after"]
        assert all(source["sha256"] == hashlib.sha256(original).hexdigest()
                   for source in evidence["sources_after"].values())
        for entry in evidence["runs"]:
            assert entry["exit_code"] == 0
            if entry["case"] == "export":
                assert entry["output_bytes"] == len(b"generated output\n")
            assert "mock CLI success" in (success / entry["log"]).read_text()
            assert not (success / f"{entry['dataset']}-{entry['case']}").exists()
        try:
            run(success)
        except FileExistsError:
            pass
        else:
            raise AssertionError("Runner reused an existing results directory")
        assert len(builds) == 1
        assert (success / "results.json").read_bytes() == manifest
        failed = repo / "results/failed"
        assert run(failed, fail=True) == 1
        evidence = json.loads((failed / "results.json").read_text())
        failures = [entry for entry in evidence["runs"] if entry["exit_code"]]
        assert len(failures) == 2 and evidence["sources_unchanged"]
        for entry in failures:
            directory = failed / f"{entry['dataset']}-{entry['case']}"
            assert entry["exit_code"] == 7 and entry["case"] == "edit-save-as"
            assert (directory / "output.csv").read_bytes() == b"generated output\n"
            assert (directory / "failure.tmp").read_bytes() == b"failure evidence"
            assert "mock CLI failure" in (failed / entry["log"]).read_text()
        assert all(source.read_bytes() == original for source in data.iterdir())
        try:
            run(rejected, during_build=lambda: tracked.write_text("changed during build\n"))
        except RuntimeError as error:
            assert "clean checkout" in str(error)
        else:
            raise AssertionError("Runner accepted a checkout changed during build")
        assert not rejected.exists()
    print("Public benchmark runner smoke check passed.")


if __name__ == "__main__":
    main()
