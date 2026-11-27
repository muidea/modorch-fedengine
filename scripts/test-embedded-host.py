#!/usr/bin/env python3
"""Build and exercise the public Facade from a disposable, external Cargo workspace."""
import argparse
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
REVISION = "336ba20196044e1b81fb2ca297a18b5e258a5800"


def run(command, directory, environment):
    print("+ " + " ".join(map(str, command)), flush=True)
    subprocess.run(command, cwd=directory, env=environment, check=True)


def smoke(binary, directory, environment):
    text = (directory / "application.toml").read_text()
    runtime = directory.parent / "host-working-directory"
    runtime.mkdir()
    # Configuration lives elsewhere; relative paths follow the explicit host CWD.
    assert not (directory / "data").exists()
    config = directory / "smoke.toml"
    config.write_text(text)
    environment = dict(environment, FEDENGINE_EMBEDDED_PASSWORD="disposable-external-host-password")
    for exit_signal in (signal.SIGINT, signal.SIGTERM):
        process = subprocess.Popen(
            [str(binary), str(config)], cwd=runtime, env=environment,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        output = b""
        try:
            deadline = time.monotonic() + 30
            while b"embedded-in-modorch Ready" not in output:
                if process.poll() is not None:
                    output += process.stdout.read()
                    raise AssertionError(output.decode(errors="replace"))
                if time.monotonic() >= deadline:
                    raise AssertionError("external binary Ready timeout")
                if select.select([process.stdout], [], [], 0.2)[0]:
                    output += os.read(process.stdout.fileno(), 65536)
            pg = re.search(rb"pgwire=Some\(127\.0\.0\.1:(\d+)\)", output)
            http = re.search(rb"management=Some\(127\.0\.0\.1:(\d+)\)", output)
            assert pg and http, output.decode(errors="replace")
            addresses = [("127.0.0.1", int(match[1])) for match in (pg, http)]
            with socket.create_connection(addresses[0], timeout=5) as client:
                client.sendall(bytes([0, 0, 0, 8, 4, 210, 22, 47]))
                assert client.recv(1) == b"N"
            with urllib.request.urlopen(
                f"http://127.0.0.1:{addresses[1][1]}/api/v1/sources", timeout=5
            ) as response:
                assert response.status == 200
                assert json.load(response) == []
            process.send_signal(exit_signal)
            tail, _ = process.communicate(timeout=15)
            output += tail
            assert process.returncode == 0, output.decode(errors="replace")
            assert b"shutdown=Ok(GracefullyStopped)" in output, output.decode(errors="replace")
            for address in addresses:
                with socket.socket() as listener:
                    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                    listener.bind(address)
            print(f"External binary PG/HTTP + {exit_signal.name} + drain: Passed", flush=True)
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.communicate(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.communicate()

    # A failed child owns only the disposable state created above.
    with socket.socket() as occupied:
        occupied.bind(("127.0.0.1", 0))
        occupied.listen()
        failed = directory / "occupied.toml"
        failed.write_text(text.replace(
            'listen = "127.0.0.1:0"',
            f'listen = "127.0.0.1:{occupied.getsockname()[1]}"', 1,
        ))
        result = subprocess.run(
            [str(binary), str(failed)], cwd=runtime, env=environment,
            capture_output=True, text=True, timeout=30,
        )
        assert result.returncode != 0, result.stdout + result.stderr
        assert "startup cause=" in result.stderr, result.stderr
        assert "cleanup=Ok(GracefullyStopped)" in result.stderr, result.stderr
    assert config.read_text() == text
    assert (runtime / "data/fedengine.duckdb").is_file()
    assert (runtime / "data/tmp").is_dir()
    assert not (directory / "data").exists()
    print("External binary startup failure + compensation + host CWD paths: Passed", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--online", action="store_true", help="Allow Cargo to fetch locked dependencies")
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target",
                        help="Reuse build/native-library caches; the consumer workspace stays separate")
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("the binary signal smoke currently requires a Unix host")
    environment = dict(os.environ, CARGO_TARGET_DIR=str(args.target_dir.resolve()))
    rustc = subprocess.check_output(["rustc", "-vV"], text=True, env=environment)
    triple = next(line.removeprefix("host: ") for line in rustc.splitlines() if line.startswith("host: "))
    # Dynamic binaries run outside cargo's injected library search environment.
    library = args.target_dir / "duckdb-download" / triple / "v2.0.0/libduckdb.so"
    if not library.is_file():
        candidates = sorted(
            args.target_dir.glob("*/build/libduckdb-sys-*/out/libduckdb.so"),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )
        if candidates:
            library = candidates[0]
    if library.is_file():
        environment["LD_LIBRARY_PATH"] = str(library.parent.resolve()) + os.pathsep + environment.get("LD_LIBRARY_PATH", "")
    flags = ["--locked"] + ([] if args.online else ["--offline"])
    with tempfile.TemporaryDirectory(prefix="fedengine-host-example-") as folder:
        directory = Path(folder) / "host"
        shutil.copytree(
            ROOT / "examples/embedded-in-modorch", directory,
            ignore=shutil.ignore_patterns("target", "data", "secrets"),
        )
        manifest = directory / "Cargo.toml"
        text = manifest.read_text()
        old = 'path = "../../crates/modorch-fedengine"'
        assert text.count(old) == 1
        manifest.write_text(text.replace(old, "path = " + json.dumps(str(ROOT / "crates/modorch-fedengine"))))
        # copytree preserves timestamps. Rebuild this consumer in its new location
        # even when a shared target directory contains an older example build.
        for source in directory.rglob("*.rs"):
            source.touch()
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", *flags, "--format-version", "1", "--filter-platform", triple],
            cwd=directory, env=environment,
        ))
        assert Path(metadata["workspace_root"]).resolve() == directory.resolve()
        consumer = next(package for package in metadata["packages"] if package["name"] == "embedded-in-modorch")
        assert metadata["workspace_members"] == [consumer["id"]]
        assert {dependency["name"] for dependency in consumer["dependencies"]} == {
            "modorch", "modorch-fedengine", "tokio", "serde_json", "tokio-postgres",
        }
        assert not {"fedengine", "fedengine-app", "modorch-fedengine-workspace", "modorch-fedengine-test-host"} & {package["name"] for package in metadata["packages"]}
        framework = [package for package in metadata["packages"] if package["name"] == "modorch"]
        assert len(framework) == 1
        assert framework[0]["id"] == f"git+https://github.com/muidea/modorch-rs?rev={REVISION}#modorch@0.0.3"
        print("Independent workspace / Facade-only consumer / unique Modorch 0.0.3: Passed", flush=True)
        run(["python3", str(ROOT / ".agents/skills/modorch-app-development/scripts/preflight.py"),
             "--manifest-path", str(manifest)], directory, environment)
        run(["python3", str(ROOT / ".agents/skills/modorch-fedengine-integration/scripts/preflight.py"),
             "--manifest-path", str(manifest)], directory, environment)
        run(["cargo", "fmt", "--all", "--", "--check"], directory, environment)
        run(["cargo", "clippy", "--all-targets", *flags, "--", "-D", "warnings"], directory, environment)
        run(["cargo", "test", "--all-targets", *flags], directory, environment)
        run(["cargo", "doc", "--no-deps", *flags], directory,
            dict(environment, RUSTDOCFLAGS="-D warnings"))
        run(["cargo", "build", "--bin", "embedded-in-modorch", *flags], directory, environment)
        binary = Path(metadata["target_directory"]) / "debug/embedded-in-modorch"
        smoke(binary, directory, environment)
    print("External host acceptance: Passed; disposable workspace/state removed", flush=True)


if __name__ == "__main__":
    main()
