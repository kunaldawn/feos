#!/usr/bin/env python3
"""Pack an application directory into a single-file feos `.fapp` bundle.

A bundle is one ZIP file (deflate) containing at least:

    manifest.yaml   id, name, entry, category, description, icon
    icon.bmp        application icon (BMP, PNG, JPEG, PPM or QOI)

Everything else in the directory is copied as a resource. The desktop detects
bundles by their ZIP magic, not by the file suffix, and scans the application
folder at session start.

Usage:
    scripts/pack-app.py path/to/app-dir path/to/output.fapp
    scripts/pack-app.py path/to/app-dir        # writes <manifest id>.fapp
"""

from __future__ import annotations

import pathlib
import re
import sys
import zipfile

FIXED_TIME = (2026, 1, 1, 0, 0, 0)
BUNDLE_ID = re.compile(r"[A-Za-z0-9_-][A-Za-z0-9._-]*")


def manifest_id(manifest: pathlib.Path) -> str:
    for line in manifest.read_text(encoding="utf-8").splitlines():
        if line.startswith("id:"):
            value = line.split(":", 1)[1].split(" #", 1)[0].strip().strip("\"'")
            if not BUNDLE_ID.fullmatch(value):
                raise SystemExit(f"{manifest}: id {value!r} is not a valid bundle id")
            return value
    raise SystemExit(f"{manifest}: no id field")


def pack(source: pathlib.Path, output: pathlib.Path) -> None:
    manifest = source / "manifest.yaml"
    if not manifest.is_file():
        raise SystemExit(f"{source}: manifest.yaml is required")

    if not output.suffix:
        output = output.with_suffix(".fapp")
    output.parent.mkdir(parents=True, exist_ok=True)

    target = output.resolve()
    files = sorted(
        path
        for path in source.rglob("*")
        if path.is_file()
        # Hidden directories are skipped too, not only hidden files.
        and not any(part.startswith(".") for part in path.relative_to(source).parts)
        # A previous run may have left the output inside the source tree.
        and path.resolve() != target
    )
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for path in files:
            name = path.relative_to(source).as_posix()
            info = zipfile.ZipInfo(name, date_time=FIXED_TIME)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            # The archive-level level is not applied to an explicit ZipInfo.
            archive.writestr(info, path.read_bytes(), compresslevel=9)

    size = output.stat().st_size
    print(f"packed {len(files)} file(s) into {output} ({size} bytes)")


def main(argv: list[str]) -> int:
    if len(argv) not in (2, 3):
        print(__doc__.strip(), file=sys.stderr)
        return 2
    source = pathlib.Path(argv[1])
    output = pathlib.Path(argv[2]) if len(argv) == 3 else pathlib.Path(
        f"{manifest_id(source / 'manifest.yaml')}.fapp"
    )
    pack(source, output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
