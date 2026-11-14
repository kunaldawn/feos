#!/usr/bin/env python3
"""Stub out vendored crates that no build of ours can reach.

`cargo vendor` copies every package in a lockfile, including Windows-only and
wasm-only ones. Cargo still needs each package's manifest to resolve the
lockfile, but it never compiles a dependency gated on another platform, so
such a package can be cut to its `Cargo.toml` and an empty `src/lib.rs`. Its
`.cargo-checksum.json` keeps the package checksum (checked against
Cargo.lock) and lists no files. Debian and Fedora prune the same way.

Usage: prune-vendor.py VENDOR_DIR KEEP_LIST

KEEP_LIST has one `name<TAB>version` per line: the packages `cargo metadata
--all-features --filter-platform <triple>` reaches for every workspace we
build, on every host triple (see ports/README.md). Anything else is stubbed.
Run it again after re-vendoring; it is idempotent.
"""
import json
import pathlib
import shutil
import sys
import tomllib


def main() -> int:
    vendor = pathlib.Path(sys.argv[1])
    keep = {tuple(line.split("\t")) for line in pathlib.Path(sys.argv[2]).read_text().splitlines() if line}
    stubbed = 0
    freed = 0
    for crate in sorted(p for p in vendor.iterdir() if p.is_dir()):
        manifest = crate / "Cargo.toml"
        package = tomllib.loads(manifest.read_text())["package"]
        if (package["name"], package["version"]) in keep:
            continue
        checksum = json.loads((crate / ".cargo-checksum.json").read_text())
        if checksum.get("files") == {} and (crate / ".feos-stub").exists():
            continue
        size = sum(f.stat().st_size for f in crate.rglob("*") if f.is_file())
        text = manifest.read_text()
        for child in crate.iterdir():
            if child.is_dir():
                shutil.rmtree(child)
            else:
                child.unlink()
        manifest.write_text(text)
        (crate / "src").mkdir()
        (crate / "src" / "lib.rs").write_text("")
        (crate / ".feos-stub").write_text("Stubbed by ports/prune-vendor.py: no build of ours reaches this crate.\n")
        (crate / ".cargo-checksum.json").write_text(json.dumps({"files": {}, "package": checksum["package"]}))
        stubbed += 1
        freed += size
    print(f"stubbed {stubbed} crates, freed {freed / 2**20:.0f} MiB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
