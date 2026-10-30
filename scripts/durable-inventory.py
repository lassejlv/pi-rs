#!/usr/bin/env python3
"""Inventory the pinned native durable port. No reference source is copied."""
import argparse
import hashlib
import json
import pathlib
import re
import subprocess

PIN = "5e63241d0294ed3fcfc4388c737a8ea27e09c74e"
ROOT = pathlib.Path(__file__).resolve().parents[1]
CHORD_MODULES = {
    "json.ts", "watch.ts", "context/index.ts", "output.ts",
    "delta/index.ts", "delta/diff.ts", "delta/draft.ts", "delta/tracker.ts",
    "delta/revision-validator.ts", "delta/apply-immutable-trusted.ts",
    "sqlite/index.ts", "sqlite/database.ts", "sqlite/schema.ts",
    "sqlite/migrations.ts", "sqlite/node.ts", "sqlite/cloudflare.ts",
}
CHORD_TESTS = {
    "json.test.ts", "watch.test.ts", "context.test.ts", "output.test.ts",
    "output-skip.test.ts", "delta.test.ts", "delta-diff.test.ts",
    "delta-clone.test.ts", "delta-apply-immutable.test.ts",
    "delta-tracker/tracker.test.ts", "delta-tracker/retention.test.ts",
    "sqlite/migrations.test.ts", "sqlite/node.test.ts", "sqlite/cloudflare.test.ts",
}


def inventory(reference):
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=reference, text=True).strip()
    if commit != PIN:
        raise SystemExit(f"Reference HEAD {commit} differs from pinned {PIN}")
    rows = []
    for package in ("chord", "durable"):
        base = reference / "packages" / package
        for directory in ("src", "test", "test-workers"):
            for path in sorted((base / directory).rglob("*.ts")):
                source = path.read_text()
                rows.append({
                    "path": str(path.relative_to(reference)),
                    "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                    "lines": len(source.splitlines()),
                    "declarations": re.findall(r"\bexport\s+(?:async\s+)?(?:type|interface|class|function|const)\s+(\w+)", source),
                    "reexports": [
                        {"from": m[2], "names": [name.strip() for name in m[1].split(",") if name.strip()]}
                        for m in re.finditer(r"export\s+(?:type\s+)?\{([^}]+)\}\s+from\s+[\"']([^\"']+)[\"']", source)
                    ],
                    "cases": [
                        {"line": source.count("\n", 0, m.start()) + 1, "name": m[2]}
                        for m in re.finditer(r"\b(?:it|test)(?:\.(?:skip|only|todo))?\s*\(\s*([\"'`])(.*?)\1", source)
                    ],
                    "runnerCases": [
                        {"line": source.count("\n", 0, m.start()) + 1, "name": m[2]}
                        for m in re.finditer(r"\bcreateCase\(\s*options,\s*([\"'`])(.*?)\1", source)
                    ],
                })
    manifests = {p: json.loads((reference / "packages" / p / "package.json").read_text()) for p in ("chord", "durable")}
    return {"reference": PIN, "packages": {p: {key: manifest.get(key) for key in ("version", "exports", "dependencies")} for p, manifest in manifests.items()}, "files": rows}


def parity_map(result, check):
    target = ROOT / "porting/durable/parity-map.json"
    previous = json.loads(target.read_text()) if target.exists() else {"reference": PIN, "items": []}
    if previous["reference"] != PIN:
        raise SystemExit("Parity map belongs to another reference")
    existing = {item["reference"]: item for item in previous["items"]}
    items = []
    for row in result["files"]:
        _, package, directory, local = row["path"].split("/", 3)
        required = package == "durable" or (directory == "src" and local in CHORD_MODULES) or (directory == "test" and local in CHORD_TESTS)
        if not required:
            continue
        item = existing.get(row["path"], {"reference": row["path"], "status": "pending", "rust": [], "evidence": []})
        if item["status"] not in ("pending", "partial", "verified"):
            raise SystemExit(f"Invalid parity state for {row['path']}")
        for rust in item["rust"]:
            if not (ROOT / rust).is_file():
                raise SystemExit(f"Missing Rust mapping: {rust}")
        if item["status"] == "verified" and not (item["rust"] and item["evidence"]):
            raise SystemExit(f"Verified module lacks mapped code and evidence: {row['path']}")
        items.append(item)
    rendered = json.dumps({"reference": PIN, "items": items}, indent=2) + "\n"
    if check:
        if not target.exists() or target.read_text() != rendered:
            raise SystemExit("Parity map is missing reference modules or tests; regenerate and review")
    else:
        target.write_text(rendered)
    counts = {status: sum(item["status"] == status for item in items) for status in ("pending", "partial", "verified")}
    print(f"Parity map: {counts}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reference", type=pathlib.Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    result = inventory(args.reference)
    target = ROOT / "porting/durable/reference-inventory.json"
    rendered = json.dumps(result, indent=2) + "\n"
    if args.check:
        if target.read_text() != rendered:
            raise SystemExit("Durable inventory differs from the reference; regenerate and review")
    else:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(rendered)
    parity_map(result, args.check)
    for package in ("chord", "durable"):
        files = [r for r in result["files"] if r["path"].startswith(f"packages/{package}/")]
        print(f"{package}: {len(files)} files, {sum(r['lines'] for r in files)} lines, {sum(len(r['cases']) for r in files)} literal test cases")


if __name__ == "__main__":
    main()
