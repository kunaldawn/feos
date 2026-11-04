#!/usr/bin/env python3
"""Boots a kernel headless in QEMU and drives the serial shell.

    ./scripts/boot-test.py arm target/aarch64-unknown-none/release/feos
    ./scripts/boot-test.py x64 target/x86_64-unknown-none/release/feos

Exits non-zero, printing the serial log, when a step fails.
"""

import os
import select
import subprocess
import sys
import time

BOOT_TIMEOUT = 120
STEP_TIMEOUT = 60
EXIT_TIMEOUT = 30
LOG_TAIL = 4000


def qemu_command(arch, kernel):
    common = ["-m", "512M", "-net", "none", "-device", "ramfb", "-display", "none"]
    common += ["-audiodev", "wav,id=snd0,path=/tmp/feos-boot-test.wav"]
    common += ["-device", "intel-hda", "-device", "hda-output,audiodev=snd0"]
    common += ["-serial", "mon:stdio"]
    if arch == "arm":
        return ["qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a53", "-smp", "4"] + common + [
            "-device", "virtio-tablet-device", "-device", "virtio-keyboard-device", "-kernel", kernel]
    # QEMU's multiboot loader needs a flat image, not the linked ELF.
    flat = "/tmp/feos-boot-test.bin"
    subprocess.run([os.environ.get("OBJCOPY", "objcopy"), "-O", "binary", kernel, flat], check=True)
    return ["qemu-system-x86_64", "-machine", "q35", "-cpu", "max", "-smp", "1", "-vga", "none"] + common + [
        "-kernel", flat]


class Guest:
    def __init__(self, command):
        self.process = subprocess.Popen(
            command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
        )
        self.log = b""
        self.mark = 0

    def pump(self, wait):
        ready, _, _ = select.select([self.process.stdout], [], [], wait)
        if ready:
            data = os.read(self.process.stdout.fileno(), 65536)
            self.log += data
            return bool(data)
        return True

    def expect(self, text, timeout=STEP_TIMEOUT):
        wanted = text.encode()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            found = self.log.find(wanted, self.mark)
            if found >= 0:
                self.mark = found + len(wanted)
                return
            if b"[panic]" in self.log[self.mark:]:
                fail(self, f"kernel panic while waiting for {text!r}")
            if not self.pump(0.2):
                break
        fail(self, f"timed out waiting for {text!r}")

    def send(self, text):
        self.process.stdin.write(text.encode())
        self.process.stdin.flush()

    def run(self, line, result):
        self.send(line + "\r")
        self.expect(result)

    def wait_exit(self):
        deadline = time.monotonic() + EXIT_TIMEOUT
        while time.monotonic() < deadline:
            self.pump(0.2)
            if self.process.poll() is not None:
                return
        fail(self, "QEMU did not exit after poweroff")


def fail(guest, message):
    tail = guest.log[-LOG_TAIL:].decode(errors="replace")
    print(f"--- serial log tail ---\n{tail}\n--- FAILED: {message}", file=sys.stderr)
    if guest.process.poll() is None:
        guest.process.kill()
    sys.exit(1)


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("arm", "x64"):
        sys.exit(__doc__)
    arch, kernel = sys.argv[1:]
    guest = Guest(qemu_command(arch, kernel))

    guest.expect("[desktop] session started", BOOT_TIMEOUT)
    guest.send("\x1c")
    guest.expect("entering the serial shell")

    # The typed command echoes without the computed value, so each result can
    # only come from running it.
    guest.run("echo boot-ok-$((40 + 2))", "boot-ok-42")
    guest.run("A='x y'; B=$A; echo \"[$B]\"", "[x y]")
    guest.run("spawn tick 1000 10000", "started pid 2")
    guest.run("kill 2", "killed pid 2")
    guest.run("echo after-kill-$((1 + 1))", "after-kill-2")

    guest.send("poweroff\r")
    guest.wait_exit()
    print(f"boot test passed: {arch}")


if __name__ == "__main__":
    main()
