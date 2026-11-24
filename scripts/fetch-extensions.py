#!/usr/bin/env python3
"""Verify or stage locally compiled custom DuckDB 2.0.0 extensions (never download official extensions)."""
import argparse
import hashlib
import json
import os
import pathlib
import platform
import tempfile

REQUIRED_EXTENSIONS = ('postgres', 'mysql')
OPTIONAL_EXTENSIONS = ('oracle', 'sqlserver', 'odbc_catalog')
ALL_EXTENSIONS = REQUIRED_EXTENSIONS + OPTIONAL_EXTENSIONS
PLATFORMS = {
    ('Linux', 'x86_64'): 'linux_amd64',
    ('Linux', 'aarch64'): 'linux_arm64',
    ('Darwin', 'arm64'): 'osx_arm64',
    ('Darwin', 'x86_64'): 'osx_amd64',
}


def extension_filename(logical: str) -> str:
    return f'{logical}_scanner.duckdb_extension'


def atomic_write_bytes(dest: pathlib.Path, data: bytes, mode: int = 0o644) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=dest.parent, delete=False) as staged:
        staged_path = pathlib.Path(staged.name)
        try:
            staged.write(data)
            staged.flush()
            staged_path.chmod(mode)
            staged_path.replace(dest)
        finally:
            staged_path.unlink(missing_ok=True)


def stage_from_source(source_dir: pathlib.Path, root: pathlib.Path, target: str) -> dict:
    src = source_dir.resolve()
    if not src.is_dir():
        raise SystemExit(f'Custom extension source directory does not exist: {src}')
    folder = root / 'v2.0.0' / target
    folder.mkdir(parents=True, exist_ok=True)
    records = {}
    for logical in ALL_EXTENSIONS:
        filename = extension_filename(logical)
        candidates = [src / 'v2.0.0' / target / filename, src / filename]
        found = next((p for p in candidates if p.exists() or p.is_symlink()), None)
        if found is None:
            if logical in REQUIRED_EXTENSIONS:
                raise SystemExit(
                    f'Missing locally compiled required custom extension {filename} under {src}; '
                    'FedEngine requires custom DuckDB 2.0.0 extensions and never downloads official upstream extensions'
                )
            continue
        if found.is_symlink() or not found.is_file():
            raise SystemExit(f'Custom extension source path must be a regular non-symlink file: {found}')
        data = found.read_bytes()
        dest = folder / filename
        if dest.is_symlink():
            raise SystemExit(f'Symlinked destination extension is refused: {dest}')
        atomic_write_bytes(dest, data)
        records[logical] = {
            'path': str(dest.relative_to(root)),
            'sha256': hashlib.sha256(data).hexdigest(),
        }
    manifest = {'duckdb_version': 'v2.0.0', 'platform': target, 'extensions': records}
    manifest_path = root / 'manifest.json'
    if manifest_path.is_symlink():
        raise SystemExit(f'Symlinked manifest is refused: {manifest_path}')
    atomic_write_bytes(manifest_path, (json.dumps(manifest, indent=2) + '\n').encode('utf-8'))
    return manifest


def verify_existing(root: pathlib.Path, target: str) -> dict:
    manifest_path = root / 'manifest.json'
    if manifest_path.is_symlink() or not manifest_path.is_file():
        raise SystemExit(
            f'Missing local custom extension manifest at {manifest_path}; '
            'FedEngine requires locally compiled custom DuckDB 2.0.0 extensions '
            '(supply --source-dir or FEDENGINE_EXTENSIONS_SOURCE) and never downloads official extensions'
        )
    try:
        manifest = json.loads(manifest_path.read_text(encoding='utf-8'))
    except (OSError, ValueError) as exc:
        raise SystemExit(f'Invalid custom extension manifest {manifest_path}: {exc}') from exc

    if manifest.get('duckdb_version') != 'v2.0.0' or manifest.get('platform') != target:
        raise SystemExit(
            f'Extension manifest mismatch in {manifest_path}: expected duckdb_version=v2.0.0 platform={target}'
        )
    extensions = manifest.get('extensions')
    if not isinstance(extensions, dict):
        raise SystemExit(f'Invalid extensions map in {manifest_path}')

    missing_required = [name for name in REQUIRED_EXTENSIONS if name not in extensions]
    if missing_required:
        raise SystemExit(
            f'Missing required locally compiled custom extensions {missing_required} in {manifest_path}; '
            'never downloading official extensions'
        )

    for logical, entry in extensions.items():
        if logical not in ALL_EXTENSIONS or not isinstance(entry, dict):
            raise SystemExit(f'Unsupported extension entry {logical!r} in {manifest_path}')
        if 'extensions.duckdb.org' in str(entry.get('url', '')):
            raise SystemExit(
                f'Official upstream extension URL rejected for {logical} in {manifest_path}; '
                'FedEngine requires locally compiled custom DuckDB 2.0.0 extensions'
            )
        rel_path = entry.get('path')
        expected_sha = entry.get('sha256')
        if not isinstance(rel_path, str) or not isinstance(expected_sha, str) or not expected_sha:
            raise SystemExit(f'Invalid metadata for extension {logical} in {manifest_path}')
        raw_path = root / rel_path
        if raw_path.is_symlink():
            raise SystemExit(f'Symlinked extension is refused: {raw_path}')
        resolved = raw_path.resolve()
        if not resolved.is_relative_to(root) or resolved.name != extension_filename(logical) or not resolved.is_file():
            raise SystemExit(f'Missing or invalid custom extension binary for {logical}: {raw_path}')
        actual_sha = hashlib.sha256(resolved.read_bytes()).hexdigest()
        if actual_sha != expected_sha:
            raise SystemExit(
                f'Custom extension SHA-256 mismatch for {logical} at {resolved}: '
                f'expected {expected_sha}, got {actual_sha}'
            )
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dest', default='data/extensions')
    parser.add_argument(
        '--source-dir',
        default=os.environ.get('FEDENGINE_EXTENSIONS_SOURCE'),
        help='Directory containing locally compiled *_scanner.duckdb_extension binaries to stage',
    )
    args = parser.parse_args()
    target = PLATFORMS.get((platform.system(), platform.machine()))
    if target is None:
        raise SystemExit('Unsupported extension platform; supply locally compiled custom extensions')
    root = pathlib.Path(args.dest).resolve()
    manifest = (
        stage_from_source(pathlib.Path(args.source_dir), root, target)
        if args.source_dir
        else verify_existing(root, target)
    )
    print(json.dumps(manifest, indent=2))
    print(f'extension_directory = "{root}"')


if __name__ == '__main__':
    main()
