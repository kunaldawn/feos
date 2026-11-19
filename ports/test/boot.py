#!/usr/bin/env python3
"""Boot a feos kernel headless and run shell commands from test specs against it.

    ports/test/boot.py arm|x64 KERNEL SPEC.toml [SPEC.toml ...]

Reuses the base's scripts/boot-test.py (imported, never modified): its `Guest`,
`qemu_command` and `boot_to_shell`. FEOS_BOOT_TEST_SMP, _CPU and _MEM work as
they do there. A spec is TOML:

    [[case]]
    name = "hello-copy"              # unique within the spec
    run = "/bin/ports-hello one"     # a line for the feos shell
    expect = ["argv[1]=one"]         # substrings of its output, in this order
    reject = ["FAILED"]              # substrings its output must not have
    status = 0                       # its exit status ($?); omit to not check
    timeout = 60                     # seconds, default 60
    arch = "x64"                     # only on this port; default both
    pending = "1c.7"                 # not run: waits on this kernel stage
    why = "needs EXEC"               # said with `pending`

A pending case is reported as PENDING, never as a pass. Each port's cases run in
one boot, in order; after a failure the rest of that boot is skipped, since the
guest may be in any state (PORTS_KEEP_GOING=1 runs them anyway, for debugging, as
long as the guest is still up). PORTS_SHOW_OUTPUT=1 prints each case's output,
PORTS_SHOW_FAILED=1 each failing one's. Prints one line per case and a summary; exits 1 if
any case failed.
"""
import importlib.util
import os
import pathlib
import re
import sys
import tomllib

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parent.parent


def load_boot_test():
    """The boot-test module of the base the kernel was built from, else the tree's."""
    base = pathlib.Path(os.environ.get("PORTS_BASE", REPO / "target/ports-base"))
    for candidate in (base / "scripts/boot-test.py", REPO / "scripts/boot-test.py"):
        if candidate.is_file():
            spec = importlib.util.spec_from_file_location("feos_boot_test", candidate)
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            return module
    sys.exit("boot.py: no scripts/boot-test.py found")


def load_cases(paths, port):
    cases = []
    for path in paths:
        data = tomllib.loads(pathlib.Path(path).read_text())
        for case in data.get("case", []):
            if case.get("arch", port) != port:
                continue
            case.setdefault("spec", pathlib.Path(path).stem)
            cases.append(case)
    return cases


class CaseFailed(Exception):
    pass


def run_case(bt, guest, case, number):
    """Runs one line; the output between it and its status marker is the case's."""
    timeout = case.get("timeout", 60)
    # `$((N))` keeps the echoed command line itself from matching the marker.
    guest.mark = len(guest.log)
    guest.send(f"{case['run']}; echo ports-case-{number}-$((0))-rc-$?\r")
    pattern = re.escape(f"ports-case-{number}-0-rc-") + r"(\d+)"
    start = len(guest.log)
    try:
        status = guest.capture(pattern, timeout)
    except SystemExit:
        # boot-test's `fail` has printed the log tail and killed the guest.
        raise CaseFailed(f"no status marker within {timeout} s") from None
    text = guest.log[start:guest.mark].decode(errors="replace")
    # Drop the echoed command line.
    first = text.find("\n")
    output = text[first + 1:] if first >= 0 else ""
    show = os.environ.get("PORTS_SHOW_OUTPUT") == "1"
    try:
        position = 0
        for wanted in case.get("expect", []):
            found = output.find(wanted, position)
            if found < 0:
                raise CaseFailed(f"output lacks {wanted!r} (in order); output: {output[-600:]!r}")
            position = found + len(wanted)
        for unwanted in case.get("reject", []):
            if unwanted in output:
                raise CaseFailed(f"output has {unwanted!r}; output: {output[-600:]!r}")
        if "status" in case and int(status) != case["status"]:
            raise CaseFailed(f"status {status}, wanted {case['status']}; output: {output[-600:]!r}")
    except CaseFailed:
        # A failing case's whole output, which a suite's runner fills with the details.
        show = show or os.environ.get("PORTS_SHOW_FAILED") == "1"
        raise
    finally:
        if show:
            for line in output.replace("\r", "").splitlines():
                if not line.startswith("ports-case-"):
                    print(f"    | {line}", flush=True)


def main():
    if len(sys.argv) < 4 or sys.argv[1] not in ("arm", "x64"):
        sys.exit(__doc__)
    port, kernel, specs = sys.argv[1], sys.argv[2], sys.argv[3:]
    bt = load_boot_test()
    cases = load_cases(specs, port)
    results = []
    runnable = [case for case in cases if not case.get("pending")]
    guest = None
    if runnable:
        guest = bt.Guest(bt.qemu_command(port, kernel))
        guest.boot_to_shell()
    failed = False
    for number, case in enumerate(cases, 1):
        name = f"{case['spec']}/{case['name']}"
        if case.get("pending"):
            results.append(("PENDING", name, f"waits on {case['pending']}" + (f": {case['why']}" if case.get("why") else "")))
            continue
        if failed and (os.environ.get("PORTS_KEEP_GOING") != "1" or guest.process.poll() is not None):
            results.append(("SKIPPED", name, "an earlier case failed"))
            continue
        try:
            run_case(bt, guest, case, number)
            results.append(("PASS", name, ""))
        except CaseFailed as error:
            failed = True
            results.append(("FAIL", name, str(error)))
            if guest.process.poll() is not None:
                continue
            tail = guest.log[-bt.LOG_TAIL:].decode(errors="replace")
            print(f"--- serial log tail ---\n{tail}\n---", file=sys.stderr)
    if guest is not None and guest.process.poll() is None:
        if not failed:
            guest.send("poweroff\r")
            try:
                guest.wait_exit()
            except SystemExit:
                pass
        if guest.process.poll() is None:
            guest.process.kill()
    for verdict, name, note in results:
        print(f"{port} {verdict:7} {name}" + (f"  ({note})" if note else ""), flush=True)
    counts = {v: sum(1 for r in results if r[0] == v) for v in ("PASS", "FAIL", "SKIPPED", "PENDING")}
    print(f"{port}: {counts['PASS']} passed, {counts['FAIL']} failed, {counts['SKIPPED']} skipped, "
          f"{counts['PENDING']} pending", flush=True)
    sys.exit(1 if counts["FAIL"] or counts["SKIPPED"] else 0)


if __name__ == "__main__":
    main()
