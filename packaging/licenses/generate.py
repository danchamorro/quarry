#!/usr/bin/env python3
"""Produce a bounded notice inventory, or check its freshness and recorded review."""

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
SUPPORTED_TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
)
SHIPPING_MANIFESTS = {
    "aarch64-apple-darwin": ("apps/quarry-gpui/Cargo.toml",),
    "x86_64-unknown-linux-gnu": (
        "apps/quarry-gpui/Cargo.toml",
        "apps/quarry-cli/Cargo.toml",
    ),
    "aarch64-unknown-linux-gnu": (
        "apps/quarry-gpui/Cargo.toml",
        "apps/quarry-cli/Cargo.toml",
    ),
}
VERSION = "0.9.2"
RUST_RELEASE = "1.100.0"
STATUS = "collected"
ARTIFACT = HERE / "THIRD_PARTY_NOTICES.html"
MANIFEST = HERE / "manifest.json"


def target_paths(target=TARGET):
    if target not in SUPPORTED_TARGETS:
        raise ValueError(
            "No notice inventory for target "
            + target
            + "; available: "
            + ", ".join(SUPPORTED_TARGETS)
        )
    if target == TARGET:
        return ARTIFACT, MANIFEST, HERE / "reviewed.sha256"
    base = HERE / "targets" / target
    return base / "THIRD_PARTY_NOTICES.html", base / "manifest.json", base / "reviewed.sha256"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def workspace_manifests():
    pending = [ROOT / "Cargo.toml"]
    inspected, manifests = set(), set()
    while pending:
        manifest = pending.pop().resolve()
        if manifest in inspected:
            continue
        result = subprocess.run([
            "cargo", "metadata", "--no-deps", "--format-version", "1",
            "--offline", "--locked", "--manifest-path", str(manifest),
        ], cwd=ROOT, text=True, capture_output=True)
        if result.returncode:
            raise ValueError("Cannot read Cargo workspace: " + result.stderr.strip())
        metadata = json.loads(result.stdout)
        inspected.add(manifest)
        for package in metadata["packages"]:
            package_manifest = Path(package["manifest_path"]).resolve()
            manifests.add(package_manifest)
            inspected.add(package_manifest)
            # --no-deps omits excluded path packages, but exposes their resolved
            # paths. Inspect those packages recursively without registry access.
            pending.extend(Path(dependency["path"]) / "Cargo.toml"
                           for dependency in package["dependencies"]
                           if dependency.get("path"))
    return manifests


def inputs():
    paths = [ROOT / name for name in (
        "Cargo.lock", "Cargo.toml", "rust-toolchain.toml", "LICENSE-MIT", "LICENSE-APACHE",
        "scripts/generate-notices.sh", "packaging/licenses/generate.py",
        "packaging/licenses/about.toml", "packaging/licenses/supplemental.json",
        "packaging/licenses/AUDIT.md",
    )]
    paths += workspace_manifests()
    paths += list((HERE / "rust").glob("*"))
    return {str(p.relative_to(ROOT)): digest(p.read_bytes()) for p in sorted(paths)}


def supplemental():
    data = json.loads((HERE / "supplemental.json").read_text())
    for notice in data["notices"]:
        if digest(notice["text"].encode("utf-8")) != notice["sha256"]:
            raise ValueError("Supplemental notice hash mismatch: " + notice["source"])
        targets = notice.get("targets")
        if targets is not None and (not targets or any(t not in SUPPORTED_TARGETS for t in targets)):
            raise ValueError("Invalid target scope in supplemental notice: " + notice["source"])
    for asset in data["assets"]:
        targets = asset.get("targets")
        if targets is not None and (not targets or any(t not in SUPPORTED_TARGETS for t in targets)):
            raise ValueError("Invalid target scope in supplemental asset: " + asset["path"])
    return data


def check(target=TARGET):
    supplemental()
    artifact_path, manifest_path, _ = target_paths(target)
    manifest = json.loads(manifest_path.read_text())
    expected = {
        "cargo_about_version": VERSION, "target": target, "status": STATUS,
        "inputs": inputs(), "artifact_sha256": digest(artifact_path.read_bytes()),
    }
    if manifest != expected:
        raise ValueError(
            f"Notices for {target} are stale or modified; regenerate and review them."
        )
    print(f"Notice inventory ({target}): freshness and integrity verified. Review is checked separately.")


def package_id(package):
    return package["name"] + "@" + package["version"]


def record_applies(record, target):
    targets = record.get("targets")
    return targets is None or target in targets


