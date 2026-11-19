#!/usr/bin/env python3
"""Tabulates the libc suite's results: each test on feos (both ports, from the record
cases' every-test lists) beside the Linux reference (libc-suite-linux.sh), and what the
feos lists do with it (run and judged, pending on a kernel stage, or skipped and why).

    ports/test/suite-report.py [BUILD]        # BUILD default /build; markdown on stdout

Reads BUILD/libc-suite/boot-{arm,x64}.log (libc-suite.sh), the reference's
BUILD/libc-suite/linux-{x86_64,aarch64}/{relibc-tests,libc-test}.result and
ports/test/{relibc-feos,libc-test-feos}.txt. docs/toolchain.md's tables are its output.
"""
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
BUILD = sys.argv[1] if len(sys.argv) > 1 else "/build"
VERDICT = re.compile(r"^(PASS|FAIL|TIMEOUT|PEND|SKIP) (\S+?):?(?: (.*))?$")
SUMMARY = re.compile(r"^testrun: (\S+\.list): (\d+) passed, (\d+) failed, (\d+) pending, (\d+) skipped")


def parse(path):
    """{list file name: {test: (verdict, note)}} from testrun output in a log."""
    results, current, tail = {}, {}, False
    if not os.path.exists(path):
        return results
    for raw in open(path, errors="replace"):
        line = raw.rstrip("\r\n")
        # boot.py's copy of the serial log's tail after a failing case repeats lines.
        if line.startswith("--- serial log tail ---"):
            tail = True
        elif tail and line == "---":
            tail = False
        if tail:
            continue
        if line.startswith("    | "):
            line = line[6:]
        line = line.rstrip("\r")
        m = SUMMARY.match(line)
        if m:
            results.setdefault(os.path.basename(m.group(1)), current)
            current = {}
            continue
        m = VERDICT.match(line)
        if m:
            verdict, name, note = m.group(1), m.group(2), (m.group(3) or "")
            if verdict == "TIMEOUT":
                note = "hangs"
            current[name] = (verdict, note)
    return results


def annotations(path):
    """NAME -> (STAGE, WHY), an architecture's own entries (NAME@ARCH) said as such."""
    notes = {}
    for line in open(path):
        words = line.split("#", 1)[0].split()
        if not words or words[0] == "include":
            continue
        name, _, only = words[0].partition("@")
        note = (words[1], " ".join(words[2:]))
        if only:
            stage, why = note
            port = {"aarch64": "arm", "x86_64": "x64"}[only]
            note = (stage, f"{port} only: {why}")
            if name in notes:
                note = (notes[name][0], f"{notes[name][1]}; {note[1]}")
        notes[name] = note
    return notes


def cell(result):
    if result is None:
        return "-"
    verdict, note = result
    if verdict == "PASS":
        return "pass"
    if verdict == "TIMEOUT":
        return "hangs"
    m = re.match(r"(exit status \d+|killed by signal \d+|didn't start)", note)
    return "fail" + (f" ({m.group(1)})" if m else (f" ({note})" if note else ""))


def table(title, all_list, judged_list, notes_file, reference_name):
    feos = {port: parse(os.path.join(BUILD, "libc-suite", f"boot-{port}.log")) for port in ("arm", "x64")}
    linux = {arch: parse(os.path.join(BUILD, "libc-suite", f"linux-{arch}", f"{reference_name}.result"))
             for arch in ("x86_64", "aarch64")}
    notes = annotations(notes_file)
    rows = {}
    for port in feos:
        for name in feos[port].get(all_list, {}):
            rows.setdefault(name, {})[port] = feos[port][all_list][name]
    for arch in linux:
        for lst in linux[arch].values():
            for name, result in lst.items():
                if result[0] in ("PASS", "FAIL", "TIMEOUT"):
                    rows.setdefault(name, {})[arch] = result
    counts = {}
    print(f"\n#### {title}\n")
    print("| Test | feos arm | feos x64 | Linux x86_64 | Linux AArch64 (qemu-user) | On feos |")
    print("|---|---|---|---|---|---|")
    for name in sorted(rows):
        r = rows[name]
        judged = {p: feos[p].get(judged_list, {}).get(name) for p in feos}
        if name in notes:
            stage, why = notes[name]
            kind = {"skip": "skip", "upstream": "relibc upstream"}.get(stage, f"pending {stage}")
            status = f"{kind}: {why}" if why else kind
        elif all(j and j[0] == "PASS" for j in judged.values()):
            status = "run, passes"
        else:
            status = "run: " + ", ".join(f"{p} {cell(j)}" for p, j in judged.items())
        key = status.split(":")[0]
        counts[key] = counts.get(key, 0) + 1
        status = status.replace("|", "\\|")
        print(f"| `{name}` | {cell(r.get('arm'))} | {cell(r.get('x64'))} | {cell(r.get('x86_64'))} | "
              f"{cell(r.get('aarch64'))} | {status} |")
    print("\n" + ", ".join(f"{v} {k}" for k, v in sorted(counts.items())))
    for port in feos:
        judged = feos[port].get(judged_list, {})
        verdicts = [v for v, _ in judged.values()]
        print(f"\n{port}: {judged_list}: {verdicts.count('PASS')} passed, "
              f"{verdicts.count('FAIL') + verdicts.count('TIMEOUT')} failed, {verdicts.count('PEND')} pending, "
              f"{verdicts.count('SKIP')} skipped")


table("relibc's tests", "relibc-all.list", "relibc.list", os.path.join(HERE, "relibc-feos.txt"), "relibc-tests")
table("libc-test", "libc-test-all.list", "libc-test.list", os.path.join(HERE, "libc-test-feos.txt"), "libc-test")
