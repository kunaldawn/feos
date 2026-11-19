"""Builds relibc's test suite for feos (ports/test/build-relibc-tests.sh says how).

    relibc_tests.py TESTS ANNOTATIONS CC PAYLOAD BUILD

    relibc_tests.py TESTS ANNOTATIONS CC PAYLOAD BUILD --all

TESTS is relibc's tests/ directory, ANNOTATIONS ports/test/relibc-feos.txt, CC the feos C
driver (or, with --all, the Linux reference's compiler, every annotation ignored), PAYLOAD the directory to make (what goes into the feos image) and BUILD a scratch
directory for every test's binary, the ones feos doesn't run yet included.
"""
import concurrent.futures
import os
import shutil
import subprocess
import sys

TESTS, ANNOTATIONS, CC, PAYLOAD, BUILD = sys.argv[1:6]
# --all: the Linux reference (libc-suite-linux.sh), which runs every test a static build
# can, whatever ANNOTATIONS says.
ALL = "--all" in sys.argv[6:]
# The architecture, from the driver's name (x86_64-unknown-feos-cc) or LINUX_ARCH.
ARCH = os.path.basename(CC).split("-", 1)[0] if os.path.basename(CC).endswith("-feos-cc") \
    else os.environ.get("LINUX_ARCH", "x86_64")

# relibc's tests/Makefile FLAGS, less what the driver does itself (the sysroot, the crt
# objects, -nostdinc/-nostdlib) and -g/-Werror: the payload is stripped, and a warning
# from this clang isn't a test result.
FLAGS = ["-fno-builtin", "-fno-stack-protector", "-Wall", "-Wextra", "-Wno-deprecated-declarations",
         "-pedantic", "-I", TESTS, "-O1", "-Wl,--gc-sections,-s"]
# What tests/Makefile's support_build copies next to the binaries.
SUPPORT = ["example_dir", "sys_select/select.c", "locale/fr.in", "stdlib/realpath.c", "stdio/fread.in",
           "stdio/fscanf_offby1.c", "stdio/fscanf.c", "stdio/stdio.in", "stdio/getline.in",
           "stdio/ungetc_ftell.c", "sys_stat/stat.c", "unistd/link.c", "wchar/fgetwc.in", "wchar/ungetwc.in"]


def make_lists():
    """The name lists of tests/Makefile.tests.mk, as its static build (IS_STATIC=1) has them."""
    script = ("include Makefile.tests.mk\n"
              "print:\n"
              "\t@echo EXPECT $(EXPECT_NAMES)\n"
              "\t@echo STATIC_CHECK $(STATIC_CHECK_EXPECT_NAMES)\n"
              "\t@echo VARIED $(VARIED_NAMES)\n"
              "\t@echo DYNAMIC_ONLY $(DYNAMIC_ONLY_EXPECT_NAMES)\n"
              "\t@echo EXPECT_INPUT $(EXPECT_INPUT_NAMES)\n")
    text = subprocess.run(["make", "-s", "-f", "-", "print", "IS_STATIC=1", "IS_REDOX=0", "BUILD=."],
                          input=script, cwd=TESTS, capture_output=True, text=True, check=True).stdout
    lists = {}
    for line in text.splitlines():
        kind, *names = line.split()
        lists[kind] = names
    return lists


def annotations():
    """NAME -> (STAGE, WHY) from relibc-feos.txt; STAGE is a kernel stage or `skip`."""
    notes = {}
    for line in open(ANNOTATIONS):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        name, stage, *why = line.split()
        # NAME@ARCH: for that architecture only.
        name, _, only = name.partition("@")
        if only and only != ARCH:
            continue
        if name in notes:
            sys.exit(f"relibc_tests: {name} is annotated twice for {ARCH}")
        notes[name] = (stage, " ".join(why) or "-")
    return notes


def expected_file(name, kind):
    """relibc's runner's lookup (tests/src/main.rs, `expected`): the shortest tail of the
    binary's path, bins_static/NAME.KIND, that names a file under expected/."""
    parts = f"bins_static/{name}.{kind}".split("/")
    for i in range(1, len(parts) + 1):
        candidate = os.path.join(TESTS, "expected", *parts[-i:])
        if os.path.isfile(candidate):
            return "expected/" + "/".join(parts[-i:])
    return None


