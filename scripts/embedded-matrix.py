#!/usr/bin/env python3
"""The embedded acceptance matrix: which upstream test files run through
crates/pi-durable-jsrt/examples/embedded_stock.rs, and whether a run satisfied the pass rule.

  embedded-matrix.py list             TSV: suite, build (plain|probes), report file name
  embedded-matrix.py excluded         TSV: suite, reason (n/a files, excluded from the pass counts)
  embedded-matrix.py verify <dir>     check every report in <dir> against the mappings

Pass rule (EMBEDDED-PLAN.md section 3): the pinned test bytes are unchanged (report sha256 equals the
mapping's), every mode (default and PI_DURABLE_DOCUMENT_RETENTION_MS=0 for durable and codemode files, one
mode for chord files) has success, no failed or uncaught case, total > 0 and passed + skipped == total, and
native calls > 0 unless the file is a justified no-native file.
"""
import json
import os
import re
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(__file__), ".."))
MAPPINGS = os.path.join(ROOT, "porting", "durable", "mappings")
# Files that never reach the durable runtime (examples/embedded_stock.rs NO_NATIVE).
NO_NATIVE = {
    "packages/durable/test/harness-telemetry-options.test.ts": "type-level test: imports only types from pi-durable",
}


def load(name):
    with open(os.path.join(MAPPINGS, name)) as handle:
        return json.load(handle)


def entries(document):
    """Acceptance entries of an embedded-*.json mapping: `files` or `tests`, as a list or a name->entry dict."""
    for key in ("files", "tests"):
        value = document.get(key)
        if isinstance(value, dict):
            yield from value.values()
        elif isinstance(value, list):
            yield from value


def collect():
    """(suites, excluded): suites is {suite: {sha, probes}}, excluded is {suite: reason}."""
    suites, excluded = {}, {}
    for name in sorted(os.listdir(MAPPINGS)):
        if not (name.startswith("embedded-") and name.endswith(".json")):
            continue
        document = load(name)
        for entry in entries(document):
            suite = entry.get("test") or entry.get("reference")
            if suite and suite.endswith(".test.ts"):
                suites[suite] = {"sha": entry.get("sha256"), "probes": "stock-test-probes" in json.dumps(entry)}
        for entry in (document.get("notApplicable") or {}).values():
            excluded[entry["test"]] = entry["reason"]
    for suite in excluded:
        suites.pop(suite, None)
    # Debugging aid: restrict the matrix to files matching a regex. The gate never sets it.
    only = os.environ.get("DURABLE_EMBEDDED_FILTER")
    if only:
        suites = {suite: info for suite, info in suites.items() if re.search(only, suite)}
    return suites, excluded


def report_name(suite):
    return re.sub(r"[^A-Za-z0-9]+", "-", suite.removeprefix("packages/").removesuffix(".ts")) + ".json"


def verify(directory):
    suites, _ = collect()
    failures, modes_run, cases = [], 0, 0
    for suite, info in sorted(suites.items()):
        path = os.path.join(directory, report_name(suite))
        if not os.path.exists(path):
            failures.append(f"{suite}: no report")
            continue
        with open(path) as handle:
            report = json.load(handle)
        problems = []
        if report.get("success") is not True:
            problems.append("report.success is not true")
        if info["sha"] and report.get("sha256") != info["sha"]:
            problems.append(f"pinned bytes differ: {report.get('sha256')} != {info['sha']}")
        modes = report.get("modes") or []
        expected = 1 if suite.startswith("packages/chord/") else 2
        if len(modes) != expected:
            problems.append(f"{len(modes)} modes ran, expected {expected}")
        for mode in modes:
            label = mode.get("retention") or "default"
            total, passed, skipped = mode.get("total", 0), mode.get("passed", 0), mode.get("skipped", 0)
            calls = mode.get("native", {}).get("calls", 0)
            if total <= 0:
                problems.append(f"{label}: no cases ran")
            if mode.get("failed", 0) or mode.get("uncaught") or mode.get("loadError"):
                problems.append(f"{label}: failed={mode.get('failed')} uncaught={len(mode.get('uncaught') or [])} loadError={mode.get('loadError')}")
            if passed + skipped != total:
                problems.append(f"{label}: passed {passed} + skipped {skipped} != total {total}")
            if calls <= 0 and suite not in NO_NATIVE:
                problems.append(f"{label}: no native calls")
            modes_run += 1
            cases += total
        failures.extend(f"{suite}: {p}" for p in problems)
    print(f"embedded matrix: {len(suites)} files, {modes_run} mode runs, {cases} cases, {len(failures)} problems")
    for suite, reason in sorted(collect()[1].items()):
        print(f"  n/a {suite}: {reason}")
    for suite, reason in sorted(NO_NATIVE.items()):
        print(f"  no-native {suite}: {reason}")
    for line in failures:
        print(f"  FAIL {line}")
    return 1 if failures else 0


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else "list"
    suites, excluded = collect()
    if command == "list":
        for suite, info in sorted(suites.items()):
            print(f"{suite}\t{'probes' if info['probes'] else 'plain'}\t{report_name(suite)}")
    elif command == "excluded":
        for suite, reason in sorted(excluded.items()):
            print(f"{suite}\t{reason}")
    elif command == "verify":
        sys.exit(verify(sys.argv[2]))
    else:
        sys.exit(f"unknown command {command}")


if __name__ == "__main__":
    main()