def merge_cargo_about_outputs(raw_list):
    """Merge multiple cargo-about JSON graphs (e.g., GUI + CLI) into one deduplicated graph."""
    crates_by_id = {}
    licenses_by_key = {}
    for raw in raw_list:
        for crate_entry in raw["crates"]:
            crates_by_id.setdefault(package_id(crate_entry["package"]), crate_entry)
        for lic in raw["licenses"]:
            key = (lic["id"], lic["source_path"])
            merged = licenses_by_key.get(key)
            if merged is None:
                licenses_by_key[key] = {
                    "id": lic["id"],
                    "source_path": lic["source_path"],
                    "used_by": list(lic["used_by"]),
                }
            else:
                seen = {package_id(u["crate"]) for u in merged["used_by"]}
                for u in lic["used_by"]:
                    pid = package_id(u["crate"])
                    if pid not in seen:
                        merged["used_by"].append(u)
                        seen.add(pid)
    return {
        "crates": list(crates_by_id.values()),
        "licenses": list(licenses_by_key.values()),
    }


def render(raw, target=TARGET):
    packages = {package_id(c["package"]): c["package"] for c in raw["crates"]}
    extra = supplemental()
    notices = [r for r in extra["notices"] if record_applies(r, target)]
    assets = [r for r in extra["assets"] if record_applies(r, target)]
    selected = {}
    for record in notices + assets:
        ids = record.get("packages", [record.get("package")])
        if any(p not in packages for p in ids):
            raise ValueError("Supplemental package is absent: " + ", ".join(ids))
        if chosen := record.get("selected_license"):
            for p in ids:
                if chosen not in (packages[p]["license"] or "").split(" OR "):
                    raise ValueError("Selected license is not offered by " + p)
                selected[p] = chosen
        path = record.get("crate_file", record.get("path"))
        if path:
            source = Path(packages[ids[0]]["manifest_path"]).parent / path
            content = source.read_bytes()
            if digest(content) != record["sha256"]:
                raise ValueError("Locked crate supplement changed: " + str(source))
            if "bytes" in record and len(content) != record["bytes"]:
                raise ValueError("Locked crate supplement size mismatch: " + str(source))

    sections, gaps = [], []
    supplemented_packages = {
        pkg for notice in notices for pkg in notice.get("packages", [])
    }
    for license in raw["licenses"]:
        users = sorted(package_id(c["crate"]) for c in license["used_by"]
                       if c["crate"]["source"] is not None
                       and selected.get(package_id(c["crate"]), license["id"]) == license["id"])
        if not users:
            continue
        source = license["source_path"]
        if source is None:
            uncovered = [u for u in users if u not in supplemented_packages]
            if uncovered:
                raise ValueError(
                    f"Fallback package(s) missing supplemental notice for {target}: "
                    + ", ".join(uncovered)
                )
            gaps.append((license["id"], users, "No source notice file supplied by cargo-about"))
            continue
        source = Path(source)
        original = source.read_bytes().decode("utf-8")
        if "<copyright holders>" in original:
            uncovered = [u for u in users if u not in supplemented_packages]
            if uncovered:
                raise ValueError(
                    f"Placeholder notice package(s) missing supplemental notice for {target}: "
                    + ", ".join(uncovered)
                )
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
             "<title>Quarry third-party notices</title>",
             "<style>body{font:16px system-ui,sans-serif;max-width:960px;margin:40px auto;padding:0 24px;line-height:1.5}pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#f5f5f5;padding:18px}table{border-collapse:collapse;width:100%}td,th{text-align:left;padding:8px;border-bottom:1px solid #ddd}</style>",
             "<body><h1>Quarry third-party notices</h1>",
             "<p>This inventory preserves collected license terms and upstream notices. The source repository's packaging/licenses/AUDIT.md records the review scope and evidence. Inventory generation does not authorize a release.</p>",
             f"<p>Target: {target}. cargo-about {VERSION}. {len(packages)} locked packages, including Quarry packages. Build and development dependencies excluded. Quarry licenses are provided separately.</p>",
             "<h2>Tool fallback inventory</h2><p>The following tool-selected notices had no source file or contained a generic placeholder. Placeholder texts have been omitted. The supplemental section preserves source permissions and copyright statements collected during review; packaging/licenses/AUDIT.md records their coverage and remaining questions.</p><ul>"]
    for kind, users, reason in sorted(gaps):
        parts.append(f"<li>{esc(', '.join(users))}: {esc(kind)}. {esc(reason)}.</li>")
    parts += ["</ul><p>Rust 1.100.0 standard-library and compiler-runtime notices are provided separately in the adjacent Rust directory, including upstream copyright, license, and backtrace notices. Other toolchains, architectures, and operating systems need their own inventories. See packaging/licenses/AUDIT.md in the source repository for the review evidence.</p>",
              "<h2>Collected source notices</h2>"]
    for kind, users, source, original in sorted(sections):
        parts.append(f"<h3>{esc(kind)}: {esc(', '.join(users))}</h3><p>Source: {esc(source)}</p><pre>{esc(original)}</pre>")
    parts.append("<h2>Supplemental upstream and bundled-resource notices</h2>")
    for notice in notices:
        choice = f"<p>Selected license: {esc(notice['selected_license'])}</p>" if "selected_license" in notice else ""
        parts.append(f"<h3>{esc(', '.join(notice['packages']))}</h3>{choice}<p>Source: {esc(notice['source'])}<br>SHA-256: {notice['sha256']}</p><pre>{esc(notice['text'])}</pre>")
    if assets:
        parts.append("<h2>Bundled font byte inventory</h2><ul>")
        for asset in assets:
            parts.append(f"<li>{esc(asset['package'] + '/' + asset['path'])}: {asset['bytes']} bytes, SHA-256 {asset['sha256']}</li>")
        parts.append("</ul>")
    parts.append("<h2>Dependency inventory</h2><table><tr><th>Package</th><th>Declared license expression</th></tr>")
    for key, package in sorted(packages.items()):
        parts.append(f"<tr><td>{esc(key)}</td><td>{esc(package['license'] or 'Unspecified')}</td></tr>")
    parts.append("</table></body></html>\n")
    return "\n".join(parts).encode("utf-8")