def compile_test(name):
    source = os.path.join(TESTS, name + ".c")
    binary = os.path.join(BUILD, "bins_static", name)
    os.makedirs(os.path.dirname(binary), exist_ok=True)
    result = subprocess.run([CC, source, "-o", binary] + FLAGS, capture_output=True, text=True)
    return name, result.returncode, result.stderr


def main():
    lists = make_lists()
    notes = annotations()
    if ALL:
        notes = {n: ("skip", "not in a static build") for n in lists["DYNAMIC_ONLY"]}
        notes.update({n: ("skip", "driven by expect(1)") for n in lists["EXPECT_INPUT"]})
        notes.update({n: ("skip", "C++") for n in lists["VARIED"] + lists["STATIC_CHECK"] if n.endswith("_cpp")})
    expect = lists["EXPECT"] + lists["STATIC_CHECK"]
    status = lists["VARIED"]
    every = expect + status + lists["DYNAMIC_ONLY"] + lists["EXPECT_INPUT"]
    if len(set(every)) != len(every):
        sys.exit("relibc_tests: a test is in two of Makefile.tests.mk's lists")
    for name in notes:
        if name not in every:
            sys.exit(f"relibc_tests: relibc-feos.txt names {name}, which Makefile.tests.mk doesn't")
    for name in lists["DYNAMIC_ONLY"] + lists["EXPECT_INPUT"]:
        if name not in notes:
            sys.exit(f"relibc_tests: {name} needs an annotation (dynamic-only or expect-driven)")

    # C++ tests need libc++ (track T5/plan phase 4); the rest must build.
    build = [n for n in every if not n.endswith("_cpp")]
    shutil.rmtree(BUILD, ignore_errors=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=os.cpu_count() or 4) as pool:
        results = list(pool.map(compile_test, build))
    broken = [(n, err) for n, rc, err in results if rc != 0]
    for name, err in broken:
        print(f"relibc_tests: {name} doesn't build:\n{err}", file=sys.stderr)
    if broken:
        sys.exit(f"relibc_tests: {len(broken)} tests don't build")

    shutil.rmtree(PAYLOAD, ignore_errors=True)
    os.makedirs(PAYLOAD)
    shutil.copytree(os.path.join(TESTS, "expected"), os.path.join(PAYLOAD, "expected"))
    for item in SUPPORT:
        src, dst = os.path.join(TESTS, item), os.path.join(PAYLOAD, item)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        if os.path.isdir(src):
            shutil.copytree(src, dst)
        else:
            shutil.copy(src, dst)
    lines = ["# relibc's tests for feos: ports/test/build-relibc-tests.sh, run by testrun\n"]
    # relibc-all.list: every test that was built, the pending and skipped ones too, for the
    # record (libc-suite.toml runs it without judging; suite-report.py tabulates it).
    every_line = ["# every relibc test built, for the record: ports/test/build-relibc-tests.sh\n"]
    counts = {"expect": 0, "status": 0, "pending": 0, "skip": 0}
    for name in every:
        if name.endswith("_cpp") and name not in notes:
            sys.exit(f"relibc_tests: {name} is C++ and needs a skip annotation")
        binary = os.path.join("bins_static", name)
        # relibc's runner passes these two arguments to every test (tests/src/main.rs).
        if name in expect:
            stdout = expected_file(name, "stdout")
            if stdout is None:
                sys.exit(f"relibc_tests: no expected stdout for {name}")
            stderr = expected_file(name, "stderr") or "-"
            line = f"expect {name} ./{binary} {stdout} {stderr} test args\n"
        else:
            line = f"status {name} ./{binary} test args\n"
        if name in build:
            dst = os.path.join(PAYLOAD, binary)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copy(os.path.join(BUILD, binary), dst)
            every_line.append(line)
        if name in notes:
            stage, why = notes[name]
            if stage == "skip":
                lines.append(f"skip {name} {why}\n")
                counts["skip"] += 1
            else:
                lines.append(f"pending {name} {stage} {why}\n")
                counts["pending"] += 1
            continue
        lines.append(line)
        counts[line.split()[0]] += 1
    with open(os.path.join(PAYLOAD, "relibc-all.list"), "w") as f:
        f.writelines(every_line)
    with open(os.path.join(PAYLOAD, "relibc.list"), "w") as f:
        f.writelines(lines)
    size = sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(PAYLOAD) for f in fs)
    print(f"relibc_tests: {CC}: {len(build)} built; payload {counts['expect']} expect, "
          f"{counts['status']} status, {counts['pending']} pending, {counts['skip']} skipped, "
          f"{size // 1024} KiB")


main()
