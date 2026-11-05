#!/usr/bin/env python3
"""Boots a kernel headless in QEMU and drives the serial shell.

    ./scripts/boot-test.py arm target/aarch64-unknown-none/release/feos
    ./scripts/boot-test.py x64 target/x86_64-feos/release/feos

Exits non-zero, printing the serial log, when a step fails.
"""

import base64
import hashlib
import os
import re
import select
import subprocess
import sys
import time
import zlib

BOOT_TIMEOUT = 120
STEP_TIMEOUT = 60
EXIT_TIMEOUT = 30
LOG_TAIL = 4000


DISK_SIZE = 512 * 1024 * 1024


def work_path(kernel, name):
    """A scratch file beside the kernel: /tmp may be RAM, and these can be big."""
    return os.path.join(os.path.dirname(os.path.abspath(kernel)), "boot-test-" + name)


def qemu_command(arch, kernel, disk_args=(), display=("-device", "ramfb"), smp=None, memory="512M", machine="virt", net_args=None, virtio_input=True):
    # Set by the tests that care; otherwise, what the ports are normally run with.
    smp = str(smp or os.environ.get("FEOS_BOOT_TEST_SMP", "4"))
    common = ["-m", memory] + (list(net_args) if net_args else ["-net", "none"]) + ["-display", "none"] + list(display)
    common += ["-audiodev", "none,id=snd0"]
    common += ["-device", "intel-hda", "-device", "hda-output,audiodev=snd0"]
    common += ["-serial", "mon:stdio"] + list(disk_args)
    if arch == "arm":
        inputs = ["-device", "virtio-tablet-device", "-device", "virtio-keyboard-device"] if virtio_input else []
        return ["qemu-system-aarch64", "-machine", machine, "-cpu", "cortex-a53", "-smp", smp] + common + inputs + [
            "-kernel", kernel]
    # QEMU's multiboot loader needs a flat image, not the linked ELF.
    flat = work_path(kernel, "kernel.bin")
    subprocess.run([os.environ.get("OBJCOPY", "objcopy"), "-O", "binary", kernel, flat], check=True)
    return ["qemu-system-x86_64", "-machine", "q35", "-cpu", "max", "-smp", smp, "-vga", "none"] + common + [
        "-kernel", flat]


def blank_disk(kernel, name, size=DISK_SIZE):
    path = work_path(kernel, name)
    if os.path.exists(path):
        os.remove(path)
    with open(path, "wb") as image:
        image.truncate(size)
    return path


def disk_device(arch, path, transport=None):
    """QEMU arguments for a virtio disk: MMIO on arm, PCI on x64, by default."""
    if transport is None:
        transport = "mmio" if arch == "arm" else "pci"
    args = ["-drive", f"if=none,id=disk0,format=raw,file={path}"]
    if transport == "mmio":
        return args + ["-device", "virtio-blk-device,drive=disk0"]
    if transport == "bridge":
        return args + ["-device", "pcie-root-port,id=rp0,bus=pcie.0,chassis=1",
                       "-device", "virtio-blk-pci,bus=rp0,drive=disk0"]
    if transport == "pci-intx":
        # No MSI-X table: the device can only use its wired pin.
        return args + ["-device", "virtio-blk-pci,vectors=0,drive=disk0"]
    return args + ["-device", "virtio-blk-pci,drive=disk0"]


def fsck(path):
    """Asks a real FAT checker about an image, without letting it change anything."""
    result = subprocess.run(["fsck.vfat", "-n", path], capture_output=True, text=True)
    if result.returncode != 0:
        print(result.stdout + result.stderr, file=sys.stderr)
        sys.exit(f"fsck.vfat found errors in {path}")


class Guest:
    def __init__(self, command):
        self.process = subprocess.Popen(
            command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
        )
        self.log = b""
        self.mark = 0

    def monitor(self, command):
        """Runs a QEMU monitor command. Ctrl-A C switches stdio to the monitor."""
        self.send("\x01c")
        self.send(command + "\n")
        time.sleep(0.2)
        self.send("\x01c")
        time.sleep(0.1)

    def crash(self):
        """Pulls the plug: the guest gets no chance to write anything out."""
        self.process.kill()
        self.process.wait()

    def boot_to_shell(self):
        self.expect("[desktop] session started", BOOT_TIMEOUT)
        self.send("\x1c")
        self.expect("entering the serial shell")

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

    def capture(self, pattern, timeout=STEP_TIMEOUT):
        """Waits for a regular expression in new output and returns its group."""
        deadline = time.monotonic() + timeout
        compiled = re.compile(pattern.encode())
        while time.monotonic() < deadline:
            match = compiled.search(self.log, self.mark)
            if match:
                self.mark = match.end()
                return match.group(1).decode()
            if b"[panic]" in self.log[self.mark:]:
                fail(self, f"kernel panic while waiting for {pattern!r}")
            if not self.pump(0.2):
                break
        fail(self, f"timed out waiting for {pattern!r}")

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


def persistence_test(arch, kernel):
    """Files outlive the machine, whether it shuts down or loses power."""
    disk = blank_disk(kernel, "disk.img")
    args = disk_device(arch, disk)

    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("[gfx] ramfb 1024x768", BOOT_TIMEOUT)
    guest.expect("blank disk, formatted as FAT32", BOOT_TIMEOUT)
    guest.expect("copied the sample files")
    guest.boot_to_shell()
    guest.run("mount | grep -c fat32 | tr 1 Y", "\nY")
    guest.run("cat /README.txt | wc -l | tr 0-9 N; echo seeded-$?", "seeded-0")
    guest.run("mkdir -p /keep/deep && echo clean-$((6 * 7)) > /keep/deep/note.txt", "$ ")
    guest.run("cat /keep/deep/note.txt", "clean-42")
    guest.send("poweroff\r")
    guest.wait_exit()
    fsck(disk)

    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("[fs] virtio0: mounted at /", BOOT_TIMEOUT)
    guest.boot_to_shell()
    guest.run("cat /keep/deep/note.txt", "clean-42")
    # A crash right after this must lose nothing: `sync` has returned.
    guest.run("echo synced-$((5 * 5)) > /keep/synced.txt; sync; echo sync-done-$?", "sync-done-0")
    # And one that's not synced can be lost, but mustn't damage anything.
    guest.run("echo unsynced > /keep/unsynced.txt; rm /keep/deep/note.txt; echo sent-$((1))", "sent-1")
    guest.crash()

    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("[fs] virtio0: mounted at /", BOOT_TIMEOUT)
    guest.boot_to_shell()
    guest.run("cat /keep/synced.txt", "synced-25")
    guest.run("mkdir /keep/after && echo works-$? > /keep/after/f; cat /keep/after/f", "works-0")
    # Settings saved in one session are what the next one starts with.
    guest.run("printf 'theme: xp-blue\\nkeyboard: de\\n' > /etc/desktop.conf; sync; echo saved-$?", "saved-0")
    # Timestamps come from the hardware clock: a year nobody would make up.
    guest.send("date +year-%Y\r")
    year = int(guest.capture(r"year-(\d{4})"))
    if year < 2024:
        fail(guest, f"the clock says {year}")
    guest.run("stat /keep/after/f | grep Modify | tr -d -c 0-9 | cut -c1-4 | sed s/^/stamp-/", f"stamp-{year}")
    guest.run("ln -s /keep/after/f /keep/link; cat /keep/link; readlink /keep/link", "/keep/after/f")
    guest.run("mount | grep -c 'on /tmp' | tr 1 T", "\nT")
    guest.send("poweroff\r")
    guest.wait_exit()
    fsck(disk)

    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("settings restored: theme=xp-blue keyboard=de", BOOT_TIMEOUT)
    guest.boot_to_shell()
    guest.run("cat /keep/link", "works-0")
    guest.send("poweroff\r")
    guest.wait_exit()
    fsck(disk)
    os.remove(disk)


def open_terminal(guest):
    """Opens the Terminal from the Start menu, with real key presses."""
    bundles = sorted(name for name in os.listdir("apps/desktop/apps") if name.endswith(".fapp"))
    guest.monitor("sendkey ctrl-esc")
    time.sleep(1)
    guest.monitor("sendkey tab")
    time.sleep(0.5)
    for _ in range(bundles.index("terminal.fapp")):
        guest.monitor("sendkey right")
        time.sleep(0.2)
    guest.monitor("sendkey ret")
    # The window takes a moment to start its shell.
    time.sleep(5)


def milestone_test(arch, kernel):
    """A file created in Terminal shows up in Explorer, and survives a reboot."""
    disk = blank_disk(kernel, "disk-milestone.img")
    args = disk_device(arch, disk)

    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("[desktop] session started", BOOT_TIMEOUT)
    time.sleep(2)
    open_terminal(guest)
    guest.send("echo made-in-terminal > /terminal-note.txt\r")
    time.sleep(2)
    guest.send("files /\r")
    # Explorer logs what it lists, as a window can't be read from out here.
    guest.capture(r"\[explorer\] /:[^\r\n]* (terminal-note\.txt)")

    # With Explorer still open, a change elsewhere reaches it unprompted.
    time.sleep(2)
    guest.monitor("sendkey alt-tab")
    time.sleep(1)
    guest.send("echo also > /live-note.txt\r")
    guest.capture(r"\[explorer\] /:[^\r\n]* (live-note\.txt)")
    guest.monitor(f"screendump {work_path(kernel, 'milestone.ppm')}")
    time.sleep(1)

    # Explorer changes the same tree. It has the keyboard after Alt-Tab, with the
    # cursor on the first row. The sorted listing has `bin`, the user programs, then
    # `dev`, a mount, and then `docs`, a folder from the image.
    time.sleep(1)
    guest.monitor("sendkey alt-tab")
    time.sleep(1)
    guest.monitor("sendkey down")
    time.sleep(0.5)
    guest.monitor("sendkey down")
    time.sleep(0.5)
    guest.monitor("sendkey ctrl-c")
    guest.monitor("sendkey ctrl-v")
    guest.capture(r"\[explorer\] /:[^\r\n]* (Copy of docs)")
    guest.monitor("sendkey f2")
    time.sleep(1)
    for _ in "docs":
        guest.monitor("sendkey backspace")
    guest.send("kept\r")
    guest.capture(r"\[explorer\] /:[^\r\n]* (kept)")
    guest.monitor("sendkey delete")
    time.sleep(1)
    guest.monitor("sendkey ret")
    listing = guest.capture(r"\[explorer\] /:([^\r\n]*)")
    if " kept" in listing or "terminal-note.txt" not in listing:
        fail(guest, f"after deleting a folder, Explorer lists:{listing}")

    # Power off the hard way, a few seconds later: the flusher has had its time.
    time.sleep(3)
    guest.crash()

    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("[fs] virtio0: mounted at /", BOOT_TIMEOUT)
    guest.expect("[desktop] session started", BOOT_TIMEOUT)
    time.sleep(2)
    open_terminal(guest)
    guest.send("files /\r")
    guest.capture(r"\[explorer\] /:[^\r\n]* (terminal-note\.txt)")
    time.sleep(1)
    guest.send("\x1c")
    guest.expect("entering the serial shell")
    guest.run("cat /terminal-note.txt", "made-in-terminal")
    guest.run("cat '/Copy of docs/getting-started.md' | wc -l | tr 0-9 N; echo copied-$?", "copied-0")
    guest.run("cat /kept/getting-started.md >/dev/null 2>&1; echo gone-$?", "gone-1")
    guest.send("poweroff\r")
    guest.wait_exit()
    fsck(disk)
    os.remove(disk)


