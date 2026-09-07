#!/usr/bin/env python3
"""Produce a bounded draft notice inventory, or check its repository freshness."""

import argparse
import hashlib
import html
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
HERE = ROOT / "packaging/licenses"
TARGET = "aarch64-apple-darwin"
VERSION = "0.9.2"
STATUS = "draft-unresolved"
ARTIFACT = HERE / "THIRD_PARTY_NOTICES.html"
MANIFEST = HERE / "manifest.json"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def inputs():
    paths = [ROOT / name for name in (
        "Cargo.lock", "Cargo.toml", "LICENSE-MIT", "LICENSE-APACHE",
        "scripts/generate-notices.sh", "packaging/licenses/generate.py",
        "packaging/licenses/about.toml", "packaging/licenses/supplemental.json",
        "packaging/licenses/AUDIT.md",
    )]
    paths += list(ROOT.glob("apps/*/Cargo.toml"))
    paths += list(ROOT.glob("crates/*/Cargo.toml"))
    return {str(p.relative_to(ROOT)): digest(p.read_bytes()) for p in sorted(paths)}


def supplemental():
    data = json.loads((HERE / "supplemental.json").read_text())
    for notice in data["notices"]:
        if digest(notice["text"].encode("utf-8")) != notice["sha256"]:
            raise ValueError("Supplemental notice hash mismatch: " + notice["source"])
    return data


def check():
    supplemental()
    manifest = json.loads(MANIFEST.read_text())
    expected = {
        "cargo_about_version": VERSION, "target": TARGET, "status": STATUS,
        "inputs": inputs(), "artifact_sha256": digest(ARTIFACT.read_bytes()),
    }
    if manifest != expected:
        raise ValueError("Draft notices are stale or modified; regenerate and review them.")
    print("Draft notices: freshness and integrity verified. Legal completeness remains unresolved.")


def package_id(package):
    return package["name"] + "@" + package["version"]


