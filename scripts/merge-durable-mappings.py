#!/usr/bin/env python3
"""Propose parity-map.json updates from porting/durable/mappings/*.json.

A reference file is `verified` only when every claim about it holds: each upstream case is native-passing
(a mapped Rust test), embedded-passing (the file ran unchanged through the embedded runner and all cases
passed), or a justified n/a (a stated reason); file-level mappings must say `verified`, module audits must
say `verified`. Anything else is `partial`, with the reason. A reference no mapping mentions is unchanged.

  merge-durable-mappings.py            dry run: print the proposed changes; write nothing
  merge-durable-mappings.py --write    apply them to porting/durable/parity-map.json

`scripts/durable-inventory.py <pi-mono> --check` must keep passing: the written file uses its rendering
(indent 2, trailing newline), only statuses pending/partial/verified, and existing Rust files.
"""
import argparse
import collections
import json
import os
import re
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(__file__), ".."))
MAPPINGS = os.path.join(ROOT, "porting", "durable", "mappings")
PARITY = os.path.join(ROOT, "porting", "durable", "parity-map.json")
RUNNER = "crates/pi-durable-jsrt/examples/embedded_stock.rs"


def rust_file(path):
    """The file of a `path::test_name` mapping when it exists in the repository, else None."""
    path = path.split("::", 1)[0]
    return path if re.fullmatch(r"[\w./+-]+", path) and os.path.isfile(os.path.join(ROOT, path)) else None


class Claims:
    def __init__(self):
        self.problems = []  # reasons the file is not verified
        self.holds = 0
        self.rust = []
        self.evidence = []
        self.embedded_passed = False  # every case passed unchanged in the embedded runner
        self.embedded_na = None  # why the file is not run embedded at all (justified n/a)
        self.cases = []  # (mapping, case) awaiting the embedded verdict

    def ok(self, source, rust=(), note=None):
        self.holds += 1
        self.rust.extend(rust)
        self.evidence.append(f"porting/durable/mappings/{source}")

    def bad(self, source, reason):
        self.problems.append(f"{source}: {reason}")


def gather():
    claims = collections.defaultdict(Claims)
    for name in sorted(os.listdir(MAPPINGS)):
        if not name.endswith(".json"):
            continue
        with open(os.path.join(MAPPINGS, name)) as handle:
            document = json.load(handle)
        if name == "module-audit.json":
            for module in document["modules"]:
                claim = claims[module["reference"]]
                if module["verdict"] == "verified":
                    claim.ok(name, [r for r in map(rust_file, module.get("rust", [])) if r])
                else:
                    claim.bad(name, module.get("reason", "module audit is partial"))
            continue
        if name == "utf16-boundaries.json":
            continue  # boundary decisions, not per-reference verdicts
        for entry in embedded_entries(name, document):
            reference = entry.get("test") or entry.get("reference")
            if not reference:
                continue
            claim = claims[reference]
            # An embedded mapping's entry with `rust` maps a source file (status/rust/evidence), not a suite run.
            if name.startswith("embedded-") and "rust" not in entry:
                embedded(claim, name, entry)
            elif "cases" in entry:
                for case in entry["cases"]:
                    claim.cases.append((name, case))
            elif "status" in entry:
                rust = [r for r in map(rust_file, entry.get("rust") or []) if r]
                if entry["status"] == "verified":
                    claim.ok(name, rust)
                    claim.evidence.extend(entry.get("evidence") or [])
                else:
                    claim.bad(name, entry.get("note") or f"mapped as {entry['status']}")
        for reference, entry in (document.get("notApplicable") or {}).items() if isinstance(document.get("notApplicable"), dict) else []:
            claims[entry["test"]].ok(name)
            claims[entry["test"]].embedded_na = entry["reason"]
            claims[entry["test"]].evidence.append(f"n/a: {entry['reason']}")
    for claim in claims.values():
        settle_cases(claim)
    return claims


def embedded_entries(name, document):
    for key in ("files", "tests"):
        value = document.get(key)
        if isinstance(value, dict):
            yield from value.values()
        elif isinstance(value, list):
            yield from value


def embedded(claim, name, entry):
    suite = entry.get("test") or entry.get("reference")
    modes = entry.get("modes")
    if isinstance(modes, dict):
        modes = list(modes.values())
    passing = entry.get("success", entry.get("status") == "verified")
    if modes:
        passing = passing and all(
            not mode.get("failed") and mode.get("passed", 0) + mode.get("skipped", 0) == mode.get("total", -1)
            for mode in modes
        )
    if passing and entry.get("status", "verified") == "verified":
        claim.embedded_passed = True
        claim.ok(name)
        claim.evidence.append(f"{RUNNER} --suite={suite}")
    else:
        claim.bad(name, entry.get("note") or "embedded run did not pass every case")


