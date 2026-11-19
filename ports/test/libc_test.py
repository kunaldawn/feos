"""Builds a subset of musl's libc-test for feos (ports/test/build-libc-test.sh says how).

    libc_test.py LIBC_TEST SELECTION CC PAYLOAD BUILD [--all]

LIBC_TEST is ports/src/libc-test, SELECTION ports/test/libc-test-feos.txt, CC the feos C
driver, PAYLOAD the directory to make (what goes into the feos image) and BUILD a scratch
directory for every binary, the ones feos doesn't run yet included.
"""
import concurrent.futures
import fnmatch
import os
import shutil
import subprocess
import sys

LT, SELECTION, CC, PAYLOAD, BUILD = sys.argv[1:6]
# --all: the Linux reference (libc-suite-linux.sh), which runs every selected test that
# builds, whatever SELECTION notes about it.
ALL = "--all" in sys.argv[6:]
# The architecture, from the driver's name (x86_64-unknown-feos-cc) or LINUX_ARCH.
ARCH = os.path.basename(CC).split("-", 1)[0] if os.path.basename(CC).endswith("-feos-cc") \
    else os.environ.get("LINUX_ARCH", "x86_64")
SRC = os.path.join(LT, "src")

# config.mak.def's CFLAGS (less -g and -pipe), and the top Makefile's include paths.
CFLAGS = ["-std=c99", "-D_POSIX_C_SOURCE=200809L", "-Wall", "-Wno-unused-function", "-Wno-missing-braces",
          "-Wno-unused", "-Wno-overflow", "-Wno-unknown-pragmas", "-fno-builtin", "-frounding-math",
          "-Werror=implicit-function-declaration", "-Werror=implicit-int", "-Werror=pointer-sign",
          "-Werror=pointer-arith", "-D_FILE_OFFSET_BITS=64", "-Wno-unsupported-floating-point-opt",
          "-O1", "-I", os.path.join(SRC, "common")]
LDFLAGS = ["-Wl,--gc-sections,-s"]
# src/common is libtest.a; runtest.c is libc-test's own runner (fork, alarm, kill), which
# testrun replaces on feos.
COMMON = ["fdfill", "memfill", "mtest", "path", "print", "rand", "setrlim", "utf8", "vmfill"]
# Built with another source, as the tests' .mk files say for the static binaries.
EXTRA = {"functional/tls_align": ["functional/tls_align_dso.c"]}
# Not tests: the shared objects other tests load.
HELPERS = ["functional/dlopen_dso", "functional/tls_align_dso", "functional/tls_init_dso",
           "regression/tls_get_new-dtv_dso"]


def selection():
    """(include patterns, NAME -> (STAGE, WHY)) from libc-test-feos.txt."""
    include, notes = [], {}
    for line in open(SELECTION):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        words = line.split()
        if words[0] == "include":
            include += words[1:]
            continue
        name, stage, *why = words
        # NAME@ARCH: for that architecture only.
        name, _, only = name.partition("@")
        if only and only != ARCH:
            continue
        if name in notes:
            sys.exit(f"libc_test: {name} is listed twice for {ARCH}")
        notes[name] = (stage, " ".join(why) or "-")
    return include, notes


def run(command, **kw):
    result = subprocess.run(command, capture_output=True, text=True, **kw)
    return result.returncode, result.stdout + result.stderr


def main():
    include, notes = selection()
    every = []
    for d in ("functional", "regression", "math"):
        for f in sorted(os.listdir(os.path.join(SRC, d))):
            name = f"{d}/{f[:-2]}"
            if f.endswith(".c") and name not in HELPERS:
                every.append(name)
    chosen = [n for n in every if any(fnmatch.fnmatchcase(n, p) for p in include)]
    for name in notes:
        if name not in chosen:
            sys.exit(f"libc_test: libc-test-feos.txt notes {name}, which the selection doesn't include")

    shutil.rmtree(BUILD, ignore_errors=True)
    os.makedirs(os.path.join(BUILD, "common"))
    objects = []
    for c in COMMON:
        obj = os.path.join(BUILD, "common", c + ".o")
        rc, text = run([CC, "-c", os.path.join(SRC, "common", c + ".c"), "-o", obj] + CFLAGS)
        if rc:
            sys.exit(f"libc_test: common/{c}.c doesn't build:\n{text}")
        objects.append(obj)
    libtest = os.path.join(BUILD, "common", "libtest.a")
    rc, text = run(["llvm-ar", "rcs", libtest] + objects)
    if rc:
        sys.exit(f"libc_test: libtest.a: {text}")

    def build(name):
        binary = os.path.join(BUILD, name)
        os.makedirs(os.path.dirname(binary), exist_ok=True)
        sources = [os.path.join(SRC, name + ".c")] + [os.path.join(SRC, s) for s in EXTRA.get(name, [])]
        rc, text = run([CC] + sources + ["-o", binary] + CFLAGS + LDFLAGS + [libtest])
        return name, rc, text

    with concurrent.futures.ThreadPoolExecutor(max_workers=os.cpu_count() or 4) as pool:
        results = list(pool.map(build, chosen))
    broken = {n: text for n, rc, text in results if rc}
    if ALL:
        notes = {n: ("skip", "doesn't build") for n in broken}
    for name, text in broken.items():
        if name not in notes:
            print(f"libc_test: {name} doesn't build:\n{text}", file=sys.stderr)
    unexpected = [n for n in broken if n not in notes]
    if unexpected:
        sys.exit(f"libc_test: {len(unexpected)} tests don't build: {' '.join(unexpected)}")

    shutil.rmtree(PAYLOAD, ignore_errors=True)
    os.makedirs(PAYLOAD)
    lines = ["# libc-test for feos: ports/test/build-libc-test.sh, run by testrun\n"]
    # libc-test-all.list: every selected test that built, noted or not, for the record
    # (libc-suite.toml runs it without judging; suite-report.py tabulates it).
    every_line = ["# every libc-test test built, for the record: ports/test/build-libc-test.sh\n"]
    counts = {"status": 0, "pending": 0, "skip": 0}
    for name in chosen:
        if name not in broken:
            dst = os.path.join(PAYLOAD, name)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copy(os.path.join(BUILD, name), dst)
            every_line.append(f"status {name} ./{name}\n")
        if name in notes:
            stage, why = notes[name]
            # `upstream` and `skip` aren't run; only a kernel stage is pending.
            kind = "skip" if stage in ("skip", "upstream") else "pending"
            label = "relibc upstream: " if stage == "upstream" else ""
            lines.append(f"skip {name} {label}{why}\n" if kind == "skip" else f"pending {name} {stage} {why}\n")
            counts[kind] += 1
            continue
        lines.append(f"status {name} ./{name}\n")
        counts["status"] += 1
    with open(os.path.join(PAYLOAD, "libc-test-all.list"), "w") as f:
        f.writelines(every_line)
    with open(os.path.join(PAYLOAD, "libc-test.list"), "w") as f:
        f.writelines(lines)
    size = sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(PAYLOAD) for f in fs)
    print(f"libc_test: {CC}: {len(chosen)} of {len(every)} selected, {len(chosen) - len(broken)} built; "
          f"payload {counts['status']} run, {counts['pending']} pending, {counts['skip']} skipped, "
          f"{size // 1024} KiB")


main()
