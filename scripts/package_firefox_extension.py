#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import pathlib
import sys
import zipfile


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Package the AriatUI Firefox extension as a local-install XPI."
    )
    parser.add_argument(
        "--source",
        default="extensions/firefox",
        help="Extension source directory containing manifest.json",
    )
    parser.add_argument(
        "--output",
        default=None,
        help="Output XPI path. Defaults to dist/download-via-ariatui-<version>.xpi",
    )
    return parser.parse_args()


def extension_version(source_dir: pathlib.Path) -> str:
    manifest_path = source_dir / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    version = manifest.get("version")
    if not isinstance(version, str) or not version.strip():
        raise SystemExit(f"manifest missing version: {manifest_path}")
    return version


def should_include(path: pathlib.Path) -> bool:
    excluded_names = {
        ".DS_Store",
    }
    excluded_suffixes = {
        ".pyc",
        ".swp",
        ".tmp",
    }
    if path.name in excluded_names:
        return False
    if path.suffix in excluded_suffixes:
        return False
    if any(part in {".git", "__pycache__", "dist"} for part in path.parts):
        return False
    return True


def build_xpi(source_dir: pathlib.Path, output_path: pathlib.Path) -> None:
    if not (source_dir / "manifest.json").is_file():
        raise SystemExit(f"missing manifest.json in {source_dir}")

    output_path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(output_path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for path in sorted(source_dir.rglob("*")):
            if path.is_dir() or not should_include(path.relative_to(source_dir)):
                continue
            archive.write(path, path.relative_to(source_dir))


def main() -> int:
    args = parse_args()
    repo_root = pathlib.Path(__file__).resolve().parent.parent
    source_dir = (repo_root / args.source).resolve()
    version = extension_version(source_dir)
    output_path = (
        pathlib.Path(args.output).resolve()
        if args.output
        else (repo_root / "dist" / f"download-via-ariatui-{version}.xpi").resolve()
    )
    build_xpi(source_dir, output_path)
    print(output_path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
