# Provenance of the MP3 decoder

## Origin

A vendored copy of [`mp3dec`](https://github.com/raphlinus/mp3dec) by Raph Levien, itself a
port of [minimp3](https://github.com/lieff/minimp3), under `Apache-2.0 OR MIT`. minimp3 is
under CC0.

**Upstream commit: `cdaaedc5ddff3304b340ef2f16914cc948f32edd`** (2026-10-05, "Exclude compare.py
from published crate"), crate version 0.1.0, or its parent `fb3b2ea`, whose code is the same.
This was established by comparing rather than recorded at the time: those two are the nearest
of its commits to this repository's first copy (`e8c7446`), at 85 differing lines.

## Local patches

- Layers I and II, ported from minimp3's own code the same way, as upstream leaves them out
  (`L12ScaleInfo` and the `l12_*` functions, and the branch that calls them).
- The `dump` feature is gone, with its `std` use, and the `no_std` and `forbid(unsafe_code)`
  attributes now belong to the crate as a whole.
- Formatting, and `#[expect(unused)]` attributes removed.
- One row of a scalefactor band table differs from upstream's: one fewer `36`. Which is right
  was not looked into. Nothing that's been tested reaches a difference.
