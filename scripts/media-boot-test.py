#!/usr/bin/env python3
"""Boots a kernel, plays sound through the real driver, and checks what came out.

    ./scripts/media-boot-test.py arm <kernel>
    ./scripts/media-boot-test.py x64 <kernel>

QEMU's WAV audio backend records everything the guest's HDA device plays. Each
file in /media/sound is the same two seconds: 440 Hz left, 660 Hz right. The test
measures the capture's frequencies, level and length for every format. It also listens
for sound stopping when a media window dies. With FEOS_MEDIA_DISK naming an image from
scripts/make-media-disk.sh, it plays files too large for memory from that disk.

FEOS_MEDIA_ONLY picks a phase: disk, window or sound.
"""

import importlib.util
import math
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


KEYS = {"|": "shift-backslash", "'": "apostrophe", "{": "shift-bracket_left", "}": "shift-bracket_right"}


def type_keys(guest, text):
    """Types into the desktop's focused window."""
    for character in text:
        guest.monitor(f"sendkey {KEYS.get(character, boot.USB_KEYS.get(character, character))}")


def loudness(capture, seconds):
    """The peak of the last `seconds` QEMU has written."""
    with open(capture, "rb") as f:
        rate = struct.unpack("<I", f.read(28)[24:])[0]
        size = os.path.getsize(capture)
        want = seconds * rate * 4
        f.seek(max(44, size - want) // 4 * 4)
        pcm = f.read()
    samples = struct.unpack(f"<{len(pcm) // 2}h", pcm[: len(pcm) // 2 * 2])
    return max((abs(s) for s in samples), default=0)


def window_test(arch, kernel):
    """Whatever ends a media window, or the task that feeds it, its sound stops, and
    nothing of it stays: the mixer's own task only leaves once no stream is open."""
    capture = boot.work_path(kernel, "capture.wav")
    command = [
        f"wav,id=snd0,path={capture}" if word == "none,id=snd0" else word
        for word in boot.qemu_command(arch, kernel)
    ]
    guest = boot.Guest(command)
    guest.expect("[desktop] session started", boot.BOOT_TIMEOUT)
    time.sleep(3)
    boot.open_terminal(guest)

    def sounding(label):
        time.sleep(1.5)
        peak = loudness(capture, 1)
        if peak < 1000:
            boot.fail(guest, f"{label}: no sound to begin with, peak {peak}")
        print(f"window {arch}: {label}: playing, peak {peak}", flush=True)

    def stops(label):
        # A device with nothing to play stops, and the capture stops growing with it,
        # so this looks only at what came after a moment's grace.
        time.sleep(1)
        before = os.path.getsize(capture)
        time.sleep(1.5)
        with open(capture, "rb") as f:
            f.seek(before // 4 * 4)
            pcm = f.read()
        samples = struct.unpack(f"<{len(pcm) // 2}h", pcm[: len(pcm) // 2 * 2])
        peak = max((abs(s) for s in samples), default=0)
        if peak > 16:
            boot.fail(guest, f"{label}: still sounding, peak {peak}")
        print(f"window {arch}: {label}: silent, peak {peak} in {len(pcm)} bytes", flush=True)

    for app, target in (("music", "/media/sound/tone.flac"), ("player", "/media/av-sample.mp4")):
        # Its window's task killed: it closes, and takes its sound along.
        type_keys(guest, f"{app} {target}\n")
        time.sleep(4)
        sounding(f"{app}, for a kill")
        guest.monitor("sendkey alt-tab")
        time.sleep(1)
        type_keys(guest, f"kill $(ps|awk '/ {app}$/{{print $1}}')\n")
        guest.expect(f"[desktop] {app} failed with status 137; closing its window", 60)
        stops(f"{app}, window task killed")

        # Its window's task crashed, in the middle of a frame.
        type_keys(guest, f"{app} --test-host-fault {target}\n")
        guest.expect("[media] a window is about to fault", 90)
        guest.expect("[fault] pid ", 30)
        guest.expect(f"[desktop] {app} failed with status 139; closing its window", 30)
        stops(f"{app}, window task crashed")

        # The task feeding the stream killed: the mixer closes it.
        type_keys(guest, f"{app} {target}\n")
        time.sleep(4)
        sounding(f"{app}, for its audio task")
        guest.monitor("sendkey alt-tab")
        time.sleep(1)
        type_keys(guest, "kill $(ps|awk '/audio-decoder$/{print $1}')\n")
        stops(f"{app}, audio task killed")
        # Clean up the window itself, which said so and is otherwise fine.
        type_keys(guest, f"kill $(ps|awk '/ {app}$/{{print $1}}')\n")
        guest.expect(f"[desktop] {app} failed with status 137; closing its window", 60)
        time.sleep(1)

    # A playlist saved from the menu's shortcut, and loaded back by opening the file.
    type_keys(guest, "player /media/av-sample.mp4\n")
    time.sleep(5)
    guest.monitor("sendkey ctrl-s")
    saved = guest.capture(r"\[player\] saved \d+ entries to (\S+)", 30)
    print(f"window {arch}: playlist saved to {saved}", flush=True)
    guest.monitor("sendkey q")
    time.sleep(3)
    type_keys(guest, f"player {saved}\n")
    loaded = guest.capture(r"\[player\] loaded (\d+ entries, 0 missing)", 30)
    print(f"window {arch}: playlist loaded back: {loaded}", flush=True)
    time.sleep(4)
    sounding("a playlist's first entry")
    guest.monitor("sendkey q")
    time.sleep(2)

    # Off the desktop, and see what's still running: the mixer's task stays for three
    # seconds at most after the last stream closes.
    guest.send("\x1c")
    guest.expect("entering the serial shell", 60)
    time.sleep(4)
    guest.run("ps | grep -c -E 'audio-decoder|mixer|video-decoder'; echo counted-$((1+1))", "counted-2")
    tail = guest.log[guest.mark - 200 : guest.mark].decode(errors="replace")
    if "\n0\r" not in tail and "\n0\n" not in tail:
        boot.fail(guest, f"something is left: {tail!r}")
    print(f"window {arch}: no audio task, mixer or decoder left", flush=True)
    guest.send("poweroff\r")
    guest.wait_exit()
    os.remove(capture)


# Goertzel, because a zero crossing count wants a cleaner signal than these files have.
def tone_at(capture, seconds, candidates):
    """Which of `candidates` is loudest in the last `seconds` of the capture."""
    with open(capture, "rb") as f:
        rate = struct.unpack("<I", f.read(28)[24:])[0]
        size = os.path.getsize(capture)
        f.seek(max(44, size - int(seconds * rate * 4)) // 4 * 4)
        pcm = f.read()
    left = struct.unpack(f"<{len(pcm) // 2}h", pcm[: len(pcm) // 2 * 2])[0::2]
    best, best_power = 0, 0.0
    for frequency in candidates:
        factor = 2 * math.cos(2 * math.pi * frequency / rate)
        a, b = 0.0, 0.0
        for sample in left:
            a, b = sample + factor * a - b, a
        power = a * a + b * b - factor * a * b
        if power > best_power:
            best, best_power = frequency, power
    return best


def silent_share(capture, seconds):
    """How much of the last while was silence: where an emulated CPU can't keep up."""
    with open(capture, "rb") as f:
        f.seek(max(44, os.path.getsize(capture) - seconds * 192000) // 4 * 4)
        pcm = f.read()
    samples = struct.unpack(f"<{len(pcm) // 2}h", pcm[: len(pcm) // 2 * 2])
    return sum(1 for s in samples if s == 0) * 100 // max(len(samples), 1)


def big_tone(position_s):
    """What frequency `scripts/make-media-disk.sh` puts at a time in its files."""
    return 300 + 100 * (position_s // 60)


TONES = [300 + 100 * minute for minute in range(16)]


def disk_test(arch, kernel, image):
    """Files too large for memory play from a virtio disk, and seek. The tone in them
    changes each minute, so what's heard says where playback is."""
    capture = boot.work_path(kernel, "capture.wav")
    args = [
        word + ",snapshot=on" if word.startswith("if=none") else word
        for word in boot.disk_device(arch, image)
    ]
    command = [
        f"wav,id=snd0,path={capture}" if word == "none,id=snd0" else word
        for word in boot.qemu_command(arch, kernel, args)
    ]
    guest = boot.Guest(command)
    guest.expect("[fs] virtio0: mounted at /", boot.BOOT_TIMEOUT)
    guest.expect("[desktop] session started", boot.BOOT_TIMEOUT)
    time.sleep(3)
    boot.open_terminal(guest)

    def expect_tone(label, seconds_in):
        time.sleep(1)
        heard = tone_at(capture, 0.5, TONES)
        want = big_tone(seconds_in)
        if heard != want:
            boot.fail(guest, f"{label}: heard {heard} Hz, wanted {want} Hz, {seconds_in} s in")
        print(f"disk {arch}: {label}: {heard} Hz, right for {seconds_in} s in", flush=True)

    # The Music window, from the file's start and after seeking forward.
    type_keys(guest, "music /big.flac\n")
    time.sleep(6)
    expect_tone("Music, big.flac", 0)
    for _ in range(26):
        guest.monitor("sendkey right")
    time.sleep(3)
    expect_tone("Music, big.flac after seeking", 130)
    type_keys(guest, "s")
    guest.monitor("sendkey alt-f4")
    time.sleep(2)

    # The Media Player, a picture and its sound, from a file twice the size of the heap.
    type_keys(guest, "player /big.mov\n")
    time.sleep(12)
    expect_tone("Player, big.mov", 0)
    # 30 s a press.
    for _ in range(7):
        guest.monitor("sendkey pgdn")
    time.sleep(6)
    expect_tone("Player, big.mov after seeking", 215)
    # Faster, keeping the pitch: 1.25 times 600 Hz would be 750 Hz.
    guest.monitor("sendkey shift-equal")
    time.sleep(4)
    heard = tone_at(capture, 0.5, [600, 750, 700, 800])
    if heard != 600:
        boot.fail(guest, f"at 1.25x, heard {heard} Hz instead of 600 Hz")
    print(f"disk {arch}: Player at 1.25x: still {heard} Hz", flush=True)
    guest.monitor("sendkey alt-f4")
    time.sleep(3)

    # VP8 and Opus: a picture decoded, and the right sound with it, after a seek.
    type_keys(guest, "player /big.webm\n")
    guest.expect("[player] opened vp8 160x120", 120)
    guest.expect("[player] first frame", 60)
    # The sound's task walks the same file for its own index, and the capture only grows
    # once there's sound.
    opened = os.path.getsize(capture)
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        guest.pump(1)
        if os.path.getsize(capture) >= opened + 200_000 and tone_at(capture, 0.5, TONES) != 0:
            break
    started = 120 - (deadline - time.monotonic())
    print(f"disk {arch}: big.webm's sound began {started:.0f} s after its picture", flush=True)
    time.sleep(3)
    print(f"disk {arch}: big.webm, last 3 s: {silent_share(capture, 3)}% silent", flush=True)
    # In pieces, as above, and what's in the capture before them is the last file's.
    deadline = time.monotonic() + 90
    while tone_at(capture, 1, TONES) != 300 and time.monotonic() < deadline:
        guest.pump(2)
    heard = tone_at(capture, 1, TONES)
    if heard != 300:
        boot.fail(guest, f"big.webm: heard {heard} Hz, wanted 300 Hz")
    print(f"disk {arch}: Player, big.webm, VP8 and Opus: {heard} Hz", flush=True)
    for _ in range(3):
        guest.monitor("sendkey pgdn")
    # Sound waits for the picture to get there, from the key frame before it.
    begun = time.monotonic()
    time.sleep(2)
    while tone_at(capture, 0.5, TONES) == 0 and time.monotonic() - begun < 90:
        guest.pump(1)
    print(f"disk {arch}: big.webm's sound was back in {time.monotonic() - begun:.0f} s", flush=True)
    time.sleep(6)
    print(f"disk {arch}: big.webm, last 5 s: {silent_share(capture, 5)}% silent", flush=True)
    heard = tone_at(capture, 3, TONES)
    if heard != 400:
        boot.fail(guest, f"after seeking big.webm: heard {heard} Hz, wanted 400 Hz")
    print(f"disk {arch}: Player, big.webm after seeking: {heard} Hz", flush=True)
    guest.monitor("sendkey alt-f4")
    time.sleep(3)

    # Off the desktop for the command line, and the desktop's own report.
    guest.send("\x1c")
    longest = int(guest.capture(r"longest frame (\d+) ms", 60))
    print(f"disk {arch}: longest desktop frame in all of that: {longest} ms", flush=True)
    guest.expect("entering the serial shell", 60)

    # Every format this handles from a file, each a long way in, and how much memory
    # that took. /proc-style numbers come from `free`.
    guest.run("ls -l /big.* | awk '{print $5, $NF}'; echo listed-$((1+1))", "listed-2")
    for name, at in (("big.flac", 200), ("big.mp3", 310), ("big.ogg", 250), ("big.aac", 370), ("big.wav", 420), ("big.opus", 500)):
        guest.send(f"play --at {at} /{name} &\r")
        pid = guest.capture(r"\[\d+\] (\d+)\s", 30)
        guest.expect(f"playing /{name}", 60)
        time.sleep(5)
        # Opus can't keep up under an emulated CPU: it gets there, in pieces.
        deadline = time.monotonic() + (90 if name == "big.opus" else 0)
        heard = tone_at(capture, 3, TONES)
        while heard != big_tone(at + 3) and time.monotonic() < deadline:
            guest.pump(2)
            heard = tone_at(capture, 3, TONES)
        if heard != big_tone(at + 3):
            boot.fail(guest, f"play --at {at} {name}: heard {heard} Hz")
        print(f"disk {arch}: play --at {at} {name}: {heard} Hz", flush=True)
        print(f"disk {arch}: {name}, last 3 s: {silent_share(capture, 3)}% silent", flush=True)
        guest.send("free | grep -i mem\r")
        time.sleep(0.5)
        guest.run(f"kill {pid}; wait; echo stopped-$((3+4))", "stopped-7")
        time.sleep(1)
    # An MPEG-1 program stream: its sound is Layer II, in packets that split frames.
    guest.send("play /av.mpg\r")
    guest.expect("playing /av.mpg", 30)
    time.sleep(1.5)
    heard = tone_at(capture, 0.5, [330, 440, 550, 660])
    if heard != 440:
        boot.fail(guest, f"the program stream sounded like {heard} Hz")
    print(f"disk {arch}: program stream's Layer II: {heard} Hz", flush=True)
    guest.expect("finished /av.mpg", 60)
    tail = guest.log[-1500:].decode(errors="replace")
    print(f"disk {arch}: serial tail:\n{tail}", flush=True)
    guest.send("poweroff\r")
    guest.wait_exit()
    os.remove(capture)
    # A frame that long means the desktop waited for the disk.
    if longest > 5000:
        sys.exit(f"the desktop stalled for {longest} ms")


def record_test(arch, kernel):
    """Real samples into the guest: QEMU's ALSA backend reads ALSA's `file` plugin, which
    gives it a file of a generated tone as what its input device heard."""
    # 440 Hz left, 660 Hz right, at the device's format, for longer than the test.
    rate = 48000
    tone = boot.work_path(kernel, "tone.raw")
    with open(tone, "wb") as f:
        for n in range(rate * 8):
            f.write(struct.pack(
                "<hh",
                int(9000 * math.sin(2 * math.pi * 440 * n / rate)),
                int(9000 * math.sin(2 * math.pi * 660 * n / rate)),
            ))
    config = boot.work_path(kernel, "asound.conf")
    with open(config, "w") as f:
        f.write(
            'pcm.feostone { type file slave.pcm "null" file "/dev/null" infile "%s" format "raw" }\n'
            'pcm.null { type null }\n' % tone
        )
    os.environ["ALSA_CONFIG_PATH"] = config
    command = []
    for word in boot.qemu_command(arch, kernel):
        if word == "none,id=snd0":
            word = "alsa,id=snd0,in.dev=feostone,out.dev=null,in.frequency=48000,in.channels=2,in.format=s16,in.try-poll=off,out.try-poll=off"
        elif word == "hda-output,audiodev=snd0":
            word = "hda-duplex,audiodev=snd0"
        command.append(word)
    guest = boot.Guest(command)
    guest.boot_to_shell()
    guest.send("rec 3 /tmp/rec.wav\r")
    guest.expect("recording 3 s", 30)
    line = guest.capture(r"(recorded \d+ frames of \d+: peak \d+ \d+, \d+ and \d+ Hz)", 60)
    print(f"record {arch}: {line}", flush=True)
    guest.run("ls -l /tmp/rec.wav | awk '{print \"size\", $5}'; echo done-$((2+3))", "size 576044")
    # A recorder that's killed lets the next one in, through the mixer's task.
    guest.send("rec 30 /tmp/gone.wav &\r")
    pid = guest.capture(r"\[\d+\] (\d+)\s", 30)
    guest.expect("recording 30 s", 30)
    guest.run(f"kill {pid}; wait; echo stopped-$((3+4))", "stopped-7")
    time.sleep(2)
    guest.send("rec 1 /tmp/next.wav\r")
    next_line = guest.capture(r"(recorded \d+ frames of \d+)", 30)
    print(f"record {arch}: after a killed recorder: {next_line}", flush=True)
    guest.send("poweroff\r")
    guest.wait_exit()
    os.remove(tone)
    os.remove(config)
    words = line.split()
    left, right = int(words[-4]), int(words[-2])
    peak = int(words[6]), int(words[7].rstrip(","))
    if abs(left - 440) > 8 or abs(right - 660) > 8 or min(peak) < 2000:
        sys.exit(f"the recording isn't the tone: {line}")
    print(f"record {arch}: real samples reached the guest", flush=True)


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("arm", "x64"):
        sys.exit(__doc__)
    arch, kernel = sys.argv[1:]
    only = os.environ.get("FEOS_MEDIA_ONLY")
    if only in (None, "record"):
        record_test(arch, kernel)
    if only == "record":
        return
    # Made by scripts/make-media-disk.sh, too large to keep anywhere near the repository.
    image = os.environ.get("FEOS_MEDIA_DISK")
    if only == "disk" or (only is None and image):
        disk_test(arch, kernel, image)
    elif only is None:
        print("media boot test: no FEOS_MEDIA_DISK, so large files weren't tried")
    if only == "disk":
        return
    if only in (None, "window"):
        window_test(arch, kernel)
    if only == "window":
        return
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
