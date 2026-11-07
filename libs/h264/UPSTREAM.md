# Provenance of feos-h264

## Origin

This decoder is a vendored port of [`z264`](https://github.com/p14c31355/z264)
by p14c31355, which adapts the pure-Rust `rust_h264` 0.4.x implementation. Both
are under MIT OR Apache-2.0; see `docs/LICENSE-MIT` and `docs/LICENSE-APACHE`.

**Upstream commit: `35a7cece2653890ca910f1cf56635f6559bc97cb`** (z264, 2026-07-31, "Merge pull
request #1"), or its parent `6024001bf80c373e75550c11c525c61fe3919f51`, whose `src/` is the
same. This was established by comparing rather than recorded at the time: of the commits in
z264's history that have a decoder, those two are nearest to this repository's first
import (`f924245`), differing in 46 lines, all of them the crate's name in documentation and
edits to one NEON kernel. The next nearest differs in 202. The `rust_h264` version z264
adapted remains unknown beyond the 0.4.x its note states.

## Inherited adaptation

From the note that came with `z264`: standard-library containers and
synchronization were replaced with `alloc`/`core` equivalents, the standard error
trait was removed, and fixed-size CABAC scratch storage, shared DPB frame planes
and regression tests were added.

## Local patches in feos

Rebranded to `feos-h264` (crate name, documentation, metadata). Changes to the
decoding behaviour are these:

- **Resource limits.** `Limits`, `Decoder::with_limits`, `DecodeError::OutOfMemory`;
  every picture allocation is fallible; memory is estimated from the SPS before
  allocating. `parse_annex_b` is capped; `annex_b_units`, `annex_b_ranges`,
  `avcc_units` and `avcc_ranges` need no list.
- **Input validation.** Range checks on SPS, PPS and slice headers: parameter set
  ids, POC cycle length, reference counts, crop, QP, weights, deblock offsets,
  and the lengths of the list modification and MMCO loops.
- **Refused rather than decoded wrongly.** Anything but 8-bit 4:2:0, transform
  bypass, and MBAFF, whose field macroblock paths don't match a reference decoder.
- **Geometry.** Coded size comes from the SPS's macroblock counts instead of the
  cropped size; left and top crops are honoured. `Frame` shares the coded planes
  and describes the crop with offsets and strides.
- **Reconstruction fixes:** `mb_is_8x8dct` was never set on CAVLC paths, and
  deblocking then filtered inside 8x8 transforms; non-zero counts of 8x8 transforms
  under CAVLC; chroma DC dequantisation rounding; the default 8x8 scaling lists
  and the scan order of custom ones; "use default" scaling lists; PPS scaling list
  fall-back rule B; `second_chroma_qp_index_offset`.
- **Deblocking.** Parameters are per slice rather than from the first slice, and
  `disable_deblocking_filter_idc == 2` keeps to slice boundaries.
- **Reference handling.** The default P list sorts by PicNum, not POC; MMCO picNum
  wraps; list modification ignores long-term pictures for short-term commands;
  MMCO 5 resets frame_num and POC.
- **POC.** FrameNumOffset for types 1 and 2; IDR uses its coded LSB for type 0.
- **Output order.** `OrderedDecoder`'s depth follows the SPS (VUI or level),
  and treats MMCO 5 as a GOP boundary. Frames carry a caller tag.
- **Concealment.** A picture's missing macroblocks come from the latest reference picture,
  and frame_num and friends find a new picture when its first slice was lost.
- **Performance.** The bit reader borrows the RBSP; `more_rbsp_data` no longer
  rescans trailing zeros; working state is recycled between pictures; a continuation
  slice no longer deep-copies the picture state; no allocation per residual block.
- `Rc` became `Arc`, so decoders can move to a worker task later.
- `sps_info` exposes dimensions, frame rate, sample aspect ratio and colour from a
  full VUI parse, including HRD and bitstream restrictions.
- `tests/`: `conformance.rs` compares against FFmpeg; `malformed.rs` is a mutation
  test under a 64 MiB heap limit and a small stack.