def render(raw):
    packages = {package_id(c["package"]): c["package"] for c in raw["crates"]}
    extra = supplemental()
    for record in extra["notices"] + extra["assets"]:
        ids = record.get("packages", [record.get("package")])
        if any(p not in packages for p in ids):
            raise ValueError("Supplemental package is absent; re-audit supplemental notices.")
        path = record.get("crate_file", record.get("path"))
        if path:
            source = Path(packages[ids[0]]["manifest_path"]).parent / path
            content = source.read_bytes()
            if digest(content) != record["sha256"]:
                raise ValueError("Locked crate supplement changed: " + str(source))
            if "bytes" in record and len(content) != record["bytes"]:
                raise ValueError("Locked crate supplement size mismatch: " + str(source))

    sections, gaps = [], []
    for license in raw["licenses"]:
        users = sorted(package_id(c["crate"]) for c in license["used_by"]
                       if c["crate"]["source"] is not None)
        if not users:
            continue
        source = license["source_path"]
        if source is None:
            gaps.append((license["id"], users, "No source notice file supplied by cargo-about"))
            continue
        source = Path(source)
        original = source.read_bytes().decode("utf-8")
        if "<copyright holders>" in original:
            gaps.append((license["id"], users, "Source file contains a generic copyright placeholder"))
            continue
        origin = None
        for key, package in packages.items():
            try:
                relative = source.relative_to(Path(package["manifest_path"]).parent)
                origin = key + "/" + str(relative)
                break
            except ValueError:
                pass
        if origin is None:
            raise ValueError("Notice source is outside the inventoried crates: " + str(source))
        sections.append((license["id"], users, origin, original))

    esc = html.escape
    parts = ["<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">",
             "<title>Quarry draft third-party notices</title>",
             "<style>body{font:16px system-ui,sans-serif;max-width:960px;margin:40px auto;padding:0 24px;line-height:1.5}pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#f5f5f5;padding:18px}table{border-collapse:collapse;width:100%}td,th{text-align:left;padding:8px;border-bottom:1px solid #ddd}.draft{border:3px solid #9b3b00;padding:20px;background:#fff5e8}</style>",
             "<body><h1>Quarry third-party notices</h1>",
             "<div class=\"draft\"><strong>DRAFT: incomplete notice audit. Not release clearance.</strong><p>This dependency inventory preserves collected notices but has unresolved coverage gaps. Freshness checks do not establish legal completeness.</p></div>",
             f"<p>Target: {TARGET}. cargo-about {VERSION}. {len(packages)} locked packages, including Quarry packages. Build and development dependencies excluded. Quarry licenses are provided separately.</p>",
             "<h2>Unresolved coverage</h2><p>The following tool-selected notices had no source file or contained a generic placeholder. Placeholder texts have been omitted. Supplemental upstream texts below do not automatically close these gaps.</p><ul>"]
    for kind, users, reason in sorted(gaps):
        parts.append(f"<li>{esc(', '.join(users))}: {esc(kind)}. {esc(reason)}.</li>")
    parts += ["</ul><p>Additional manual gates: AccessKit Chromium and kurbo attribution; objc2 permission texts and Apple SDK derived code; embedded font copyright metadata; source-level and generated-code attribution; Rust standard library and compiler runtime; native libraries and platform SDK requirements. Other target architectures and operating systems need separate inventories. See packaging/licenses/AUDIT.md in the source repository for the detailed audit.</p>",
              "<h2>Collected source notices</h2>"]
    for kind, users, source, original in sorted(sections):
        parts.append(f"<h3>{esc(kind)}: {esc(', '.join(users))}</h3><p>Source: {esc(source)}</p><pre>{esc(original)}</pre>")
    parts.append("<h2>Supplemental upstream and bundled-resource notices</h2>")
    for notice in extra["notices"]:
        parts.append(f"<h3>{esc(', '.join(notice['packages']))}</h3><p>Source: {esc(notice['source'])}<br>SHA-256: {notice['sha256']}</p><pre>{esc(notice['text'])}</pre>")
    parts.append("<h2>Bundled font byte inventory</h2><ul>")
    for asset in extra["assets"]:
        parts.append(f"<li>{esc(asset['package'] + '/' + asset['path'])}: {asset['bytes']} bytes, SHA-256 {asset['sha256']}</li>")
    parts.append("</ul><h2>Dependency inventory</h2><table><tr><th>Package</th><th>Declared license expression</th></tr>")
    for key, package in sorted(packages.items()):
        parts.append(f"<tr><td>{esc(key)}</td><td>{esc(package['license'] or 'Unspecified')}</td></tr>")
    parts.append("</table></body></html>\n")
    return "\n".join(parts).encode("utf-8")


def generate(binary):
    version = subprocess.check_output([binary, "--version"], text=True).strip()
    if version != "cargo-about " + VERSION:
        raise ValueError("Expected cargo-about " + VERSION + ", got " + version)
    before = inputs()
    with tempfile.TemporaryDirectory(prefix="quarry-notices-") as directory:
        output = Path(directory) / "notices.json"
        subprocess.run([
            binary, "generate", "--locked", "--target", TARGET,
            "-m", str(ROOT / "apps/quarry-egui/Cargo.toml"),
            "-c", str(HERE / "about.toml"), "--fail", "--format", "json",
            "-o", str(output),
        ], cwd=ROOT, check=True)
        rendered = render(json.loads(output.read_text()))
    if inputs() != before:
        raise ValueError("Notice inputs changed during generation; run again.")
    manifest = {"cargo_about_version": VERSION, "target": TARGET, "status": STATUS,
                "inputs": before, "artifact_sha256": digest(rendered)}
    ARTIFACT.write_bytes(rendered)
    MANIFEST.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    check()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="Offline freshness and integrity only")
    mode.add_argument("--release-check", action="store_true", help="Also require resolved audit gates")
    parser.add_argument("--cargo-about", default="cargo-about", help="Path to cargo-about 0.9.2")
    parser.add_argument("--target", default=TARGET, help="Assert the packaging target matches this inventory")
    args = parser.parse_args()
    try:
        if args.target != TARGET:
            raise ValueError("No notice inventory for target " + args.target + "; available: " + TARGET)
        if args.check or args.release_check:
            check()
        else:
            generate(args.cargo_about)
        if args.release_check:
            raise ValueError("Release notice audit is unresolved. See packaging/licenses/AUDIT.md.")
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print("Notice check failed: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
