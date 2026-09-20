#!/usr/bin/env python3
"""Smoke-check runner evidence and cleanup using tiny inputs and a fake CLI."""

import contextlib
import hashlib
import io
import json
from pathlib import Path
import runpy
import shutil
import sys
import tempfile
from unittest.mock import patch


def main():
    with tempfile.TemporaryDirectory(prefix="quarry-runner-smoke-") as temporary:
        repo = Path(temporary)
        script = repo / "scripts/benchmark-public-datasets.py"
        script.parent.mkdir()
        shutil.copyfile(Path(__file__).with_name(script.name), script)
        runner = runpy.run_path(str(script))
        binary = repo / "target/release/quarry-bench"
        binary.parent.mkdir(parents=True)
        binary.write_text(f"#!{sys.executable}\n" + '''
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
''')
        binary.chmod(0o700)
        data = repo / "data"
        data.mkdir()
        original = b"id,value\n1,tiny\n"
        for config in runner["DATASETS"].values():
            (data / config[0]).write_bytes(original)
        def run(results, fail=False):
            with patch("sys.argv", [str(script), str(data), str(results)]), \
                 patch.dict("os.environ", {"QUARRY_RUNNER_SMOKE_FAIL": "1" if fail else ""}), \
                 patch("subprocess.check_output", side_effect=lambda *a, **k: "smoke-revision\n" if k.get("text") else b""), \
                 contextlib.redirect_stdout(io.StringIO()):
                return runner["main"]()
        success = repo / "success"
        assert run(success) == 0
        manifest = (success / "results.json").read_bytes()
        evidence = json.loads(manifest)
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
        assert (success / "results.json").read_bytes() == manifest
        failed = repo / "failed"
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
    print("Public benchmark runner smoke check passed.")


if __name__ == "__main__":
    main()
