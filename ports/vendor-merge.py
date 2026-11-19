#!/usr/bin/env python3
"""Merge a workspace vendored on its own into ports/vendor and ports/vendor-git.

`cargo vendor` refuses one directory holding two packages of the same name and
version from different sources. relibc takes `object 0.36.7` from a git fork,
and rust's workspace takes `object 0.36.7` from crates.io, so relibc can't be one
more `--sync` of the main run. vendor.sh vendors it on its own, with
`--versioned-dirs`, into a scratch directory, and this script sorts the result:

- a git package (its `.cargo-checksum.json` has `"package": null`) goes to
  GIT_DIR as `name-version`, which ports/.cargo/config.toml's git source
  replacements point at;
- a crates.io package goes to VENDOR_DIR as `name-version`, unless VENDOR_DIR
  already has that name and version (from the main run).

A directory source finds packages by their manifests, never by directory name,
so the names only have to be unique.

Usage: vendor-merge.py SCRATCH_DIR VENDOR_DIR GIT_DIR
"""
import json
import pathlib
import shutil
import sys
import tomllib


def package(crate):
    manifest = tomllib.loads((crate / "Cargo.toml").read_text())["package"]
    return manifest["name"], manifest["version"]


def main() -> int:
    scratch, vendor, git = (pathlib.Path(p) for p in sys.argv[1:4])
    have = {package(c) for c in vendor.iterdir() if c.is_dir()}
    if git.exists():
        shutil.rmtree(git)
    git.mkdir()
    added = gits = 0
    for crate in sorted(p for p in scratch.iterdir() if p.is_dir()):
        name, version = package(crate)
        checksum = json.loads((crate / ".cargo-checksum.json").read_text())
        if checksum.get("package") is None:
            shutil.copytree(crate, git / f"{name}-{version}", symlinks=True)
            gits += 1
        elif (name, version) not in have:
            shutil.copytree(crate, vendor / f"{name}-{version}", symlinks=True)
            have.add((name, version))
            added += 1
    print(f"vendor-merge: {added} crates.io packages added, {gits} git packages")
    return 0


if __name__ == "__main__":
    sys.exit(main())