def generate(binary, target=TARGET):
    version = subprocess.check_output([binary, "--version"], text=True).strip()
    if version != "cargo-about " + VERSION:
        raise ValueError("Expected cargo-about " + VERSION + ", got " + version)
    artifact_path, manifest_path, _ = target_paths(target)
    before = inputs()
    with tempfile.TemporaryDirectory(prefix="quarry-notices-") as directory:
        raw_graphs = []
        for idx, manifest_rel in enumerate(SHIPPING_MANIFESTS[target]):
            output = Path(directory) / f"notices-{idx}.json"
            subprocess.run([
                binary, "generate", "--locked", "--target", target,
                "-m", str(ROOT / manifest_rel),
                "-c", str(HERE / "about.toml"), "--fail", "--format", "json",
                "-o", str(output),
            ], cwd=ROOT, check=True)
            raw_graphs.append(json.loads(output.read_text()))
        rendered = render(merge_cargo_about_outputs(raw_graphs), target=target)
    if inputs() != before:
        raise ValueError("Notice inputs changed during generation; run again.")
    manifest = {"cargo_about_version": VERSION, "target": target, "status": STATUS,
                "inputs": before, "artifact_sha256": digest(rendered)}
    artifact_path.parent.mkdir(parents=True, exist_ok=True)
    artifact_path.write_bytes(rendered)
    manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    check(target=target)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="Offline freshness and integrity only")
    mode.add_argument("--release-check", action="store_true", help="Also require a matching recorded inventory review")
    parser.add_argument("--cargo-about", default="cargo-about", help="Path to cargo-about 0.9.2")
    parser.add_argument("--target", default=TARGET, help="Target triple for notice inventory")
    parser.add_argument("--all-targets", action="store_true", help="Operate on all supported packaging targets")
    parser.add_argument("--rust-release", default=RUST_RELEASE, help="Assert the compiler matches the runtime notices")
    args = parser.parse_args()
    try:
        if args.rust_release != RUST_RELEASE:
            raise ValueError("No runtime notice inventory for Rust " + args.rust_release + "; available: " + RUST_RELEASE)
        targets = SUPPORTED_TARGETS if args.all_targets else (args.target,)
        for target in targets:
            _, manifest_path, reviewed = target_paths(target)
            if args.check or args.release_check:
                check(target=target)
            else:
                generate(args.cargo_about, target=target)
            if args.release_check:
                if not reviewed.is_file() or reviewed.read_text().strip() != digest(manifest_path.read_bytes()):
                    raise ValueError(f"Notice inventory review required for {target}. See packaging/licenses/AUDIT.md.")
                print(f"Reviewed notice inventory matches ({target}). Candidate and publication gates are separate.")
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print("Notice check failed: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
