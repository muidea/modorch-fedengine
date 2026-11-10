#!/usr/bin/env python3
"""Validate library contracts; optionally execute local or external mapped suites."""
import argparse
from collections import deque
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
RULE = re.compile(r"^## (FE-[A-Z]+-\d{3})$", re.MULTILINE)
FRAMEWORK_REVISION = "336ba20196044e1b81fb2ca297a18b5e258a5800"


def local_file(root, value):
    path = (root / value).resolve()
    if not path.is_relative_to(root.resolve()) or not path.is_file():
        raise ValueError(f"missing or out-of-tree file: {value}")
    return path


def validate_mapping(root, mapping):
    rules = []
    for path in sorted((root / "contracts").glob("*.md")):
        rules.extend(RULE.findall(path.read_text()))
    if not rules or len(rules) != len(set(rules)):
        raise ValueError("contract rule IDs must be present and unique")
    if mapping["revision"] != "1.1.2":
        raise ValueError("unsupported library contract revision")
    suites = mapping["suites"]
    seen, covered, used = set(), set(), set()
    for suite in suites.values():
        if suite["scope"] not in {"local", "external", "remote"} or not suite["kind"]:
            raise ValueError("suite needs an explicit scope and fixture kind")
        if not suite["command"] or not all(isinstance(v, str) for v in suite["command"]):
            raise ValueError("suite needs an argument-list command")
    for entry in mapping["tests"]:
        suite = entry["suite"]
        if suite not in suites:
            raise ValueError(f"unknown suite: {suite}")
        key = (suite, entry["output"])
        if key in seen:
            raise ValueError(f"duplicate mapped test: {key}")
        seen.add(key)
        used.add(suite)
        source = local_file(root, entry["source"]).read_text()
        # Require a real test attribute immediately above the named function,
        # allowing ignore/cfg/other attributes between it and the function.
        pattern = r"#\[(?:tokio::)?test[^\n]*\]\s*(?:#\[[^\n]*\]\s*)*(?:async\s+)?fn\s+" + re.escape(entry["name"]) + r"\s*\("
        if not re.search(pattern, source):
            raise ValueError(f"missing Rust test: {entry['source']}::{entry['name']}")
        if not entry["rules"] or not set(entry["rules"]) <= set(rules):
            raise ValueError(f"unknown or empty rule mapping: {entry['name']}")
        covered.update(entry["rules"])
    if covered != set(rules) or used != set(suites):
        raise ValueError("unmapped contract rules or unused suites")
    return {"rules": len(rules), "mapped_tests": len(seen), "suite_count": len(suites)}


def validate_links(root):
    paths = [root / "README.md", *sorted((root / "contracts").glob("*.md"))]
    for name in ["architecture/embedded-library-design.md", "migration/fedengine-to-modorch-fedengine.md",
                 "integration/modorch-host.md", "integration/magicdatahub.md", "releases/0.1.0.md",
                 "embedded-library.md", "configuration-ownership.md", "resource-lifecycle.md", "in-process-clients.md"]:
        paths.append(local_file(root, "docs/" + name))
    for path in paths:
        for value in re.findall(r"\]\(([^)]+)\)", path.read_text()):
            url = urlsplit(value.strip("<>"))
            if url.scheme or url.netloc or not url.path:
                continue
            target = (path.parent / unquote(url.path)).resolve()
            if not target.is_relative_to(root.resolve()) or not target.exists():
                raise ValueError(f"broken local link in {path.relative_to(root)}: {value}")
    return len(paths)


def validate_library_layout(root):
    """Keep deployable services in magicDataHub and adapters in test support."""
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    if (manifest["package"]["name"] != "modorch-fedengine-workspace"
            or manifest["package"].get("autobins") is not False
            or manifest.get("dependencies") or manifest.get("bin")
            or (root / "src/main.rs").exists() or (root / "src/bin").exists()):
        raise ValueError("root must remain a development workspace without service binaries")
    if (root / "docker/Dockerfile").exists() or (root / "Dockerfile").exists():
        raise ValueError("service Dockerfile belongs to magicDataHub")
    support = "modorch-fedengine-test-host"
    if support not in manifest["dev-dependencies"]:
        raise ValueError("test host must be a development dependency")
    paths = sorted((root / "crates").glob("*/Cargo.toml"))
    paths.append(root / "tests/support/host/Cargo.toml")
    for path in paths:
        package = tomllib.loads(path.read_text())
        if not package["package"]["name"].startswith("modorch-fedengine"):
            raise ValueError("internal packages must use modorch-fedengine names")
        if (package.get("bin") or (path.parent / "src/main.rs").exists()
                or (path.parent / "src/bin").exists()):
            raise ValueError("library workspace must not contain service binaries")
        if path.parent.parent.name == "crates" and support in package.get("dependencies", {}):
            raise ValueError("test host must not enter library dependencies")