def partition_test(arch, kernel):
    """A disk laid out by other tools mounts from inside its partition table."""
    sector = 512
    start = 2048
    count = (64 * 1024 * 1024) // sector
    total = start + count + 2048

    def lay_out(name, write_table):
        path = blank_disk(kernel, name, total * sector)
        subprocess.run(["mkfs.vfat", "-F", "32", "--offset", str(start), path, str(count // 2)],
                       check=True, capture_output=True)
        with open(path, "r+b") as image:
            write_table(image)
        return path

    def mbr(image, kind=0x0c):
        image.seek(446)
        image.write(bytes([0, 0, 0, 0, kind, 0, 0, 0]) + start.to_bytes(4, "little")
                    + count.to_bytes(4, "little"))
        image.seek(510)
        image.write(b"\x55\xaa")

    def gpt(image):
        mbr(image, 0xee)
        basic_data = bytes.fromhex("a2a0d0ebe5b9334487c068b6b72699c7")
        entry = basic_data + b"\x11" * 16 + start.to_bytes(8, "little")
        entry += (start + count - 1).to_bytes(8, "little") + bytes(56)
        table = entry.ljust(128 * 128, b"\0")
        image.seek(2 * sector)
        image.write(table)
        header = bytearray(92)
        header[:8] = b"EFI PART"
        header[8:12] = (0x00010000).to_bytes(4, "little")
        header[12:16] = (92).to_bytes(4, "little")
        header[24:32] = (1).to_bytes(8, "little")
        header[32:40] = (total - 1).to_bytes(8, "little")
        header[40:48] = (34).to_bytes(8, "little")
        header[48:56] = (total - 34).to_bytes(8, "little")
        header[72:80] = (2).to_bytes(8, "little")
        header[80:84] = (128).to_bytes(4, "little")
        header[84:88] = (128).to_bytes(4, "little")
        header[88:92] = zlib.crc32(table).to_bytes(4, "little")
        header[16:20] = zlib.crc32(bytes(header)).to_bytes(4, "little")
        image.seek(sector)
        image.write(header)

    for name, table in (("mbr", mbr), ("gpt", gpt)):
        disk = lay_out(f"disk-{name}.img", table)
        guest = Guest(qemu_command(arch, kernel, disk_device(arch, disk)))
        guest.expect("[fs] virtio0p1: mounted at /", BOOT_TIMEOUT)
        guest.boot_to_shell()
        guest.run(f"echo in-partition-$((6 * 6)) > /{name}; sync; cat /{name}", "in-partition-36")
        guest.send("poweroff\r")
        guest.wait_exit()
        os.remove(disk)


def format_test(arch, kernel):
    """A disk too small for FAT32 becomes exFAT; old FAT formats still read."""
    disk = blank_disk(kernel, "disk-small.img", 16 * 1024 * 1024)
    args = disk_device(arch, disk)
    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("blank disk, formatted as exFAT", BOOT_TIMEOUT)
    guest.boot_to_shell()
    guest.run("mount | grep -c exfat | tr 1 Y", "\nY")
    guest.run("mkdir /d; echo on-exfat-$((3 * 3)) > /d/f; sync; cat /d/f", "on-exfat-9")
    guest.send("poweroff\r")
    guest.wait_exit()
    result = subprocess.run(["fsck.exfat", "-n", disk], capture_output=True, text=True)
    if result.returncode != 0:
        sys.exit(f"fsck.exfat found errors:\n{result.stdout}{result.stderr}")

    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("[fs] virtio0: mounted at /", BOOT_TIMEOUT)
    guest.boot_to_shell()
    guest.run("cat /d/f", "on-exfat-9")
    guest.send("poweroff\r")
    guest.wait_exit()
    os.remove(disk)

    for bits in ("12", "16"):
        disk = blank_disk(kernel, f"disk-fat{bits}.img", 8 * 1024 * 1024)
        # One sector per cluster, or eight megabytes is too few clusters for FAT16.
        subprocess.run(["mkfs.vfat", "-F", bits, "-s", "1" if bits == "16" else "8", disk],
                       check=True, capture_output=True)
        guest = Guest(qemu_command(arch, kernel, disk_device(arch, disk)))
        guest.expect("[fs] virtio0: mounted at /", BOOT_TIMEOUT)
        guest.boot_to_shell()
        guest.run("echo old-fat-$((4 * 4)); ls / > /tmp/l; echo listed-$?", "listed-0")
        guest.send("poweroff\r")
        guest.wait_exit()
        os.remove(disk)


def stored_zip(name, data):
    """One member, uncompressed: the image's Python has no zipfile module."""
    def le(value, size):
        return value.to_bytes(size, "little")

    name = name.encode()
    fields = le(20, 2) + le(0, 2) + le(0, 2) + le(0, 2) + le(0, 2) + le(zlib.crc32(data), 4)
    fields += le(len(data), 4) + le(len(data), 4) + le(len(name), 2) + le(0, 2)
    local = b"PK\x03\x04" + fields + name + data
    central = b"PK\x01\x02" + le(20, 2) + fields + le(0, 2) + le(0, 2) + le(0, 2) + le(0, 4) + le(0, 4) + name
    end = b"PK\x05\x06" + le(0, 2) + le(0, 2) + le(1, 2) + le(1, 2)
    end += le(len(central), 4) + le(len(local), 4) + le(0, 2)
    return local + central + end


def install_test(arch, kernel):
    """A package typed into a Terminal appears in the catalog, and stays."""
    package = stored_zip("manifest.yaml", b'id: runtime-demo\nname: Runtime Demo\nversion: "1"\nentry: ps\n')
    digest = hashlib.sha256(package).hexdigest()
    encoded = base64.b64encode(package).decode()

    disk = blank_disk(kernel, "disk-install.img", 64 * 1024 * 1024)
    args = disk_device(arch, disk)
    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("[desktop] session started", BOOT_TIMEOUT)
    time.sleep(2)
    open_terminal(guest)
    for start in range(0, len(encoded), 120):
        guest.send(f"printf %s {encoded[start:start + 120]} >> /tmp/p.b64\r")
        time.sleep(0.5)
    guest.send("base64 -d /tmp/p.b64 > /tmp/p.fapp\r")
    time.sleep(1)
    # With a wrong digest, nothing's installed. The right one is noticed at once.
    guest.send(f"appinstall /tmp/p.fapp {'0' * 64} || appinstall /tmp/p.fapp {digest}\r")
    guest.expect("[desktop] installed app: runtime-demo")
    guest.expect("after a change to the application folder")
    time.sleep(1)
    guest.send("\x1c")
    guest.expect("entering the serial shell")
    guest.run("ls /apps | wc -l | tr 2 Y", "\nY")
    guest.run("appinstall /tmp/p.fapp 2>&1 | grep -c 'no SHA-256' | tr 1 N", "\nN")
    guest.send("poweroff\r")
    guest.wait_exit()
    fsck(disk)

    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("[desktop] installed app: runtime-demo", BOOT_TIMEOUT)
    guest.send("\x1c")
    guest.expect("entering the serial shell")
    # A package the disk has changed since is left out.
    guest.run("printf x >> /apps/runtime-demo.fapp; sync; echo tampered-$?", "tampered-0")
    guest.send("poweroff\r")
    guest.wait_exit()

    guest = Guest(qemu_command(arch, kernel, args))
    guest.expect("runtime-demo.fapp: package does not match its digest, not loaded", BOOT_TIMEOUT)
    guest.send("\x1c")
    guest.expect("entering the serial shell")
    guest.send("poweroff\r")
    guest.wait_exit()
    os.remove(disk)


def read_ppm(path):
    """Width, height and pixels of a QEMU screendump."""
    with open(path, "rb") as image:
        data = image.read()
    magic, size, _, pixels = data.split(b"\n", 3)
    width, height = map(int, size.split())
    return width, height, pixels


def gpu_test(arch, kernel):
    """A virtio-gpu sets the mode, and shows only what it's told about."""
    display = ("-device", "virtio-gpu-pci,xres=800,yres=600")
    guest = Guest(qemu_command(arch, kernel, (), display))
    guest.expect("[gfx] virtio-gpu 800x600", BOOT_TIMEOUT)
    guest.expect("[desktop] session started", BOOT_TIMEOUT)
    time.sleep(3)

    before = work_path(kernel, "gpu-before.ppm")
    guest.monitor(f"screendump {before}")
    time.sleep(1)
    width, height, first = read_ppm(before)
    if (width, height) != (800, 600):
        fail(guest, f"the display is {width}x{height}")
    if len(set(first[index] for index in range(0, len(first), 977))) < 4:
        fail(guest, "the display is blank: flushes aren't reaching the host")

    # A window opening is damage, which has to get through by itself.
    open_terminal(guest)
    after = work_path(kernel, "gpu-after.ppm")
    guest.monitor(f"screendump {after}")
    time.sleep(1)
    _, _, second = read_ppm(after)
    if first == second:
        fail(guest, "opening a window changed nothing on the display")

    guest.send("\x1c")
    guest.expect("entering the serial shell")
    guest.run("ps | grep -c displayd | tr 1 D", "\nD")
    guest.send("poweroff\r")
    guest.wait_exit()
    os.remove(before)
    os.remove(after)


def transport_test(arch, kernel):
    """The disk works on every path to it the machine offers."""
    variants = ["bridge"] if arch == "x64" else ["pci", "bridge"]
    for transport in variants:
        disk = blank_disk(kernel, "disk-transport.img", 64 * 1024 * 1024)
        guest = Guest(qemu_command(arch, kernel, disk_device(arch, disk, transport)))
        guest.expect("blank disk, formatted as FAT32", BOOT_TIMEOUT)
        guest.boot_to_shell()
        guest.run(f"echo via-$((1 + 1)) > /t; sync; cat /t", "via-2")
        guest.send("poweroff\r")
        guest.wait_exit()
        fsck(disk)
        os.remove(disk)


def gicv3_test(arch, kernel):
    """A machine with a GICv3 works as one with a GICv2 does. AArch64 only."""
    if arch != "arm":
        return
    machine = "virt,gic-version=3"
    disk = blank_disk(kernel, "disk-gicv3.img", 64 * 1024 * 1024)
    args = disk_device(arch, disk, "mmio")
    guest = Guest(qemu_command(arch, kernel, args, machine=machine))
    guest.expect("[sched] GICv3", BOOT_TIMEOUT)
    guest.boot_to_shell()
    guest.send("ktest cpus\r")
    guest.expect("online=4 ran-on=4 ticking=4")
    guest.run("echo gicv3-$((5 * 5)) > /t; sync; cat /t", "gicv3-25")
    # Interrupts from EL0 are claimed through system registers on this one.
    guest.run("/bin/spin 1 500 | tr a-z A-Z | tr = :; /bin/malicious --probe kernel-read; echo el0-$?", "el0-139")
    guest.send("ktest irqs\r")
    count = int(guest.capture(r"irq +\d+ virtio-blk mmio +count=(\d+)"))
    if count == 0:
        fail(guest, "no interrupts from the disk")
    guest.send("ktest timer\r")
    guest.expect("precise=true quiet=true")
    print(f"gicv3: {count} disk interrupts", flush=True)
    guest.send("poweroff\r")
    guest.wait_exit()
    fsck(disk)
    os.remove(disk)

    # PCI's wired interrupts, with no MSI to fall back on without an ITS.
    disk = blank_disk(kernel, "disk-gicv3.img", 64 * 1024 * 1024)
    guest = Guest(qemu_command(arch, kernel, disk_device(arch, disk, "pci"), machine=machine))
    guest.expect(": INTx", BOOT_TIMEOUT)
    guest.boot_to_shell()
    guest.run("echo gicv3-$((5 * 6)) > /t; sync; cat /t", "gicv3-30")
    guest.send("ktest irqs\r")
    count = int(guest.capture(r"irq +\d+ virtio-blk intx +count=(\d+)"))
    if count == 0:
        fail(guest, "no interrupts from the PCI disk")
    guest.send("poweroff\r")
    guest.wait_exit()
    fsck(disk)
    os.remove(disk)

    # A failure on one CPU still stops the rest, through SGIs.
    guest = Guest(qemu_command(arch, kernel, machine=machine))
    guest.boot_to_shell()
    guest.send("ktest panic\r")
    guest.expect("[fatal] stopped ")
    guest.process.kill()


def sched_test(arch, kernel):
    """Priorities, affinity and one-shot timers, on one CPU and several."""
    for smp in (4, 1):
        guest = Guest(qemu_command(arch, kernel, smp=smp))
        guest.boot_to_shell()
        guest.send("ktest timer\r")
        report = guest.capture(r"\[ktest\] (timer:[^\r\n]*)[\r\n]")
        print(f"sched {arch} -smp {smp}: {report}", flush=True)
        if "precise=true quiet=true" not in report:
            fail(guest, report)
        guest.send("ktest priority\r")
        report = guest.capture(r"\[ktest\] (priority:[^\r\n]*)[\r\n]")
        print(f"sched {arch} -smp {smp}: {report}", flush=True)
        if "favoured=true" not in report:
            fail(guest, report)
        if smp > 1:
            guest.send("ktest affinity\r")
            report = guest.capture(r"\[ktest\] (affinity:[^\r\n]*)[\r\n]")
            print(f"sched {arch} -smp {smp}: {report}", flush=True)
            if "ok=true" not in report:
                fail(guest, report)
        # The commands, on a real task.
        guest.send("spawn tick 1000 10000\r")
        pid = guest.capture(r"started pid (\d+): tick")
        guest.run(f"renice 12 {pid}", f"pid {pid}: priority 12")
        guest.run(f"taskset 1 {pid}", f"pid {pid}: CPUs 0x1")
        guest.run(f"renice 99 {pid}; echo status-$?", "status-2")
        guest.run(f"kill {pid}", f"killed pid {pid}")
        guest.send("poweroff\r")
        guest.wait_exit()


def fault_test(arch, kernel):
    """A task that faults is ended, and the machine carries on. A fault with a lock
    held stops it, with a report."""
    guest = Guest(qemu_command(arch, kernel))
    guest.boot_to_shell()
    for command, report in (
        ("overflow", "stack overflow into the guard page"),
        ("null", ": "),
        ("task-panic", "a task's panic"),
    ):
        guest.send(f"ktest {command}\r")
        guest.expect("[fault] pid ")
        guest.expect(report)
        guest.expect("; ended")
        guest.run("echo status-$?", "status-139")
        guest.run(f"echo survived-$((6 * 7))", "survived-42")
        print(f"fault {arch} {command}: contained", flush=True)
    # The rest of the machine is still whole.
    guest.send("spawn tick 1000 10000\r")
    pid = guest.capture(r"started pid (\d+): tick")
    guest.run(f"kill {pid}", f"killed pid {pid}")
    guest.run("ktest cpus", "ran-on=4")
    guest.send("poweroff\r")
    guest.wait_exit()

    guest = Guest(qemu_command(arch, kernel))
    guest.boot_to_shell()
    guest.send("ktest overflow-locked\r")
    guest.expect("[fatal] stack overflow into a guard page")
    guest.expect("system halted")
    print(f"fault {arch} overflow-locked: machine stopped with a report", flush=True)
    guest.process.kill()


def ram_test(arch, kernel):
    """The kernel uses whatever RAM it's given, however much that is."""
    # With 3 GiB on x86_64, QEMU puts the last of it above 4 GiB, where nothing reaches.
    # Up to 2.75 GiB, it's all below the PCI hole.
    for memory, mib in (("192M", 192), ("512M", 512), ("1G", 1024), ("2600M", 2600), ("3G", 2048 if arch == "x64" else 3072)):
        guest = Guest(qemu_command(arch, kernel, memory=memory))
        report = guest.capture(r"\[mem\] (\d+ MiB of RAM)", BOOT_TIMEOUT)
        guest.boot_to_shell()
        guest.send("ktest mem\r")
        guest.expect("aligned=True intact=True restored=True".lower())
        total = int(report.split()[0])
        # A little goes to firmware, and on x86 to its reserved low memory.
        if not mib - 32 <= total <= mib:
            fail(guest, f"{memory} of RAM reported as {total} MiB")
        print(f"ram {arch} {memory}: {report}", flush=True)
        guest.run("echo after-mem-$((6 * 7))", "after-mem-42")
        guest.send("poweroff\r")
        guest.wait_exit()


def irq_test(arch, kernel):
    """A disk's requests complete through whichever interrupt it can use."""
    # The audio controller's codec responses are the machine's plain MSI.
    guest = Guest(qemu_command(arch, kernel))
    guest.boot_to_shell()
    guest.send("ktest irqs\r")
    count = int(guest.capture(r"irq +\d+ intel-hda msi +count=(\d+)"))
    if count == 0:
        fail(guest, "no MSI from the audio controller")
    print(f"irq {arch} plain MSI: {count} interrupts", flush=True)
    guest.send("poweroff\r")
    guest.wait_exit()

    variants = [("pci", "MSI-X", "msi-x"), ("pci-intx", "INTx", "intx"), ("bridge", "MSI-X", "msi-x")]
    if arch == "arm":
        variants.insert(0, ("mmio", "line", "mmio"))
    for transport, how, name in variants:
        disk = blank_disk(kernel, "disk-irq.img", 64 * 1024 * 1024)
        guest = Guest(qemu_command(arch, kernel, disk_device(arch, disk, transport)))
        guest.expect(f": {how}", BOOT_TIMEOUT)
        guest.boot_to_shell()
        guest.run("echo irq-$((3 * 3)) > /t; sync; cat /t", "irq-9")
        guest.send("ktest irqs\r")
        count = int(guest.capture(rf"irq +\d+ virtio-blk {name} +count=(\d+)"))
        if count == 0:
            fail(guest, f"no {how} interrupts arrived")
        print(f"irq {arch} {transport}: {count} interrupts", flush=True)
        guest.send("poweroff\r")
        guest.wait_exit()
        fsck(disk)
        os.remove(disk)


def smp_test(arch, kernel):
    """Tasks run on every CPU, and a failure on one stops the others."""
    for smp in (4, 1):
        guest = Guest(qemu_command(arch, kernel, smp=smp))
        guest.boot_to_shell()
        guest.send("ktest cpus\r")
        guest.expect(f"[ktest] cpus: online={smp} ran-on={smp} ticking={smp}")
        guest.send("ktest fp\r")
        guest.expect("failures=0", 120)
        guest.send("spawn tick 1000 10000\r")
        pid = guest.capture(r"started pid (\d+): tick")
        guest.run(f"kill {pid}", f"killed pid {pid}")
        guest.send("poweroff\r")
        guest.wait_exit()

    guest = Guest(qemu_command(arch, kernel, smp=4))
    guest.boot_to_shell()
    guest.send("ktest panic\r")
    guest.expect("[fatal] stopped ")
    stopped = guest.capture(r"(\d) of 3 other CPU")
    guest.expect("ktest: deliberate panic")
    guest.expect("system halted")
    # GICv2 has no NMI, so a CPU with interrupts masked can only be timed out on.
    wanted = 3 if arch == "x64" else 1
    if int(stopped) < wanted:
        fail(guest, f"only {stopped} other CPUs stopped")
    guest.process.kill()


class NetPeer:
    """The far end of the guest's network, all of it on this side of QEMU's slirp.

    A small HTTP server, a server that sends a blob and reads one back, and a name
    server for slirp to forward 10.0.2.3 to. Nothing leaves this machine.
    """

    # The port stays taken from one boot to the next, so there's only one.
    name_server = None
    dns_queries = 0

    def __init__(self, blob):
        import socket
        import threading

        self.socket = socket
        self.blob = blob
        self.blob_sum = hashlib.sha256(blob).hexdigest()
        self.received = {}
        self.done = threading.Event()

        self.http_port = self.listen(self.serve_http)
        self.echo_port = self.listen(self.serve_echo)
        self.sink_port = self.listen(self.serve_sink)
        self.dns_ok = self.start_dns(threading)

    def listen(self, handler, port=0):
        import threading

        server = self.socket.socket()
        server.setsockopt(self.socket.SOL_SOCKET, self.socket.SO_REUSEADDR, 1)
        server.bind(("127.0.0.1", port))
        server.listen(8)

        def loop():
            while True:
                connection, _ = server.accept()
                thread = threading.Thread(target=self.guard, args=(handler, connection), daemon=True)
                thread.start()

        threading.Thread(target=loop, daemon=True).start()
        return server.getsockname()[1]

    def guard(self, handler, connection):
        try:
            with connection:
                connection.settimeout(120)
                handler(connection)
        except OSError as error:
            print(f"peer: {handler.__name__}: {error}", flush=True)

    def serve_http(self, connection):
        request = b""
        while b"\r\n\r\n" not in request:
            piece = connection.recv(4096)
            if not piece:
                return
            request += piece
        path = request.split(b" ")[1]
        if path == b"/big.bin":
            head = b"HTTP/1.0 200 OK\r\nContent-Length: %d\r\n\r\n" % len(self.blob)
            connection.sendall(head + self.blob)
        elif path == b"/hello.txt":
            body = b"hello over http\n"
            connection.sendall(b"HTTP/1.0 200 OK\r\nContent-Length: %d\r\n\r\n%s" % (len(body), body))
        else:
            connection.sendall(b"HTTP/1.0 404 Not Found\r\n\r\n")

    def serve_echo(self, connection):
        while True:
            piece = connection.recv(65536)
            if not piece:
                return
            connection.sendall(piece.upper())

    def serve_sink(self, connection):
        """Takes all the guest will send, and remembers what it came to."""
        digest = hashlib.sha256()
        total = 0
        while True:
            piece = connection.recv(1 << 18)
            if not piece:
                break
            digest.update(piece)
            total += len(piece)
        self.received["sink"] = (total, digest.hexdigest())
        self.done.set()

    def start_dns(self, threading):
        """Answers for feos.test on port 53, which needs this to run as root."""
        if NetPeer.name_server is not None:
            return True
        # Slirp asks the name server in resolv.conf. A test has no business asking a
        # real one, so this only goes ahead with it pointing here.
        try:
            with open("/etc/resolv.conf") as source:
                servers = [line.split()[1] for line in source if line.startswith("nameserver")]
        except OSError:
            servers = []
        if servers[:1] != ["127.0.0.1"]:
            print("peer: resolv.conf doesn't point here, so no DNS test", flush=True)
            return False
        udp = self.socket.socket(self.socket.AF_INET, self.socket.SOCK_DGRAM)
        try:
            udp.bind(("127.0.0.1", 53))
        except OSError as error:
            print(f"peer: no name server: {error}", flush=True)
            return False
        NetPeer.name_server = udp

        def loop():
            while True:
                query, client = udp.recvfrom(1500)
                NetPeer.dns_queries += 1
                name = query[12:query.index(b"\0", 12)]
                code = 0 if name == b"\x04feos\x04test" else 3
                answer = query[:2] + bytes([0x81, 0x80 | code]) + query[4:6]
                answer += (1 if code == 0 else 0).to_bytes(2, "big") + bytes(4) + query[12:]
                if code == 0:
                    answer += bytes([0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 10, 0, 2, 99])
                udp.sendto(answer, client)

        threading.Thread(target=loop, daemon=True).start()
        return True


def connect_retrying(socket, port, timeout=30):
    """Connects to what the guest listens on through hostfwd, which may not be up yet."""
    deadline = time.monotonic() + timeout
    while True:
        connection = socket.socket()
        connection.settimeout(120)
        try:
            connection.connect(("127.0.0.1", port))
            return connection
        except OSError:
            connection.close()
            if time.monotonic() > deadline:
                raise
            time.sleep(0.2)


def net_device(arch, transport):
    if transport == "mmio":
        return ["-device", "virtio-net-device,netdev=net0"]
    if transport == "bridge":
        return ["-device", "pcie-root-port,id=rpnet,bus=pcie.0,chassis=7", "-device", "virtio-net-pci,bus=rpnet,netdev=net0"]
    if transport == "intx":
        return ["-device", "virtio-net-pci,vectors=0,netdev=net0"]
    return ["-device", "virtio-net-pci,netdev=net0"]


def net_exercise(arch, kernel, transport, size, machine="virt", smp=None):
    """Lease, ping, DNS, TCP both ways, and a big transfer checked at both ends."""
    blob = bytes((index * 2654435761 >> 11) & 0xff for index in range(1 << 16)) * (size >> 16)
    peer = NetPeer(blob)
    forward = 18080
    netdev = (
        f"user,id=net0,net=10.0.2.0/24,dhcpstart=10.0.2.15"
        f",hostfwd=tcp:127.0.0.1:{forward}-:8000,hostfwd=tcp:127.0.0.1:{forward + 1}-:8001"
        f",guestfwd=tcp:10.0.2.100:9000-tcp:127.0.0.1:{peer.echo_port}"
    )
    args = ["-netdev", netdev] + net_device(arch, transport)
    guest = Guest(qemu_command(arch, kernel, net_args=args, machine=machine, smp=smp))
    guest.expect("[net] virtio-net ready", BOOT_TIMEOUT)
    guest.expect("[net] lease 10.0.2.15/24 gateway 10.0.2.2 dns 10.0.2.3", BOOT_TIMEOUT)
    guest.boot_to_shell()

    guest.run("ifconfig | grep -c 'inet 10.0.2.15' | tr 1 Y", "\nY")
    guest.send("ping -c 3 10.0.2.2\r")
    guest.expect("seq=2 time=")
    guest.expect("3 packets transmitted, 3 received")
    print(f"net {arch} {transport}: lease and ping", flush=True)

    # slirp forwards 10.0.2.3 to this container's name server, which is us, offline.
    if peer.dns_ok:
        guest.send("nslookup feos.test\r")
        guest.expect("Address: 10.0.2.99")
        guest.send("nslookup nope.test; echo dns-status-$?\r")
        guest.expect("name not found")
        guest.expect("dns-status-1")
        print(f"net {arch} {transport}: DNS through 10.0.2.3 ({peer.dns_queries} queries)", flush=True)
    else:
        print(f"net {arch} {transport}: DNS not tried", flush=True)

    # The guest connects out: to slirp's host address, and through a guestfwd.
    # A guestfwd goes through a QEMU chardev, which doesn't pass a close on in either
    # direction, so that one ends with Ctrl-C.
    guest.send("nc -s from-guest 10.0.2.100 9000\r")
    guest.expect("FROM-GUEST")
    time.sleep(0.5)
    guest.send("\x03")
    guest.run("echo interrupted-$?", "interrupted-130")
    guest.send(f"nc -s via-host 10.0.2.2 {peer.echo_port}\r")
    guest.expect("VIA-HOST")
    guest.expect("nc: done")
    guest.send(f"wget -O /tmp/hello.txt http://10.0.2.2:{peer.http_port}/hello.txt\r")
    guest.expect("saved 16 bytes")
    guest.run("cat /tmp/hello.txt", "hello over http")
    guest.send(f"wget -O /tmp/none http://10.0.2.2:{peer.http_port}/none; echo get-status-$?\r")
    guest.expect("server said 404")
    guest.expect("get-status-1")
    # A port nothing listens on is refused, not a hang.
    guest.send("nc -s x 10.0.2.2 1; echo refused-status-$?\r")
    guest.expect("refused-status-1")
    print(f"net {arch} {transport}: TCP out", flush=True)

    # The guest listens, and a peer outside connects in, through a hostfwd.
    socket = peer.socket
    guest.send("nc -l -s hello-from-guest 8000\r")
    guest.expect("listening on port 8000")
    connection = connect_retrying(socket, forward)
    connection.sendall(b"hello-from-host")
    connection.shutdown(socket.SHUT_WR)
    answer = b""
    while True:
        piece = connection.recv(4096)
        if not piece:
            break
        answer += piece
    connection.close()
    if answer != b"hello-from-guest":
        fail(guest, f"the guest answered {answer!r}")
    guest.expect("hello-from-host")
    guest.expect("nc: done")
    print(f"net {arch} {transport}: TCP in", flush=True)

    # A big transfer in, checked against what was sent.
    started = time.monotonic()
    guest.send(f"wget -O /tmp/big.bin http://10.0.2.2:{peer.http_port}/big.bin\r")
    guest.expect(f"saved {len(blob)} bytes", 300)
    took = time.monotonic() - started
    guest.send("sha256sum /tmp/big.bin\r")
    guest.expect(peer.blob_sum, 120)
    print(f"net {arch} {transport}: {len(blob)} bytes in, {took:.1f} s, sha256 matches", flush=True)

    # And the same file out to a sink, which works out its own checksum.
    peer.done.clear()
    started = time.monotonic()
    guest.send(f"nc -f /tmp/big.bin 10.0.2.2 {peer.sink_port}\r")
    guest.expect("nc: done", 300)
    took = time.monotonic() - started
    if not peer.done.wait(30):
        fail(guest, "the sink never saw the end of the transfer")
    total, digest = peer.received["sink"]
    if (total, digest) != (len(blob), peer.blob_sum):
        fail(guest, f"the sink got {total} bytes, sha256 {digest}")
    print(f"net {arch} {transport}: {total} bytes out, {took:.1f} s, sha256 matches", flush=True)

    # And into the guest from outside, saved and summed there. /tmp holds 16 MiB.
    guest.run("rm /tmp/big.bin; echo removed-$?", "removed-0")
    guest.send(f"nc -l -o /tmp/in.bin 8001\r")
    guest.expect("listening on port 8001")
    connection = connect_retrying(socket, forward + 1)
    connection.sendall(blob)
    connection.shutdown(socket.SHUT_WR)
    connection.recv(16)
    connection.close()
    guest.expect(f"received {len(blob)} bytes", 300)
    guest.send("sha256sum /tmp/in.bin\r")
    guest.expect(peer.blob_sum, 120)
    print(f"net {arch} {transport}: {len(blob)} bytes from outside, sha256 matches", flush=True)

    # Killing a task with sockets gives them back.
    guest.send("spawn nc -l 8002\r")
    pid = guest.capture(r"started pid (\d+): nc")
    time.sleep(1)
    guest.run("ifconfig -s | grep -c LISTEN | tr 1 Y", "\nY")
    guest.run(f"kill {pid}", f"killed pid {pid}")
    time.sleep(2)
    guest.run("ifconfig -s | grep -c LISTEN | tr 0 N", "\nN")

    guest.send("ifconfig\r")
    report = guest.capture(r"(tcp \d+ sockets, \d+ segments retransmitted)")
    print(f"net {arch} {transport}: {report}", flush=True)
    guest.run("ps | grep -c netd | tr 1 D", "\nD")
    guest.send("poweroff\r")
    guest.wait_exit()


def net_test(arch, kernel):
    """Everything over slirp, on each transport that matters to each machine."""
    size = int(os.environ.get("FEOS_NET_TEST_BYTES", str(8 << 20)))
    net_exercise(arch, kernel, "mmio" if arch == "arm" else "pci", size)
    # A smaller file on the others: they only differ in how frames reach the driver.
    small = 1 << 20
    variants = ["bridge", "intx"]
    if arch == "arm":
        variants.insert(0, "pci")
    for transport in variants:
        net_exercise(arch, kernel, transport, small)
    if arch == "arm":
        net_exercise(arch, kernel, "intx", small, machine="virt,gic-version=3")


USB_KEYS = {"\n": "ret", " ": "spc", ">": "shift-dot", ".": "dot", "-": "minus", "/": "slash", "$": "shift-4", "(": "shift-9",
            ")": "shift-0", "*": "shift-8", "+": "shift-equal", "=": "equal"}


def type_keys(guest, text):
    """Types through whichever keyboard QEMU has given the keys to."""
    for character in text:
        guest.monitor(f"sendkey {USB_KEYS.get(character, character)}")


class Qmp:
    """QEMU's machine protocol: an absolute pointer's events can only come this way,
    as the monitor's `mouse_move` is relative."""

    def __init__(self, path, timeout=30):
        import socket

        self.connection = socket.socket(socket.AF_UNIX)
        deadline = time.monotonic() + timeout
        while True:
            try:
                self.connection.connect(path)
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.1)
        self.connection.settimeout(10)
        self.read_line()
        self.send('{"execute":"qmp_capabilities"}')

    def read_line(self):
        data = b""
        while not data.endswith(b"\n"):
            piece = self.connection.recv(1)
            if not piece:
                break
            data += piece
        return data

    def send(self, text):
        self.connection.sendall(text.encode() + b"\n")
        while True:
            line = self.read_line()
            if b'"return"' in line or b'"error"' in line or not line:
                if b'"error"' in line:
                    sys.exit(f"QMP: {line!r}")
                return

    def pointer(self, x, y):
        self.send('{"execute":"input-send-event","arguments":{"events":['
                  f'{{"type":"abs","data":{{"axis":"x","value":{x}}}}},'
                  f'{{"type":"abs","data":{{"axis":"y","value":{y}}}}}]}}}}')

    def button(self, down):
        state = "true" if down else "false"
        self.send('{"execute":"input-send-event","arguments":{"events":['
                  f'{{"type":"btn","data":{{"down":{state},"button":"left"}}}}]}}}}')


def usb_stats(guest):
    guest.send("ktest usb\r")
    line = guest.capture(r"\[ktest\] usb: (devices=\d+ keyboard=\d+ pointer=\d+ storage=\d+)")
    return line, {key: int(value) for key, value in (part.split("=") for part in line.split())}


def usb_devices(stick):
    return ["-device", "qemu-xhci,id=xhci", "-device", "usb-kbd,bus=xhci.0", "-device", "usb-tablet,bus=xhci.0",
            "-drive", f"if=none,id=stick,format=raw,file={stick}", "-device", "usb-storage,bus=xhci.0,drive=stick"]


def make_stick(kernel, name, contents):
    stick = blank_disk(kernel, name, 64 * 1024 * 1024)
    subprocess.run(["mkfs.vfat", "-F", "32", stick], check=True, capture_output=True)
    source = work_path(kernel, "stick-source")
    with open(source, "wb") as out:
        out.write(contents)
    subprocess.run(["mcopy", "-i", stick, source, "::/BLOB.BIN"], check=True, capture_output=True)
    os.remove(source)
    return stick


def usb_test(arch, kernel):
    """A keyboard, a tablet and a stick on xHCI, with nothing else to take their place."""
    blob = bytes((index * 2654435761 >> 13) & 0xff for index in range(1 << 16)) * 32
    digest = hashlib.sha256(blob).hexdigest()
    stick = make_stick(kernel, "usb-stick.img", blob)
    control = work_path(kernel, "qmp.sock")
    if os.path.exists(control):
        os.remove(control)
    args = usb_devices(stick) + ["-qmp", f"unix:{control},server=on,wait=off"]
    guest = Guest(qemu_command(arch, kernel, args, virtio_input=False))
    qmp = Qmp(control)
    how = guest.capture(r"\[usb\] xHCI ready: ([^\r\n]*)", BOOT_TIMEOUT)
    guest.expect("[fs] usb0: mounted at /usb0", BOOT_TIMEOUT)
    guest.expect("[desktop] session started", BOOT_TIMEOUT)
    for line in (b": keyboard", b": tablet", b": mass storage, 64 MiB"):
        if line not in guest.log:
            fail(guest, f"no {line!r} from enumeration")
    print(f"usb {arch}: enumerated, interrupt {how}", flush=True)
    time.sleep(2)

    # The cursor is drawn on the framebuffer, and goes where the tablet says.
    before = work_path(kernel, "usb-before.ppm")
    after = work_path(kernel, "usb-after.ppm")
    guest.monitor(f"screendump {before}")
    time.sleep(1)
    qmp.pointer(8000, 8000)
    time.sleep(1)
    guest.monitor(f"screendump {after}")
    time.sleep(1)
    _, _, first = read_ppm(before)
    _, _, second = read_ppm(after)
    if first == second:
        fail(guest, "moving the tablet changed nothing on the screen")

    # A Terminal opened and typed in, all through the USB keyboard.
    open_terminal(guest)
    type_keys(guest, "echo typed-on-usb > /usb0/typed.txt\n".replace("\n", ""))
    guest.monitor("sendkey ret")
    time.sleep(3)
    guest.monitor(f"screendump {work_path(kernel, 'usb-terminal.ppm')}")
    time.sleep(1)
    guest.send("\x1c")
    guest.expect("entering the serial shell")
    line, stats = usb_stats(guest)
    print(f"usb {arch}: {line}", flush=True)
    if stats["keyboard"] < 10:
        fail(guest, "the keys didn't come through USB")
    if stats["pointer"] == 0:
        fail(guest, "the tablet never reported")
    guest.run("cat /usb0/typed.txt", "typed-on-usb")
    print(f"usb {arch}: typed a file into a Terminal through usb-kbd", flush=True)

    # The pointer path as apps see it: to the middle, and a click.
    guest.send("ktest input\r")
    guest.expect("input: start")
    time.sleep(0.3)
    qmp.pointer(16384, 8192)
    guest.capture(r"input: pointer (51\d) 19\d")
    qmp.button(True)
    qmp.button(False)
    guest.expect("left=Some(true)")
    guest.expect("input: done")

    # A file put there by something else reads back, and one written here goes out.
    guest.send("sha256sum /usb0/BLOB.BIN\r")
    guest.expect(digest, 120)
    guest.run("cp /usb0/BLOB.BIN /usb0/COPY.BIN; sync; echo copied-$?", "copied-0")
    guest.run("mkdir -p /usb0/sub/dir && echo nested-$((6 * 7)) > /usb0/sub/dir/f.txt; sync; echo made-$?", "made-0")
    print(f"usb {arch}: read and wrote the FAT volume", flush=True)
    guest.send("poweroff\r")
    guest.wait_exit()

    fsck(stick)
    copied = work_path(kernel, "usb-copied")
    subprocess.run(["mcopy", "-n", "-i", stick, "::/COPY.BIN", copied], check=True, capture_output=True)
    with open(copied, "rb") as source:
        if hashlib.sha256(source.read()).hexdigest() != digest:
            sys.exit("the copy on the stick doesn't match")
    os.remove(copied)
    nested = subprocess.run(["mtype", "-i", stick, "::/sub/dir/f.txt"], check=True, capture_output=True).stdout
    if b"nested-42" not in nested:
        sys.exit(f"the nested file holds {nested!r}")
    typed = subprocess.run(["mtype", "-i", stick, "::/typed.txt"], check=True, capture_output=True).stdout
    if b"typed-on-usb" not in typed:
        sys.exit(f"the typed file holds {typed!r}")
    print(f"usb {arch}: fsck clean, and the files read back on the host", flush=True)

    # A stick is never the root, and never formatted just for being blank.
    blank = blank_disk(kernel, "usb-blank.img", 16 * 1024 * 1024)
    guest = Guest(qemu_command(arch, kernel, ["-device", "qemu-xhci,id=xhci", "-drive", f"if=none,id=stick,format=raw,file={blank}",
                                              "-device", "usb-storage,bus=xhci.0,drive=stick"]))
    guest.expect("mass storage, 16 MiB", BOOT_TIMEOUT)
    guest.expect("[fs] no disk: root is in RAM", BOOT_TIMEOUT)
    guest.expect("[fs] usb0: no filesystem recognised, left alone", BOOT_TIMEOUT)
    guest.boot_to_shell()
    guest.send("poweroff\r")
    guest.wait_exit()
    with open(blank, "rb") as image:
        if any(image.read(1 << 20)):
            sys.exit("the blank stick was written to")
    os.remove(blank)

    # A relative mouse, and no MSI-X at all.
    guest = Guest(qemu_command(arch, kernel, ["-device", "nec-usb-xhci,id=xhci,msix=off", "-device", "usb-mouse,bus=xhci.0"], virtio_input=False))
    guest.expect("mouse", BOOT_TIMEOUT)
    how = guest.capture(r"\[usb\] xHCI ready: ([^\r\n]*)", BOOT_TIMEOUT)
    guest.boot_to_shell()
    guest.send("ktest input\r")
    guest.expect("input: start")
    time.sleep(0.3)
    guest.monitor("mouse_move 40 -30")
    report = guest.capture(r"input: (pointer \d+ \d+)")
    guest.expect("input: done")
    print(f"usb {arch}: usb-mouse over {how}: {report}", flush=True)
    guest.send("poweroff\r")
    guest.wait_exit()

    if arch == "arm":
        # A GICv3 has no MSI here, so this is the wired line.
        stick = make_stick(kernel, "usb-stick.img", blob[:1 << 16])
        guest = Guest(qemu_command(arch, kernel, usb_devices(stick), machine="virt,gic-version=3", virtio_input=False))
        guest.expect("xHCI ready: INTx", BOOT_TIMEOUT)
        guest.boot_to_shell()
        guest.run("echo on-gicv3 > /usb0/g.txt; sync; cat /usb0/g.txt", "on-gicv3")
        guest.send("ktest irqs\r")
        count = int(guest.capture(r"irq +\d+ xhci intx +count=(\d+)"))
        if count == 0:
            fail(guest, "no interrupts from xHCI")
        print(f"usb {arch} gicv3: {count} interrupts", flush=True)
        guest.send("poweroff\r")
        guest.wait_exit()
        fsck(stick)
    os.remove(stick)


def el_test(arch, kernel):
    """Starting above EL1, as real machines do. QEMU only: there's no hardware here."""
    if arch != "arm":
        return
    for machine, label, smp in (
        ("virt,virtualization=on", "EL2, PSCI by SMC", 4),
        ("virt,virtualization=on,gic-version=3", "EL2 and a GICv3", 4),
    ):
        guest = Guest(qemu_command(arch, kernel, machine=machine, smp=smp))
        guest.boot_to_shell()
        guest.send("ktest cpus\r")
        guest.expect(f"online={smp} ran-on={smp} ticking={smp}")
        guest.send("ktest timer\r")
        guest.expect("precise=true quiet=true")
        # Guard pages came from splitting the map's blocks, so this tests that too.
        guest.send("ktest overflow\r")
        guest.expect("stack overflow into the guard page")
        guest.send("info\r")
        guest.capture(r"(?i)(EL1)")
        guest.run("echo after-el-$((6 * 7))", "after-el-42")
        # EL0 from a machine that started at EL2, with its traps set up by `el_setup`.
        guest.run("/bin/spin 1 500 | tr a-z A-Z | tr = :; /bin/malicious --probe kernel-read; echo el0-$?", "el0-139")
        guest.send("poweroff\r")
        guest.wait_exit()
        print(f"el {arch}: {label}: SMP, timers and guard pages, and it powers off", flush=True)

    # Secure EL3 with no firmware: QEMU has no PSCI on this machine at all.
    guest = Guest(qemu_command(arch, kernel, machine="virt,secure=on", smp=1))
    guest.boot_to_shell()
    guest.run("sleep 200; echo at-el3-$((6 * 7))", "at-el3-42")
    guest.run("/bin/hello --quiet --exit 5; echo el0-at-el3-$?", "el0-at-el3-5")
    guest.send("spawn tick 1000 10000\r")
    pid = guest.capture(r"started pid (\d+): tick")
    guest.run(f"kill {pid}", f"killed pid {pid}")
    guest.send("ktest cpus\r")
    report = guest.capture(r"\[ktest\] (cpus:[^\r\n]*)")
    print(f"el {arch}: EL3 without firmware boots, with no PSCI: {report}; nothing can power it off", flush=True)
    guest.process.kill()


def user_command(arch, kernel, cpu=None, smp=4):
    command = qemu_command(arch, kernel, smp=str(smp))
    if cpu:
        command[command.index("-cpu") + 1] = cpu
    return command


def run_malicious(guest, arch, label):
    """Every probe has to report being blocked, each by name, and the machine stays up."""
    guest.send("/bin/malicious; echo malicious-status-$?\r")
    guest.mark = len(guest.log)
    guest.expect("malicious-status-0", 300)
    text = guest.log[guest.mark - 2000000 if guest.mark > 2000000 else 0:guest.mark].decode(errors="replace")
    start = text.rfind("/bin/malicious; echo")
    probes = re.findall(r"probe ([a-z0-9-]+): (blocked|NOT BLOCKED)", text[start:])
    summary = re.search(r"malicious: (\d+) of (\d+) probes blocked", text[start:])
    if not summary or summary.group(1) != summary.group(2):
        fail(guest, f"malicious: {summary.group(0) if summary else 'no summary'}")
    if len(probes) != int(summary.group(2)):
        fail(guest, f"malicious: reported {len(probes)} probes of {summary.group(2)}")
    bad = [name for name, verdict in probes if verdict != "blocked"]
    if bad or "got through" in text[start:]:
        fail(guest, f"malicious: not blocked: {bad}")
    print(f"user {arch} {label}: {len(probes)} probes blocked: " + " ".join(name for name, _ in probes), flush=True)


def user_test(arch, kernel):
    """User programs: separate binaries in /bin, at EL0 or ring 3, on their own page
    tables. One tries everything it shouldn't, and only it pays for that."""
    guest = Guest(user_command(arch, kernel))
    guest.boot_to_shell()
    protection = re.search(rb"\[user\] protection: ([^\r\n]*)", guest.log)
    if not protection:
        fail(guest, "no protection summary in the boot log")
    print(f"user {arch}: protection: {protection.group(1).decode()}", flush=True)

    # By path, with arguments and an exit status.
    guest.send("/bin/hello one 'two words'; echo status-$?\r")
    guest.expect("hello from user mode")
    guest.expect("argc 3")
    guest.expect("argv[1]=one")
    guest.expect("argv[2]=two words")
    guest.expect("status-0")
    guest.run("/bin/hello --quiet --exit 7; echo status-$?", "status-7")
    # Through the console's pipes, redirection, and from any directory.
    guest.run("/bin/cat /etc/motd | wc -l | tr 0-9 A-J | sed s/^/motd-/", "motd-")
    guest.run("echo piped-$((1+1)) | /bin/cat | tr a-z A-Z", "PIPED-2")
    guest.run("echo piped-$((1+2)) | /bin/cat | /bin/cat | /bin/cat | tr a-z A-Z", "PIPED-3")
    guest.run("seq 1 20000 | /bin/cat | wc -l | tr 2 Q", "Q0000")
    guest.run("/bin/hello redirected > /tmp/u.out; grep -c redirected /tmp/u.out | tr 1 R", "\nR")
    guest.run("/bin/cat < /etc/motd > /tmp/m.out; cmp /etc/motd /tmp/m.out; echo same-$?", "same-0")
    guest.run("cd /etc; /bin/cat motd > /tmp/n.out; cd /; cmp /etc/motd /tmp/n.out; echo rel-$?", "rel-0")
    guest.run("/bin/cat /no/such/file 2>&1 | tr a-z A-Z", "ENOENT")
    guest.run("x=$(/bin/hello captured); echo \"[${x##*=}]\"", "[captured]")
    print(f"user {arch}: argv, status, pipes and redirection work", flush=True)

    # Anything else in a file is refused, not run.
    guest.run("printf 'hello' > /tmp/not-elf; /tmp/not-elf 2>&1 | tr a-z A-Z; echo done-$?", "done-0")
    guest.run("printf '\\177ELFjunk' > /tmp/bad; /tmp/bad 2>&1 | tr a-z A-Z", "TRUNCATED")
    guest.run("/tmp/bad 2>/dev/null; echo bad-$?", "bad-126")
    guest.run("head -c 200 /bin/hello > /tmp/cut; /tmp/cut 2>/dev/null; echo cut-$?", "cut-126")
    guest.run("head -c 4097 /bin/hello > /tmp/cut2; /tmp/cut2 2>/dev/null; echo cut2-$?", "cut2-126")
    guest.run("/no/such/program; echo gone-$?", "gone-127")
    print(f"user {arch}: bad files are refused", flush=True)

    # The system calls.
    guest.send("/bin/utest 2>&1; echo utest-$?\r")
    guest.expect("utest: all ok", 120)
    guest.expect("utest-0")
    if b"FAILED" in guest.log:
        fail(guest, "utest failures")
    print(f"user {arch}: system calls work", flush=True)

    # The same address holds different data in each of two processes.
    guest.send("/bin/vmcheck; echo vm-$?\r")
    guest.expect("vmcheck: separate", 60)
    guest.expect("vm-0")
    found = re.findall(rb"vmcheck (parent|child) pid=(\d+) static=(0x[0-9a-f]+) mmap=(0x[0-9a-f]+) value=(\d+) intact=(\w+)", guest.log)
    parent = next(item for item in found if item[0] == b"parent")
    child = next(item for item in found if item[0] == b"child")
    if parent[2:4] != child[2:4] or parent[1] == child[1] or parent[4] == child[4]:
        fail(guest, f"vmcheck: {parent} {child}")
    if parent[5] != b"true" or child[5] != b"true":
        fail(guest, f"vmcheck: {parent} {child}")
    print(f"user {arch}: same address {parent[2].decode()} and {parent[3].decode()}, "
          f"values {parent[4].decode()} and {child[4].decode()}", flush=True)

    # A program that tries everything.
    run_malicious(guest, arch, "smp=4")
    guest.run("echo survived-$((6 * 7))", "survived-42")

    # A kernel-linked command at the same time, and user programs on every CPU, with
    # their floating point registers checked through preemption and migration.
    guest.send("spawn tick 1000 3000\r")
    tick = guest.capture(r"started pid (\d+): tick")
    start = len(guest.log)
    jobs = " ".join(f"/bin/spin {seed} 3000 &" for seed in range(1, 9))
    guest.send(f"{jobs} wait; echo spun-$((1+1))\r")
    guest.expect("spun-2", 120)
    spun = re.findall(rb"spin seed=(\d+) rounds=(\d+) cpus=(0x[0-9a-f]+) intact=(\w+)", guest.log[start:])
    if len(spun) != 8 or any(item[3] != b"true" for item in spun):
        fail(guest, f"spin: {spun}")
    if b"CORRUPTED" in guest.log[start:]:
        fail(guest, "a user program's registers changed")
    cpus = 0
    for item in spun:
        cpus |= int(item[2], 16)
    if bin(cpus).count("1") < 4:
        fail(guest, f"user programs only ran on CPUs {cpus:#x}")
    moved = sum(1 for item in spun if bin(int(item[2], 16)).count("1") > 1)
    print(f"user {arch}: 8 programs on CPUs {cpus:#x}, {moved} migrated, registers intact", flush=True)
    guest.run(f"kill {tick}", f"killed pid {tick}")

    # One that won't stop can be stopped.
    guest.send("/bin/spin 2 0 &\r")
    guest.capture(r"\[\d+\] (\d+)")
    time.sleep(1)
    guest.run("kill $!; wait $!; echo killed-$?", "killed-137")
    guest.send("/bin/spin 2 0\r")
    time.sleep(1)
    guest.send("\x03")
    guest.run("echo interrupted-$?", "interrupted-130")

    # Everything they used comes back, whichever way they ended.
    start = len(guest.log)
    guest.send("ktest uspawn 400\r")
    deadline = time.monotonic() + 300
    while not re.search(rb"pipes=\d+->\d+\s", guest.log[start:]) and time.monotonic() < deadline:
        guest.pump(0.2)
    guest.mark = len(guest.log)
    text = guest.log[start:].decode(errors="replace")
    match = re.search(r"rounds=(\d+) exited=(\d+) faulted=(\d+) killed=(\d+) failed=(\d+) frames-free=(\d+)->(\d+) leaked-frames=(\d+) leaked-heap=(\d+) pipes=(\d+)->(\d+)", text)
    if not match:
        fail(guest, "no uspawn report")
    rounds, exited, faulted, killed, failed, before, after, frames, heap, pipes_before, pipes_after = (int(value) for value in match.groups())
    print(f"user {arch}: {match.group(0)}", flush=True)
    if failed or frames or before != after or pipes_after > pipes_before or heap > 64 * 1024:
        fail(guest, f"leak: {match.group(0)}")
    if not (exited and faulted and killed):
        fail(guest, f"not every way of ending was tried: {match.group(0)}")

    # A surface goes back too, with nobody having looked at it.
    guest.send("ktest user-memory\r")
    before = guest.capture(r"frames-free=(\d+) ")
    guest.run("/bin/wdemo --frames 3 | tail -1 | tr a-z A-Z", "DREW 3 FRAMES")
    time.sleep(1)
    guest.send("ktest user-memory\r")
    after = guest.capture(r"frames-free=(\d+) ")
    if before != after:
        fail(guest, f"a surface leaked: {before} -> {after}")
    print(f"user {arch}: surface frames came back: {before} -> {after}", flush=True)

    # And what ran before is as it was.
    guest.run("ps | grep -c PPID", "\n1")
    guest.send("spawn tick 1000 10000\r")
    pid = guest.capture(r"started pid (\d+): tick")
    guest.run(f"kill {pid}", f"killed pid {pid}")
    guest.run("ktest cpus", "ran-on=4")
    guest.run("ktest fp", "[ktest] fp")
    guest.run("ps | grep shell | wc -l | tr 1 Y", "\nY")
    guest.send("poweroff\r")
    guest.wait_exit()

    # PAN needs a newer CPU than the one the others use.
    if arch == "arm":
        guest = Guest(user_command(arch, kernel, cpu="max"))
        guest.boot_to_shell()
        if b"PAN" not in guest.log or b"no PAN" in guest.log:
            fail(guest, "PAN isn't on with -cpu max")
        print(f"user {arch} -cpu max: PAN is on", flush=True)
        run_malicious(guest, arch, "-cpu max")
        guest.send("/bin/utest 2>&1; echo utest-$?\r")
        guest.expect("utest: all ok", 120)
        guest.send("poweroff\r")
        guest.wait_exit()

    # One CPU works too: nothing here relies on another being free.
    guest = Guest(user_command(arch, kernel, smp=1))
    guest.boot_to_shell()
    run_malicious(guest, arch, "smp=1")
    guest.send("/bin/vmcheck; echo vm-$?\r")
    guest.expect("vmcheck: separate", 60)
    guest.send("poweroff\r")
    guest.wait_exit()


def screen_has(guest, kernel, color, least):
    """Whether the screen holds at least this many pixels of an exact colour."""
    shot = work_path(kernel, "user-screen.ppm")
    guest.monitor(f"screendump {shot}")
    time.sleep(1)
    width, height, pixels = read_ppm(shot)
    wanted = bytes(color)
    count = sum(1 for index in range(0, len(pixels), 3) if pixels[index:index + 3] == wanted)
    os.remove(shot)
    return count >= least, count


def hosted_test(arch, kernel):
    """A window's app runs on a task of its own: when it fails, its window closes and
    the desktop goes on. A window can hold a user program."""
    guest = Guest(user_command(arch, kernel))
    guest.expect("[desktop] session started", BOOT_TIMEOUT)
    time.sleep(3)
    open_terminal(guest)

    # A kernel-linked window that faults, then one that panics. The user program in it
    # is stopped with its window, so nothing's left of either.
    type_keys(guest, "wdemo --test-host-fault\n")
    guest.expect("a hosted app is about to fault", 60)
    guest.expect("[fault] pid ", 30)
    guest.expect("; ended")
    guest.expect("[desktop] wdemo failed with status 139; closing its window", 30)
    guest.expect("[desktop] wdemo: closing", 30)
    print(f"hosted {arch}: a window's fault ended only that window", flush=True)
    time.sleep(2)

    type_keys(guest, "wdemo --test-host-panic\n")
    guest.expect("a hosted app is about to panic", 60)
    guest.expect("a window's panic; ended", 60)
    guest.expect("[desktop] wdemo failed with status 139; closing its window", 30)
    guest.expect("[desktop] wdemo: closing", 30)
    print(f"hosted {arch}: a window's panic ended only that window", flush=True)
    time.sleep(2)

    # A user program in a window, shown on the desktop.
    type_keys(guest, "wdemo\n")
    guest.expect("wdemo: surface ready 320x240", 60)
    time.sleep(3)
    shown, count = screen_has(guest, kernel, (0xc8, 0x3c, 0x1e), 320 * 20)
    if not shown:
        fail(guest, f"the user program's surface isn't on the screen: {count} pixels")
    print(f"hosted {arch}: a user program's surface is on the screen, {count} pixels of its colour", flush=True)

    # Input reaches it, through its own calls: it has the keyboard, having just opened.
    type_keys(guest, "q")
    guest.expect("wdemo: key 113", 30)
    print(f"hosted {arch}: a user program got a key from its window", flush=True)

    # A window that never returns is stopped, though the desktop is stuck until then.
    guest.monitor("sendkey alt-tab")
    time.sleep(1)
    type_keys(guest, "wdemo --test-host-hang\n")
    guest.expect("a hosted app is about to hang", 60)
    guest.expect("[desktop] wdemo failed with status 137; closing its window", 90)
    print(f"hosted {arch}: a window that hung was stopped", flush=True)
    time.sleep(2)
    type_keys(guest, "wdemo\n")
    guest.expect("wdemo: surface ready 320x240", 60)
    time.sleep(2)

    # The same window, told to crash. Only the program pays.
    guest.monitor("sendkey alt-tab")
    time.sleep(1)
    type_keys(guest, "wdemo --crash\n")
    guest.expect("wdemo: crashing on purpose", 60)
    guest.expect("[fault] pid ", 30)
    guest.expect("[desktop] /bin/wdemo ended with status 139; closing its window", 30)
    time.sleep(2)
    shown, count = screen_has(guest, kernel, (0xc8, 0x3c, 0x1e), 320 * 20)
    if shown:
        fail(guest, f"the window of a dead program is still there: {count} pixels")
    print(f"hosted {arch}: a user program's fault closed its window", flush=True)

    # The desktop is still there to log off from, with nothing left behind.
    guest.send("\x1c")
    guest.expect("entering the serial shell", 60)
    guest.run("echo alive-$((6 * 7))", "alive-42")
    guest.run("ps | grep -c PPID", "\n1")
    guest.send("poweroff\r")
    guest.wait_exit()


def input_test(arch, kernel):
    """virtio input on q35's PCI bus, with no PS/2 to fall back on."""
    if arch != "x64":
        return
    control = work_path(kernel, "qmp-input.sock")
    if os.path.exists(control):
        os.remove(control)
    args = ["-device", "virtio-tablet-pci", "-device", "virtio-keyboard-pci", "-qmp", f"unix:{control},server=on,wait=off"]
    guest = Guest(qemu_command(arch, kernel, args, machine="q35,i8042=off"))
    qmp = Qmp(control)
    guest.expect("[input] virtio-pci", BOOT_TIMEOUT)
    guest.expect("[desktop] session started", BOOT_TIMEOUT)
    for line in (b"virtio-pci pointer ready (Absolute)", b"virtio-pci keyboard ready"):
        if line not in guest.log:
            fail(guest, f"no {line!r}")
    time.sleep(2)
    open_terminal(guest)
    type_keys(guest, "echo typed-on-virtio > /virtio-typed.txt")
    guest.monitor("sendkey ret")
    time.sleep(3)
    guest.send("\x1c")
    guest.expect("entering the serial shell")
    guest.run("cat /virtio-typed.txt", "typed-on-virtio")
    guest.send("ktest input\r")
    guest.expect("input: start")
    time.sleep(0.3)
    qmp.pointer(16384, 8192)
    position = guest.capture(r"input: pointer (51\d 19\d)")
    guest.expect("input: done")
    print(f"input {arch}: a Terminal typed in and the pointer at {position}, on virtio-pci alone", flush=True)
    guest.send("poweroff\r")
    guest.wait_exit()


def efi_path(arch, kernel):
    """The loader, built next to the kernel in the same target directory."""
    target = "x86_64-unknown-uefi" if arch == "x64" else "aarch64-unknown-uefi"
    return os.path.join(os.path.dirname(os.path.abspath(kernel)), "..", "..", target, "release", "feos-efi.efi")


def make_esp(arch, kernel, name):
    """A FAT ESP: the removable-media boot path, and the kernel beside it."""
    esp = blank_disk(kernel, name, 96 * 1024 * 1024)
    subprocess.run(["mkfs.vfat", "-F", "32", esp], check=True, capture_output=True)
    boot_name = "BOOTX64.EFI" if arch == "x64" else "BOOTAA64.EFI"
    for command in (
        ["mmd", "-i", esp, "::/EFI"],
        ["mmd", "-i", esp, "::/EFI/BOOT"],
        ["mcopy", "-i", esp, efi_path(arch, kernel), f"::/EFI/BOOT/{boot_name}"],
        ["mcopy", "-i", esp, kernel, "::/feos.elf"],
    ):
        subprocess.run(command, check=True, capture_output=True)
    return esp


def efi_command(arch, kernel, esp, machine=None, display=None, smp=4, extra=()):
    """Firmware from the image, a copy of its variables, and nothing else to boot."""
    variables = work_path(kernel, "efi-vars.fd")
    if arch == "x64":
        with open("/usr/share/OVMF/OVMF_VARS_4M.fd", "rb") as source, open(variables, "wb") as out:
            out.write(source.read())
        code = "/usr/share/OVMF/OVMF_CODE_4M.fd"
        head = ["qemu-system-x86_64", "-machine", machine or "q35", "-cpu", "max"]
        display = display or ["-vga", "std"]
    else:
        with open("/usr/share/AAVMF/AAVMF_VARS.fd", "rb") as source, open(variables, "wb") as out:
            out.write(source.read())
        code = "/usr/share/AAVMF/AAVMF_CODE.fd"
        head = ["qemu-system-aarch64", "-machine", machine or "virt", "-cpu", "cortex-a57"]
        display = display or ["-device", "ramfb"]
    return head + ["-smp", str(smp), "-m", "512M", "-net", "none", "-display", "none"] + display + [
        "-audiodev", "none,id=snd0", "-device", "intel-hda", "-device", "hda-output,audiodev=snd0",
        "-serial", "mon:stdio",
        "-drive", f"if=pflash,format=raw,readonly=on,file={code}",
        "-drive", f"if=pflash,format=raw,file={variables}",
        "-drive", f"if=none,id=esp,format=raw,file={esp}",
        "-device", "virtio-blk-pci,drive=esp",
    ] + list(extra)


def write_png(path, width, height, pixels):
    """A screendump's RGB as a PNG, for looking at."""
    def chunk(kind, data):
        body = kind + data
        return len(data).to_bytes(4, "big") + body + zlib.crc32(body).to_bytes(4, "big")

    rows = b"".join(b"\0" + pixels[row * width * 3:(row + 1) * width * 3] for row in range(height))
    with open(path, "wb") as out:
        out.write(b"\x89PNG\r\n\x1a\n")
        out.write(chunk(b"IHDR", width.to_bytes(4, "big") + height.to_bytes(4, "big") + bytes([8, 2, 0, 0, 0])))
        out.write(chunk(b"IDAT", zlib.compress(rows)))
        out.write(chunk(b"IEND", b""))


def efi_boot(arch, kernel, label, machine=None, needs_acpi_note=False):
    esp = make_esp(arch, kernel, "esp.img")
    # Devices firmware has been at first, whose addresses it chose.
    extra = ["-device", "qemu-xhci,id=xhci", "-device", "usb-tablet,bus=xhci.0",
             "-netdev", "user,id=net0", "-device", "virtio-net-pci,netdev=net0"]
    guest = Guest(efi_command(arch, kernel, esp, machine=machine, extra=extra))
    guest.expect("feos-efi: loading", BOOT_TIMEOUT)
    fb = guest.capture(r"feos-efi: framebuffer (0x[0-9a-f]{16})", BOOT_TIMEOUT)
    rsdp = guest.capture(r" rsdp (0x[0-9a-f]{16})", 10)
    tree = guest.capture(r" device tree (0x[0-9a-f]{16})", 10)
    print(f"efi {arch} {label}: framebuffer {fb}, RSDP {rsdp}, device tree {tree}", flush=True)
    if int(fb, 16) == 0:
        fail(guest, "firmware gave no linear framebuffer")
    guest.expect("feos-efi: leaving boot services", BOOT_TIMEOUT)
    guest.expect("feos 0.1.0", 30)
    mode = guest.capture(r"\[gfx\] firmware framebuffer (\d+x\d+) XRGB8888", 60)
    guest.expect("[desktop] session started", BOOT_TIMEOUT)
    time.sleep(3)
    shot = work_path(kernel, f"efi-{arch}-{label}.ppm")
    guest.monitor(f"screendump {shot}")
    time.sleep(2)
    width, height, pixels = read_ppm(shot)
    if f"{width}x{height}" != mode:
        fail(guest, f"the screen is {width}x{height}, and the kernel thinks it's {mode}")
    colours = len(set(pixels[index:index + 3] for index in range(0, len(pixels) - 3, 3 * 997)))
    if colours < 4:
        fail(guest, f"only {colours} colours on the desktop: the framebuffer isn't on screen")
    write_png(shot[:-4] + ".png", width, height, pixels)
    print(f"efi {arch} {label}: desktop on the firmware's {mode} framebuffer, {colours} colours in a sample", flush=True)
    guest.send("\x1c")
    guest.expect("entering the serial shell")
    guest.run("ktest cpus", "online=4")
    guest.send("ktest mem\r")
    guest.expect("aligned=true intact=true restored=true")
    guest.send("ping -c 2 10.0.2.2\r")
    guest.expect("2 packets transmitted, 2 received")
    guest.send("ktest usb\r")
    guest.expect("usb: devices=1")
    guest.run("echo on-esp-$((6 * 7)) > /efi-note; sync; cat /efi-note", "on-esp-42")
    guest.send("poweroff\r")
    guest.wait_exit()
    fsck(esp)
    print(f"efi {arch} {label}: SMP, memory and disk fine, powered off cleanly, ESP fsck clean", flush=True)
    os.remove(esp)
    os.remove(shot)


def efi_test(arch, kernel):
    """Boots from a FAT ESP through real firmware, to the desktop on its framebuffer."""
    if not os.path.exists(efi_path(arch, kernel)):
        sys.exit(f"no loader at {efi_path(arch, kernel)}: run scripts/cargo-efi.sh")
    efi_boot(arch, kernel, "default")
    if arch == "arm":
        # Without ACPI, firmware passes the device tree on in its configuration table.
        efi_boot(arch, kernel, "dtb", machine="virt,acpi=off")


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("arm", "x64"):
        sys.exit(__doc__)
    arch, kernel = sys.argv[1:]
    only = os.environ.get("FEOS_BOOT_TEST_ONLY")
    if only in ("milestone", "partition", "format", "install", "persistence", "gpu", "transport", "smp", "irq", "ram", "fault", "sched", "gicv3", "net", "usb", "efi", "el", "input", "user", "hosted"):
        globals()[only + "_test"](arch, kernel)
        print(f"{only} boot test passed: {arch}")
        return
    persistence_test(arch, kernel)
    milestone_test(arch, kernel)
    transport_test(arch, kernel)
    partition_test(arch, kernel)
    format_test(arch, kernel)
    install_test(arch, kernel)
    gpu_test(arch, kernel)
    smp_test(arch, kernel)
    irq_test(arch, kernel)
    ram_test(arch, kernel)
    fault_test(arch, kernel)
    sched_test(arch, kernel)
    gicv3_test(arch, kernel)
    net_test(arch, kernel)
    usb_test(arch, kernel)
    efi_test(arch, kernel)
    el_test(arch, kernel)
    input_test(arch, kernel)
    user_test(arch, kernel)
    hosted_test(arch, kernel)
    print(f"storage boot test passed: {arch}")
    if os.environ.get("FEOS_BOOT_TEST_ONLY") == "storage":
        return

    guest = Guest(qemu_command(arch, kernel))

    guest.expect("[desktop] session started", BOOT_TIMEOUT)
    guest.send("\x1c")
    guest.expect("entering the serial shell")

    # The typed command echoes without the computed value, so each result can
    # only come from running it.
    guest.run("echo boot-ok-$((40 + 2))", "boot-ok-42")
    guest.run("A='x y'; B=$A; echo \"[$B]\"", "[x y]")
    guest.send("spawn tick 1000 10000\r")
    pid = guest.capture(r"started pid (\d+): tick")
    guest.run(f"kill {pid}", f"killed pid {pid}")
    guest.run("echo after-kill-$((1 + 1))", "after-kill-2")

    # Exit statuses of external commands reach the shell.
    guest.run("kill 99999 2>/dev/null && echo killed-$((1)) || echo status-$?", "status-1")
    guest.run("sleep 1 && echo slept-$((2 + 2))", "slept-4")

    # Arguments with spaces survive the trip to an external command.
    guest.run("coreutils 'no such' 2>&1 | tr a-z A-Z", "UNKNOWN COMMAND 'NO SUCH'")

    # An external command's output can be captured, piped and redirected.
    guest.run("ps | grep -c PPID", "\n1")
    guest.run("x=$(ps); echo \"[${x%% *}]$(( ${#x} > 40 ))\"", "]1")
    guest.run("ps > /tmp/ps.out; grep -c PPID /tmp/ps.out; echo file-$?", "file-0")
    guest.run("ps | grep shell | wc -l | tr 1 Y", "\nY")

    # Ctrl-C stops a foreground command.
    guest.send("sleep 100000\r")
    time.sleep(1)
    guest.send("\x03")
    guest.run("echo interrupted-$?", "interrupted-130")

    # Pipelines stream, so an endless producer ends with its reader.
    guest.run("yes | head -3 | wc -l | tr 3 Z", "\nZ")
    guest.run("seq 1 20000 | wc -l | tr 2 Q", "Q0000")
    guest.run("false | true; echo pipe-$?", "pipe-0")

    # A stage shares the filesystem with the shell that started it.
    guest.run("echo shared > /tmp/s | true; cat /tmp/s | tr a-z A-Z", "SHARED")

    # Job control.
    guest.send("sleep 100000 &\r")
    job = guest.capture(r"\[1\] (\d+)")
    guest.run("echo last-$!", f"last-{job}")
    guest.run("jobs | tr a-z A-Z", "RUNNING")
    guest.run("kill -STOP %1; jobs | tr a-z A-Z", "STOPPED")
    guest.run("bg; jobs | tr a-z A-Z", "RUNNING")
    guest.run("kill %1", f"killed pid {job}")
    guest.run("wait $!; echo waited-$?", "waited-137")
    guest.run("(sleep 10; exit 5) & wait $!; echo status-$?", "status-5")
    guest.run("sleep 20 & fg; echo fg-$?", "fg-0")
    guest.run("trap 'echo un-trapped' EXIT; (trap 'echo sub-$((1+1))' EXIT; true)", "sub-2")
    guest.run("trap - EXIT; echo clear-$((3))", "clear-3")

    # awk, including as deep as a program may nest on a real task stack.
    guest.run("seq 1 5 | awk '{ s += $1 } END { print \"sum-\" s }'", "sum-15")
    guest.run("awk 'function f(n) { return n < 1 ? 0 : 1 + f(n - 1) } BEGIN { print \"deep-\" f(30) }'", "deep-30")
    guest.run("awk 'BEGIN { print \"parens-\" " + "(" * 18 + "7" + ")" * 18 + " }'", "parens-7")
    guest.run("awk 'function f(n) { return f(n + 1) } BEGIN { f(0) }' 2>&1 | tr a-z A-Z", "NESTED TOO DEEPLY")

    # The pager and editor are terminal programs that step aside when piped.
    guest.run("echo paged-$((5 * 5)) | less | cat", "paged-25")
    guest.run("edit /tmp/x < /dev/null | cat; echo edit-$?", "edit-0")

    guest.send("poweroff\r")
    guest.wait_exit()
    print(f"boot test passed: {arch}")


if __name__ == "__main__":
    main()
