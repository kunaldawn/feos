#!/usr/bin/env python3
"""Synthesizes FeOS's demo tracks, so FeAmp and its visualisations have music to
play that nobody else owns. Everything here is generated from scratch: no
samples, no loops from elsewhere.

    python3 scripts/make-demo-music.py assets/music

Needs numpy, and ffmpeg (with libmp3lame) on PATH for the MP3 encoding.
"""

import os
import subprocess
import sys
import tempfile
import wave

import numpy as np

RATE = 44100
NOTE = {name: index for index, name in enumerate(
    ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"])}


def hz(name, octave):
    """Equal temperament, A4 = 440 Hz."""
    return 440.0 * 2 ** ((NOTE[name] - 9) / 12 + (octave - 4))


class Track:
    def __init__(self, seconds, seed):
        self.left = np.zeros(int(seconds * RATE) + RATE)
        self.right = np.zeros_like(self.left)
        self.rng = np.random.default_rng(seed)

    def add(self, at, sound, pan=0.0, gain=1.0):
        start = int(at * RATE)
        end = min(start + len(sound), len(self.left))
        if start >= end:
            return
        sound = sound[: end - start] * gain
        self.left[start:end] += sound * np.sqrt(0.5 * (1 - pan))
        self.right[start:end] += sound * np.sqrt(0.5 * (1 + pan))

    def master(self, duck=None):
        if duck is not None:
            self.left *= duck[: len(self.left)]
            self.right *= duck[: len(self.right)]
        peak = max(np.abs(self.left).max(), np.abs(self.right).max(), 1e-9)
        left = np.tanh(1.6 * self.left / peak) / np.tanh(1.6)
        right = np.tanh(1.6 * self.right / peak) / np.tanh(1.6)
        return np.stack([left, right], axis=1) * 0.89


def time_axis(seconds):
    return np.arange(int(seconds * RATE)) / RATE


def envelope(seconds, attack, release, sustain_level=1.0):
    t = time_axis(seconds)
    env = np.minimum(1.0, t / max(attack, 1e-4))
    tail = np.clip((seconds - t) / max(release, 1e-4), 0, 1)
    return env * tail * sustain_level


def lowpass(signal, cutoff):
    """One-pole lowpass; `cutoff` is a number or a per-sample array, in Hz."""
    cutoff = np.broadcast_to(np.asarray(cutoff, dtype=float), signal.shape)
    alpha = 1 - np.exp(-2 * np.pi * cutoff / RATE)
    out = np.empty_like(signal)
    state = 0.0
    for index in range(len(signal)):
        state += alpha[index] * (signal[index] - state)
        out[index] = state
    return out


def highpass(signal, cutoff):
    return signal - lowpass(signal, cutoff)


def saw(freq, seconds, detune=0.0):
    t = time_axis(seconds)
    phase = (t * freq * (1 + detune)) % 1.0
    return 2 * phase - 1


def pulse(freq, seconds, width=0.5):
    t = time_axis(seconds)
    return np.where((t * freq) % 1.0 < width, 1.0, -1.0)


def kick(seconds=0.45):
    t = time_axis(seconds)
    freq = 45 + 110 * np.exp(-t * 28)
    phase = 2 * np.pi * np.cumsum(freq) / RATE
    body = np.sin(phase) * np.exp(-t * 7)
    click = np.exp(-t * 400) * 0.6
    return body + click


def snare(rng, seconds=0.25):
    t = time_axis(seconds)
    noise = highpass(rng.standard_normal(len(t)), 1200) * np.exp(-t * 18)
    tone = np.sin(2 * np.pi * 185 * t) * np.exp(-t * 30)
    return noise * 0.7 + tone * 0.5


def hat(rng, seconds=0.06, open_hat=False):
    length = 0.28 if open_hat else seconds
    t = time_axis(length)
    noise = highpass(rng.standard_normal(len(t)), 7000)
    return noise * np.exp(-t * (9 if open_hat else 70))


def pad(freqs, seconds):
    voice = sum(saw(f, seconds, d) for f in freqs for d in (-0.004, 0.0, 0.005))
    sweep = 900 + 700 * np.sin(np.linspace(0, np.pi, int(seconds * RATE)))
    return lowpass(voice / (3 * len(freqs)), sweep) * envelope(seconds, 0.4, 0.6)


def pluck(freq, seconds, bright=3000):
    tone = pulse(freq, seconds, 0.3) * 0.6 + saw(freq * 2, seconds) * 0.4
    t = time_axis(seconds)
    return lowpass(tone, bright * np.exp(-t * 6) + 200) * np.exp(-t * 5)


def fm_keys(freq, seconds):
    t = time_axis(seconds)
    modulator = np.sin(2 * np.pi * freq * 2 * t) * 1.8 * np.exp(-t * 3)
    return np.sin(2 * np.pi * freq * t + modulator) * np.exp(-t * 1.6)


def echo(track, delay, feedback, mix):
    """Ping-pong delay over the whole mix."""
    step = int(delay * RATE)
    left, right = track.left.copy(), track.right.copy()
    wet_l = np.zeros_like(left)
    wet_r = np.zeros_like(right)
    feed_l, feed_r = left * mix, right * mix
    for repeat in range(6):
        shifted_l = np.zeros_like(left)
        shifted_r = np.zeros_like(right)
        shift = step * (repeat + 1)
        if shift >= len(left):
            break
        # Alternate sides.
        source_l, source_r = (feed_r, feed_l) if repeat % 2 == 0 else (feed_l, feed_r)
        shifted_l[shift:] = source_l[:-shift]
        shifted_r[shift:] = source_r[:-shift]
        wet_l += shifted_l * feedback ** repeat
        wet_r += shifted_r * feedback ** repeat
    track.left += wet_l
    track.right += wet_r


def sidechain(length, beat, hits):
    """A gain curve that ducks after each kick."""
    curve = np.ones(length)
    t = time_axis(beat)
    shape = 1 - 0.55 * np.exp(-t * 9)
    for at in hits:
        start = int(at * RATE)
        end = min(start + len(shape), length)
        if start < end:
            curve[start:end] = np.minimum(curve[start:end], shape[: end - start])
    return curve


def iron_pulse():
    """Electro, 124 BPM, A minor: Am F C G."""
    bpm = 124
    beat = 60 / bpm
    bars = 32
    seconds = bars * 4 * beat
    track = Track(seconds, 7)
    chords = [
        [hz("A", 3), hz("C", 4), hz("E", 4)],
        [hz("F", 3), hz("A", 3), hz("C", 4)],
        [hz("C", 3), hz("E", 3), hz("G", 3)],
        [hz("G", 3), hz("B", 3), hz("D", 4)],
    ]
    roots = [hz("A", 1), hz("F", 1), hz("C", 2), hz("G", 1)]
    kick_sound = kick()
    snare_sound = snare(track.rng)
    kicks = []
    arp = [0, 1, 2, 1, 0, 2, 1, 2]
    for bar in range(bars):
        start = bar * 4 * beat
        chord = chords[bar % 4]
        breakdown = 20 <= bar < 24
        drums = 4 <= bar < 30 and not breakdown
        track.add(start, pad(chord, 4 * beat), gain=0.9)
        for step in range(16):
            at = start + step * beat / 4
            if step % 4 == 0 and drums:
                track.add(at, kick_sound, gain=1.1)
                kicks.append(at)
            if step % 8 == 4 and bar >= 8 and drums:
                track.add(at, snare_sound, gain=0.55)
            if step % 2 == 1 and bar >= 2 and not (breakdown and bar < 22):
                track.add(at, hat(track.rng, open_hat=step % 8 == 7), pan=0.3, gain=0.18)
            if step % 2 == 1 and drums:
                freq = roots[bar % 4] * (2 if step % 4 == 3 else 1)
                bass = lowpass(saw(freq, beat / 2), 600) * envelope(beat / 2, 0.005, 0.05)
                track.add(at, bass, gain=0.85)
            if bar >= 12:
                note = chord[arp[step % 8]] * (2 if (step // 8) % 2 else 1) * 2
                track.add(at, pluck(note, beat / 2), pan=0.4 * np.sin(step), gain=0.22)
    echo(track, beat * 0.75, 0.45, 0.35)
    duck = sidechain(len(track.left), beat, kicks)
    return track.master(duck)


def ferrous_dreams():
    """Downtempo, 92 BPM, D minor: Dm Bb F C, with a sung-sounding lead."""
    bpm = 92
    beat = 60 / bpm
    bars = 24
    seconds = bars * 4 * beat
    track = Track(seconds, 11)
    chords = [
        [hz("D", 3), hz("F", 3), hz("A", 3)],
        [hz("A#", 2), hz("D", 3), hz("F", 3)],
        [hz("F", 2), hz("A", 2), hz("C", 3)],
        [hz("C", 3), hz("E", 3), hz("G", 3)],
    ]
    melody = [("A", 4, 1.5), ("F", 4, 0.5), ("G", 4, 1), ("A", 4, 1),
              ("D", 5, 2), ("C", 5, 1), ("A#", 4, 1),
              ("A", 4, 1.5), ("G", 4, 0.5), ("F", 4, 1), ("E", 4, 1),
              ("C", 4, 2), ("D", 4, 2)]
    kick_sound = kick(0.6)
    snare_sound = snare(track.rng, 0.35)
    kicks = []
    for bar in range(bars):
        start = bar * 4 * beat
        chord = chords[bar % 4]
        for index, freq in enumerate(chord):
            track.add(start + index * 0.02, fm_keys(freq, 4 * beat), pan=(index - 1) * 0.5, gain=0.35)
            track.add(start + 2 * beat, fm_keys(freq * 2, 2 * beat), pan=(1 - index) * 0.5, gain=0.12)
        if 2 <= bar < 22:
            for at_beat in (0, 1.75, 2.5):
                at = start + at_beat * beat
                track.add(at, kick_sound, gain=1.0)
                kicks.append(at)
            for at_beat in (1, 3):
                track.add(start + at_beat * beat, snare_sound, gain=0.5)
            for eighth in range(8):
                track.add(start + eighth * beat / 2, hat(track.rng), pan=-0.3, gain=0.12)
            bass = lowpass(saw(chord[0] / 2, 4 * beat) + pulse(chord[0] / 4, 4 * beat) * 0.5, 400)
            track.add(start, bass * envelope(4 * beat, 0.01, 0.2), gain=0.6)
    # The lead, twice through after the intro.
    at = 4 * 4 * beat
    for _ in range(2):
        for name, octave, beats in melody:
            length = beats * beat
            t = time_axis(length)
            vibrato = 1 + 0.006 * np.sin(2 * np.pi * 5.5 * t) * np.minimum(1, t * 2)
            phase = 2 * np.pi * np.cumsum(hz(name, octave) * vibrato) / RATE
            voice = np.sin(phase) + 0.3 * np.sin(2 * phase) + 0.12 * np.sin(3 * phase)
            track.add(at, voice * envelope(length, 0.04, 0.15), pan=0.1, gain=0.33)
            at += length
        at += 4 * 4 * beat - sum(b for *_, b in melody) * beat
    echo(track, beat * 1.5, 0.5, 0.3)
    duck = sidechain(len(track.left), beat, kicks)
    return track.master(duck)


TRACKS = [
    ("01 - Iron Pulse.mp3", "Iron Pulse", iron_pulse),
    ("02 - Ferrous Dreams.mp3", "Ferrous Dreams", ferrous_dreams),
]


def encode(samples, path, title, number):
    pcm = (np.clip(samples, -1, 1) * 32767).astype("<i2")
    with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as handle:
        temporary = handle.name
    try:
        with wave.open(temporary, "wb") as out:
            out.setnchannels(2)
            out.setsampwidth(2)
            out.setframerate(RATE)
            out.writeframes(pcm.tobytes())
        subprocess.run([
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", temporary,
            "-codec:a", "libmp3lame", "-b:a", "128k", "-id3v2_version", "3",
            "-metadata", f"title={title}", "-metadata", "artist=The FeOS Band",
            "-metadata", "album=Nostalgia in Rust", "-metadata", f"track={number}",
            "-metadata", "genre=Electronic", path,
        ], check=True)
    finally:
        os.remove(temporary)


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "assets/music"
    os.makedirs(out, exist_ok=True)
    for number, (name, title, make) in enumerate(TRACKS, 1):
        print(f"synthesizing {title}...", flush=True)
        encode(make(), os.path.join(out, name), title, number)
        print(f"  {os.path.join(out, name)}: {os.path.getsize(os.path.join(out, name)) // 1024} KiB")


if __name__ == "__main__":
    main()