def validate_metadata(root):
    validate_library_layout(root)
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    workspace = manifest["workspace"]["package"]
    if workspace["repository"] != "https://github.com/muidea/modorch-fedengine":
        raise ValueError("workspace repository metadata does not match renamed origin")
    if workspace["publish"] is not False:
        raise ValueError("publication policy must remain explicit and disabled")
    framework = manifest["workspace"]["dependencies"]["modorch"]
    if (framework["version"], framework["rev"], framework["git"]) != (
        "=0.0.3", FRAMEWORK_REVISION, "https://github.com/muidea/modorch-rs"
    ):
        raise ValueError("framework dependency is not the agreed version/source")
    facade = tomllib.loads((root / "crates/modorch-fedengine/Cargo.toml").read_text())
    package = facade["package"]
    if package["name"] != "modorch-fedengine" or not package["repository"]["workspace"]:
        raise ValueError("Facade name/repository metadata mismatch")
    local_file(root, "crates/modorch-fedengine/" + package["readme"])
    release = package["metadata"]["modorch-fedengine"]
    if (release["contract-revision"], release["framework-version"], release["framework-revision"],
        release["status"], release["embedded-core"], release["public-clients"]) != (
        "1.1.2", "0.0.3", FRAMEWORK_REVISION, "development", False, True
    ):
        raise ValueError("release metadata overstates or mismatches current capabilities")
    return {"package_version": workspace["version"], "framework_version": "0.0.3",
            "contract_revision": release["contract-revision"], "publication": "disabled"}


def passed_tests(output):
    return set(re.findall(r"^test ([^\r\n]+?) \.\.\. ok\s*$", output, re.MULTILINE))


def require_evidence(entries, output):
    missing = {entry["output"] for entry in entries} - passed_tests(output)
    if missing:
        raise ValueError("missing successful test evidence: " + ", ".join(sorted(missing)))


def run_suite(root, suite_id, suite, entries):
    print("+ " + " ".join(suite["command"]), flush=True)
    process = subprocess.Popen(suite["command"], cwd=root, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, text=True)
    output, tail = [], deque(maxlen=40)
    try:
        for line in process.stdout:
            output.append(line)
            tail.append(line)
            if line.startswith("test result:") or "External " in line:
                print(line, end="", flush=True)
        if process.wait():
            raise ValueError(f"suite failed: {suite_id}\n{''.join(tail)}")
        require_evidence(entries, "".join(output))
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    return {"suite": suite_id, "scope": suite["scope"], "kind": suite["kind"],
            "status": "Passed", "mapped_tests_passed": len(entries)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--run", choices=["local", "external"])
    args = parser.parse_args()
    root = args.root.resolve()
    report = {"static_status": "Failed", "execution_status": "NotRun", "suites": []}
    try:
        mapping = json.loads((root / "tests/contracts/mapping.json").read_text())
        report.update(validate_mapping(root, mapping))
        report["metadata"] = validate_metadata(root)
        report["link_documents"] = validate_links(root)
        report["static_status"] = "Passed"
        if args.run:
            report["execution_scope"] = args.run
            report["execution_status"] = "Failed"
            for name, suite in mapping["suites"].items():
                if suite["scope"] == args.run:
                    entries = [e for e in mapping["tests"] if e["suite"] == name]
                    report["suites"].append(run_suite(root, name, suite, entries))
            report["execution_status"] = "Passed"
        report["unexecuted_scopes"] = sorted({s["scope"] for s in mapping["suites"].values()} - {args.run})
    except (OSError, ValueError, KeyError, TypeError) as error:
        report["reason"] = str(error)
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report["static_status"] == "Passed" and report["execution_status"] != "Failed" else 1


if __name__ == "__main__":
    sys.exit(main())
