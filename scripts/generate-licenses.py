#!/usr/bin/env python3
"""Generate the checked-in license inventory and website acknowledgments page.

Requires cargo-about 0.9.2 (install with --locked --features cli).
No application build, native download, or access to application data is needed.
"""

import argparse
import hashlib
import html
import json
from pathlib import Path
import subprocess
import tempfile
import tomllib
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parents[1]


def read_json(path):
    return json.loads(path.read_text())


def digest(data):
    return hashlib.sha256(data).hexdigest()


def checked_text(record):
    data = (ROOT / record["path"]).read_bytes()
    if digest(data) != record["sha256"]:
        raise ValueError(f"Review changed license text: {record['path']}")
    return data.decode(record.get("encoding", "utf-8"))


def crate_id(package):
    return f"{package['name']}@{package['version']}"


def verify_native(metadata):
    packages = {p["name"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    for name in ("spandsp", "spandsp-sys"):
        package = packages[name]
        if package["version"] != "0.2.3":
            raise ValueError("Review SpanDSP native notices after a version change")
        if "v150" in nodes[package["id"]]["features"]:
            raise ValueError("GPLv2-only SpanDSP v150 feature must remain disabled")
    vendor = Path(packages["spandsp-sys"]["manifest_path"]).parent / "vendor"
    if (vendor / "COPYING").read_bytes() != (ROOT / "licenses/native/spandsp/COPYING").read_bytes():
        raise ValueError("SpanDSP COPYING differs from the reviewed notice")
    pj = ROOT / "crates/faxe-pj-sys/pjproject"
    revision = subprocess.check_output(["git", "-C", str(pj), "rev-parse", "HEAD"], text=True).strip()
    if revision != "5a457451fa2712ba18e12b01738e8ff3af2b26fd":
        raise ValueError("Review PJPROJECT native notices after a revision change")
    if (pj / "COPYING").read_bytes() != (ROOT / "licenses/native/pjproject/COPYING").read_bytes():
        raise ValueError("PJPROJECT COPYING differs from the reviewed notice")
    if 'pdfium-8044' not in (ROOT / "crates/faxe-engine/build.rs").read_text():
        raise ValueError("Review PDFium artifact notices after a version change")


def inventory(raw, metadata):
    verify_native(metadata)
    supplements = {}
    for record in read_json(ROOT / "licenses/supplemental.json"):
        text = checked_text(record)
        for package in record["used_by"]:
            supplements[(package, record["license"])] = (text, record["source"])

    groups = {}
    covered = set()
    for license in raw["licenses"]:
        for used in license["used_by"]:
            package = crate_id(used["crate"])
            text = license["text"]
            source = None
            if license["id"] == "MIT" and not license["source_path"] and "<copyright holders>" in text:
                # cargo-about can fall back to SPDX's generic MIT template when
                # crates omit license files. Never publish missing attribution.
                text, source = supplements[(package, "MIT")]
            if package.startswith("faxe-") and license["id"] == "GPL-3.0-only":
                text = (ROOT / "LICENSE").read_text()
            key = (license["id"], text)
            group = groups.setdefault(key, {
                "id": license["id"], "name": license["name"], "text": text,
                "used_by": [], "sources": [],
            })
            group["used_by"].append(package)
            if source:
                group["sources"].append(source)
            covered.add(package)

    crates = []
    for entry in raw["crates"]:
        p = entry["package"]
        if entry["license"] in ("Unknown", "Ignore") or crate_id(p) not in covered:
            raise ValueError(f"Missing license for {crate_id(p)}")
        crates.append({key: p[key] for key in ("name", "version", "license", "repository")})
    groups = list(groups.values())
    for group in groups:
        group["used_by"] = sorted(set(group["used_by"]))
        group["sources"] = sorted(set(group["sources"]))
    groups.sort(key=lambda g: (g["id"], g["used_by"]))

    native = read_json(ROOT / "licenses/native.json")
    for library in native:
        library["texts"] = [dict(file=record["path"], text=checked_text(record))
                            for record in library["files"]]
    return {
        "schema_version": 1,
        "generator": "cargo-about 0.9.2 + scripts/generate-licenses.py",
        "cargo_lock_sha256": digest((ROOT / "Cargo.lock").read_bytes()),
        "targets": tomllib.loads((ROOT / "about.toml").read_text())["targets"],
        "scope": "Workspace dependencies for configured desktop targets, including build dependencies; excluding test-only dependencies. Native notices record the current artifact versions; release packagers must check libraries actually bundled.",
        "crates": sorted(crates, key=lambda c: (c["name"], c["version"])),
        "licenses": groups,
        "native": native,
    }


def link(url, label):
    if not url or urlparse(url).scheme not in ("http", "https"):
        return html.escape(label)
    return f'<a href="{html.escape(url, quote=True)}">{html.escape(label)}</a>'


def render(data):
    e = html.escape
    sections = []
    for index, native in enumerate(data["native"]):
        texts = "".join(f'<h3>{e(Path(t["file"]).name)}</h3><pre>{e(t["text"])}</pre>'
                        for t in native["texts"])
        sections.append(f'<section id="native-{index}"><h2>{e(native["name"])}</h2>'
                        f'<p>{e(native["version"])} · {e(native["license"])}</p>'
                        f'<p>{link(native["source"], "Project and licensing information")}</p>'
                        f'<p>{e(native.get("display_note", native["notes"]))}</p>{texts}</section>')
    for index, license in enumerate(data["licenses"]):
        used = ", ".join(e(p.replace("@", " ")) for p in license["used_by"])
        sections.append(f'<section id="license-{index}"><h2>{e(license["name"])}</h2>'
                        f'<p class="used-by">{used}</p><pre>{e(license["text"])}</pre></section>')
    rows = []
    for crate in data["crates"]:
        package = crate_id(crate)
        anchors = [f'<a href="#license-{i}">{e(l["id"])}</a>'
                   for i, l in enumerate(data["licenses"]) if package in l["used_by"]]
        rows.append(f'<tr><td>{link(crate["repository"], crate["name"])}</td>'
                    f'<td>{e(crate["version"])}</td><td>{", ".join(anchors)}</td></tr>')
    template = (ROOT / "licenses/page.html.in").read_text()
    return template.replace("{{SECTIONS}}", "\n".join(sections)).replace("{{CRATES}}", "\n".join(rows))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cargo-about", default="cargo-about", help="Path to cargo-about 0.9.2")
    parser.add_argument("--check", action="store_true", help="Fail if checked-in output is stale")
    parser.add_argument("--check-page", action="store_true",
                        help="Check website HTML against the inventory and lockfile without cargo-about")
    args = parser.parse_args()
    if args.check_page:
        data = read_json(ROOT / "licenses/manifest.json")
        if data["cargo_lock_sha256"] != digest((ROOT / "Cargo.lock").read_bytes()):
            raise ValueError("License inventory is stale; regenerate after Cargo.lock changes")
        if (ROOT / "web/public/licenses.html").read_bytes() != render(data).encode("utf-8"):
            raise ValueError("Website acknowledgments are stale; regenerate the license page")
        print("Verified website acknowledgments against the inventory and Cargo.lock.")
        return
    version = subprocess.check_output([args.cargo_about, "--version"], text=True).strip()
    if version != "cargo-about 0.9.2":
        raise ValueError(f"Expected cargo-about 0.9.2, got {version}")
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--frozen", "--format-version", "1"], cwd=ROOT, text=True))
    with tempfile.TemporaryDirectory(prefix="faxe-licenses-") as directory:
        output = Path(directory) / "about.json"
        subprocess.run([args.cargo_about, "generate", "--workspace", "--frozen", "--fail",
                        "--format", "json", "-o", str(output)], cwd=ROOT, check=True)
        data = inventory(read_json(output), metadata)
    outputs = {
        ROOT / "licenses/manifest.json": json.dumps(data, indent=2, ensure_ascii=False) + "\n",
        ROOT / "web/public/licenses.html": render(data),
    }
    for path, content in outputs.items():
        if args.check:
            if not path.exists() or path.read_bytes() != content.encode("utf-8"):
                raise ValueError(f"Regenerate stale {path.relative_to(ROOT)}")
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
    print(f"{'Verified' if args.check else 'Generated'} notices for {len(data['crates'])} crates "
          f"and {len(data['native'])} native library groups.")


if __name__ == "__main__":
    main()
