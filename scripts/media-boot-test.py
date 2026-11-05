#!/usr/bin/env python3
"""Boots a kernel, plays sound through the real driver, and checks what came out.

    ./scripts/media-boot-test.py arm <kernel>
    ./scripts/media-boot-test.py x64 <kernel>

QEMU's WAV audio backend records everything the guest's HDA device plays. Each
file in /media/sound is the same two seconds: 440 Hz left, 660 Hz right. The test
measures the capture's frequencies, level and length for every format.
"""

import importlib.util
import os
import struct
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("boot_test", os.path.join(here, "boot-test.py"))
boot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(boot)

FILES = os.environ.get("FEOS_MEDIA_FILES", "").split() or ["tone.wav", "tone.aiff", "tone.au", "tone.mp3", "tone.flac", "tone.ogg", "tone.aac"]


def analyse(pcm, RATE):
    """Returns seconds of signal and each channel's frequency and peak."""
    samples = struct.unpack(f"<{len(pcm) // 2}h", pcm[: len(pcm) // 2 * 2])
    left, right = samples[0::2], samples[1::2]
    loud = [i for i, s in enumerate(left) if abs(s) > 500]
    if not loud:
        return 0.0, (0, 0), (0, 0)
    first, last = loud[0], loud[-1]

    def freq(channel):
        window = channel[first + RATE // 10 : last - RATE // 10]
        crossings = sum(1 for a, b in zip(window, window[1:]) if a <= 0 < b)
        seconds = max(len(window), 1) / RATE
        return round(crossings / seconds), max(abs(s) for s in window) if window else 0

    return (last - first) / RATE, freq(left), freq(right)


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("arm", "x64"):
        sys.exit(__doc__)
    arch, kernel = sys.argv[1:]
    # Beside the kernel, because /tmp may be RAM, and this grows as QEMU runs.
    capture = boot.work_path(kernel, "capture.wav")
    command = [
        f"wav,id=snd0,path={capture}" if word == "none,id=snd0" else word
        for word in boot.qemu_command(arch, kernel)
    ]
    guest = boot.Guest(command)
    guest.expect("[desktop] session started", boot.BOOT_TIMEOUT)
    guest.send("\x1c")
    guest.expect("entering the serial shell")

    results = []
    for name in FILES:
        before = os.path.getsize(capture)
        start = time.monotonic()
        guest.send(f"play /media/sound/{name}\r")
        guest.expect(f"finished /media/sound/{name}", 180)
        took = time.monotonic() - start
        # The device stops once drained; let the file catch up.
        time.sleep(0.5)
        guest.pump(0.1)
        with open(capture, "rb") as f:
            f.seek(max(before, 44))
            pcm = f.read()
        # QEMU resamples to its backend's rate, whatever the guest plays at.
        with open(capture, "rb") as f:
            rate = struct.unpack("<I", f.read(28)[24:])[0]
        seconds, left, right = analyse(pcm, rate)
        ok = (
            1.9 <= seconds <= 2.1
            and abs(left[0] - 440) <= 8
            and abs(right[0] - 660) <= 8
            and left[1] > 2000
            and right[1] > 2000
        )
        results.append(ok)
        print(
            f"{name:10} {'ok ' if ok else 'BAD'} heard {seconds:.2f} s in {took:.2f} s, "
            f"left {left[0]} Hz peak {left[1]}, right {right[0]} Hz peak {right[1]}"
        )

    # Real time on the wire: a decoder too slow for this machine shows up here as
    # gaps, which the length check above catches.
    guest.send("poweroff\r")
    guest.wait_exit()
    if not all(results):
        sys.exit("some files did not play correctly")
    print(f"media boot test passed: {arch}")


if __name__ == "__main__":
    main()
