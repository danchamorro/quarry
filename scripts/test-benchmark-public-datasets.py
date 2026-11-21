#!/usr/bin/env python3
"""Smoke-check provenance, input/output digest enforcement, negative controls, and cleanup."""

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
        (repo / ".gitignore").write_text("/target/\n/data/\n/results/\n/fixtures/\n")
        tracked = repo / "Cargo.toml"
        tracked.write_text("committed source\n")
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        subprocess.run(["git", "add", "."], cwd=repo, check=True)
        subprocess.run(
            [
                "git",
                "-c",
                "user.name=Runner Smoke",
                "-c",
                "user.email=smoke@example.invalid",
                "commit",
                "-qm",
                "Initial fixture",
            ],
            cwd=repo,
            check=True,
        )
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()

        stale_binary = repo / "target/release/quarry-bench"
        stale_binary.parent.mkdir(parents=True)
        stale_binary.write_text(f"#!{sys.executable}\nraise SystemExit('stale binary executed')\n")
        stale_binary.chmod(0o700)

        binary = repo / "target/custom/release/quarry-bench"
        binary.parent.mkdir(parents=True)

        good_output = b'id,value\n1,"line1\nline2"\n2,unique\n'
        wrong_output = b"id,value\n999,corrupted\n"
        cli = f"#!{sys.executable}\n" + '''
import os, sys
from pathlib import Path
args = sys.argv[1:]
if args == ["--help"]:
    if os.environ.get("QUARRY_RUNNER_SMOKE_SUPPORTS_THREADS"):
        print("usage: quarry-bench [--threads N]")
    else:
        print("usage: quarry-bench")
    sys.exit(0)
assert "--join" not in args or args.count("--output-header") == 1, "join requires exactly one --output-header value"
if "--threads" in args and not os.environ.get("QUARRY_RUNNER_SMOKE_SUPPORTS_THREADS"):
    print("unknown option --threads", file=sys.stderr)
    sys.exit(2)
output = (Path(args[args.index("--output") + 1]) if "--output" in args else
          Path(args[2]) if args[0] == "sort-save-as" else None)
if output and "--cancel-after-bytes" not in args:
    if args[0] == "export" and os.environ.get("QUARRY_RUNNER_SMOKE_WRONG_OUTPUT"):
        output.write_bytes(b"id,value\\n999,corrupted\\n")
    elif args[0] == "duplicates":
        # Preserve header and first occurrence order only.
        output.write_bytes(b"id,value\\n1,\\"line1\\nline2\\"\\n2,unique\\n")
    else:
        output.write_bytes(b"id,value\\n1,\\"line1\\nline2\\"\\n2,unique\\n")
if args[0] == "edit-save-as" and os.environ.get("QUARRY_RUNNER_SMOKE_FAIL"):
    output.with_name("failure.tmp").write_bytes(b"failure evidence")
    print("mock CLI failure", file=sys.stderr)
    sys.exit(7)
print("mock CLI success")
'''

        data = repo / "data"
        data.mkdir()
        original = b'id,value\n1,"line1\nline2"\n1,"line1\nline2"\n2,unique\n'
        original_sha = hashlib.sha256(original).hexdigest()
        for config in runner["DATASETS"].values():
            (data / config[0]).write_bytes(original)

        fixtures_dir = repo / "fixtures"
        fixtures_dir.mkdir()
        expected_hashes_file = fixtures_dir / "expected-hashes.json"
        expected_hashes_file.write_text(
            json.dumps({name: original_sha for name in runner["DATASETS"]}, indent=2) + "\n"
        )

        good_output_sha = hashlib.sha256(good_output).hexdigest()
        expected_outputs_file = fixtures_dir / "expected-outputs.json"
        golden = {}
        for ds in runner["DATASETS"]:
            for case in runner["OUTPUT_EQUALITY"]:
                golden[f"{ds}/{case}"] = {
                    "output_sha256": good_output_sha,
                    "output_bytes": len(good_output),
                    "output_rows": 3,
                }
        expected_outputs_file.write_text(json.dumps(golden, indent=2) + "\n")

        original_run = subprocess.run
        builds = []

        def run(
            results,
            fail=False,
            wrong_output_control=False,
            use_default_hashes=False,
            expected_hashes_override=None,
            expected_outputs=None,
            threads=None,
            supports_threads=True,
            during_build=None,
        ):
            def build_or_run(command, **kwargs):
                if command[0] != "cargo":
                    return original_run(command, **kwargs)
                assert command == [
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
                assert kwargs["cwd"] == repo and kwargs["check"]
                builds.append(command)
                binary.write_text(cli)
                binary.chmod(0o700)
                if during_build:
                    during_build()
                return subprocess.CompletedProcess(
                    command,
                    0,
                    json.dumps(
                        {
                            "reason": "compiler-artifact",
                            "target": {"name": "quarry-bench"},
                            "executable": str(binary),
                        }
                    )
                    + "\n",
                )

            argv = [str(script), str(data), str(results)]
            if expected_hashes_override is not None:
                argv.extend(["--expected-hashes", str(expected_hashes_override)])
            elif not use_default_hashes:
                argv.extend(["--expected-hashes", str(expected_hashes_file)])
            if expected_outputs is not None:
                argv.extend(["--expected-outputs", str(expected_outputs)])
            if threads is not None:
                argv.extend(["--threads", str(threads)])

            env_patch = {
                "QUARRY_RUNNER_SMOKE_FAIL": "1" if fail else "",
                "QUARRY_RUNNER_SMOKE_WRONG_OUTPUT": "1" if wrong_output_control else "",
                "QUARRY_RUNNER_SMOKE_SUPPORTS_THREADS": "1" if supports_threads else "",
            }
            with patch("sys.argv", argv), \
                 patch.dict("os.environ", env_patch), \
                 patch("subprocess.run", side_effect=build_or_run), \
                 contextlib.redirect_stdout(io.StringIO()):
                return runner["main"]()

        # 1. Input SHA-256 mismatch and incomplete/unknown override maps must fail before building.
        rejected = repo / "results/rejected"
        try:
            run(rejected, use_default_hashes=True)
        except RuntimeError as error:
            assert "Input SHA-256 mismatch" in str(error)
        else:
            raise AssertionError("Runner accepted mismatched input SHA-256")
        assert not builds and not rejected.exists()

        incomplete_hashes = fixtures_dir / "incomplete-hashes.json"
        incomplete_hashes.write_text(json.dumps({"weather": original_sha, "typo": original_sha}) + "\n")
        try:
            run(rejected, expected_hashes_override=incomplete_hashes)
        except ValueError as error:
            assert "missing=" in str(error) and "unknown=" in str(error)
        else:
            raise AssertionError("Runner accepted incomplete/unknown expected hashes map")
        assert not builds and not rejected.exists()

        # 1b. --threads rejected cleanly when CLI binary does not advertise --threads support.
        try:
            run(rejected, threads=4, supports_threads=False)
        except RuntimeError as error:
            assert "does not support --threads" in str(error)
        else:
            raise AssertionError("Runner forwarded unsupported --threads flag")
        assert len(builds) == 1 and not rejected.exists()
        builds.clear()

        # 2. Dirty checkout rejection (unstaged, staged, untracked).
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

        # 3. Clean run with golden digests, duplicates-remove, metrics, and --threads pass-through.
        success = repo / "results/success"
        assert run(success, expected_outputs=expected_outputs_file, threads=4) == 0
        assert len(builds) == 1
        manifest = (success / "results.json").read_bytes()
        evidence = json.loads(manifest)
        assert evidence["engine_revision"] == revision and evidence["engine_source_status"] == "clean"
        assert evidence["build_command"] == builds[0]
        assert evidence["threads"] == 4
        assert (success / "quarry-bench").read_text() == cli
        assert evidence["binary_sha256"] == hashlib.sha256(cli.encode()).hexdigest()
        assert len(evidence["runs"]) == 30 and evidence["sources_unchanged"]
        assert evidence["sources_before"] == evidence["sources_after"]
        assert evidence["rusage_children_max_rss_bytes"] >= 0
        assert all(source["sha256"] == original_sha for source in evidence["sources_after"].values())

        cases_seen = {entry["case"] for entry in evidence["runs"]}
        assert "duplicates-remove" in cases_seen

        for entry in evidence["runs"]:
            assert entry["exit_code"] == 0
            assert "output_verification_error" not in entry
            assert entry["child_max_rss_bytes"] >= 0
            assert entry["peak_scratch_bytes"] >= 0
            assert "--threads" in entry["command"] and "4" in entry["command"]
            if entry["case"] in runner["OUTPUT_EQUALITY"]:
                assert entry["output_bytes"] == len(good_output)
                assert entry["output_rows"] == 3  # header + 2 logical rows (1 multiline quoted)
                assert entry["output_sha256"] == good_output_sha
                assert entry["output_equality"] == runner["OUTPUT_EQUALITY"][entry["case"]]
            assert "mock CLI success" in (success / entry["log"]).read_text()
            assert not (success / f"{entry['dataset']}-{entry['case']}").exists()

        # 4. Existing results directory must not be overwritten.
        try:
            run(success)
        except FileExistsError:
            pass
        else:
            raise AssertionError("Runner reused an existing results directory")
        assert len(builds) == 1
        assert (success / "results.json").read_bytes() == manifest

        # 5. Deliberately failing smoke control: CLI exits 0 with wrong output -> runner MUST fail.
        wrong_out_dir = repo / "results/wrong-output-control"
        assert run(wrong_out_dir, wrong_output_control=True, expected_outputs=expected_outputs_file) == 1
        wrong_evidence = json.loads((wrong_out_dir / "results.json").read_text())
        mismatches = [entry for entry in wrong_evidence["runs"] if "output_verification_error" in entry]
        assert len(mismatches) == 2
        for entry in mismatches:
            assert entry["case"] == "export"
            assert entry["exit_code"] == 0
            assert entry["output_sha256"] == hashlib.sha256(wrong_output).hexdigest()
            assert entry["output_rows"] == 2
            assert (wrong_out_dir / f"{entry['dataset']}-export" / "output.csv").read_bytes() == wrong_output

        # 6. Non-zero CLI exit preserves run directory & failure evidence.
        failed = repo / "results/failed"
        assert run(failed, fail=True) == 1
        evidence = json.loads((failed / "results.json").read_text())
        failures = [entry for entry in evidence["runs"] if entry["exit_code"]]
        assert len(failures) == 2 and evidence["sources_unchanged"]
        for entry in failures:
            directory = failed / f"{entry['dataset']}-{entry['case']}"
            assert entry["exit_code"] == 7 and entry["case"] == "edit-save-as"
            assert (directory / "output.csv").read_bytes() == good_output
            assert (directory / "failure.tmp").read_bytes() == b"failure evidence"
            assert "mock CLI failure" in (failed / entry["log"]).read_text()

        assert all(source.read_bytes() == original for source in data.iterdir())

        # 7. Checkout mutated during build must fail before running workloads.
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