def settle_cases(claim):
    for source, case in claim.cases:
        kind = case.get("class")
        if kind == "native":
            rust = rust_file(case.get("rust", ""))
            if rust:
                claim.ok(source, [rust])
            else:
                claim.bad(source, f"native case '{case['name']}' has no existing Rust test ({case.get('rust')!r})")
        elif kind == "embedded-only":
            if claim.embedded_passed or claim.embedded_na:
                claim.ok(source)
            else:
                claim.bad(source, f"embedded-only case '{case['name']}' has no passing embedded run")
        elif kind == "n/a":
            if case.get("note"):
                claim.ok(source)
                claim.evidence.append(f"n/a: {case['name']}: {case['note']}")
            else:
                claim.bad(source, f"n/a case '{case['name']}' has no reason")
        else:
            claim.bad(source, f"case '{case.get('name')}' has class {kind!r}")


def unique(values):
    return list(dict.fromkeys(values))


def propose(items, claims):
    changes, unknown = [], []
    known = {item["reference"] for item in items}
    for reference in sorted(claims):
        if reference not in known:
            unknown.append(reference)
    updated = []
    for item in items:
        claim = claims.get(item["reference"])
        if claim is None or not (claim.holds or claim.problems):
            updated.append(item)
            continue
        status = "verified" if claim.holds and not claim.problems else "partial"
        rust = unique(item["rust"] + claim.rust)
        evidence = unique(item["evidence"] + claim.evidence)
        if status == "verified" and not (rust and evidence):
            status = "partial"
            claim.problems.append(
                "every claim holds or is a justified n/a, but the parity map only accepts verified with a mapped "
                "Rust file and evidence; there is no Rust counterpart"
            )
        new = {**item, "status": status, "rust": rust, "evidence": evidence}
        if status == "partial" and claim.problems:
            new["reason"] = "; ".join(unique(claim.problems))
        elif "reason" in new:
            del new["reason"]
        if new != item:
            changes.append((item, new))
        updated.append(new)
    return updated, changes, unknown


def invariants(items):
    """What `scripts/durable-inventory.py --check` demands of every item."""
    problems = []
    for item in items:
        if item["status"] not in ("pending", "partial", "verified"):
            problems.append(f"{item['reference']}: invalid status {item['status']}")
        for rust in item["rust"]:
            if not os.path.isfile(os.path.join(ROOT, rust)):
                problems.append(f"{item['reference']}: missing Rust mapping {rust}")
        if item["status"] == "verified" and not (item["rust"] and item["evidence"]):
            problems.append(f"{item['reference']}: verified without code or evidence")
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--write", action="store_true", help="write parity-map.json (default: dry run)")
    args = parser.parse_args()
    with open(PARITY) as handle:
        parity = json.load(handle)
    updated, changes, unknown = propose(parity["items"], gather())
    transitions = collections.Counter()
    for old, new in changes:
        transitions[f"{old['status']} -> {new['status']}"] += 1
        print(f"{old['reference']}: {old['status']} -> {new['status']}")
        for key in ("rust", "evidence"):
            for value in new[key]:
                if value not in old[key]:
                    print(f"    + {key}: {value}")
        if new["status"] == "partial" and new.get("reason") and new.get("reason") != old.get("reason"):
            print(f"    reason: {new['reason'][:400]}")
    counts = collections.Counter(item["status"] for item in updated)
    invariant_problems = invariants(updated)
    for problem in invariant_problems:
        print(f"INVARIANT: {problem}")
    print(f"\n{len(changes)} references change: " + ", ".join(f"{k} x{v}" for k, v in sorted(transitions.items())))
    print(f"resulting parity map: {dict(counts)}")
    if unknown:
        print("mapped references missing from parity-map.json:")
        for reference in unknown:
            print(f"    {reference}")
    if args.write and invariant_problems:
        sys.exit("refusing to write: durable-inventory.py --check would fail")
    if args.write:
        with open(PARITY, "w") as handle:
            handle.write(json.dumps({"reference": parity["reference"], "items": updated}, indent=2) + "\n")
        print("wrote porting/durable/parity-map.json")
    else:
        print("dry run: parity-map.json not written (use --write)")


if __name__ == "__main__":
    sys.exit(main())
