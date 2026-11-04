# Licensing

Most feos crates are licensed under `MIT OR Apache-2.0` (`LICENSE-MIT`,
`LICENSE-APACHE`). Each crate's `Cargo.toml` states its licence.

## GPL parts

| Item | Licence | Why |
|---|---|---|
| `libs/dosbox` (`feos-dosbox`) | GPL-2.0-or-later | Contains code derived from rust-dos and an LGPL OPL3 core |
| `apps/dosbox` (`feos-dosbox-app`) | GPL-2.0-or-later | Wrapper built directly on `feos-dosbox` |
| `kernel` (`feos-kernel`) and the linked `feos` binary | GPL-2.0-or-later | The binary statically links `feos-dosbox` |

Anyone distributing the `feos` kernel binary must follow GPL-2.0-or-later
(`COPYING`) for the whole binary, including providing its source. The other
crates stay available under `MIT OR Apache-2.0` when used without `feos-dosbox`.

## Third-party code

| Code | Location | Licence | Notes |
|---|---|---|---|
| Nuked-OPL3, Rust port | `libs/dosbox/src/opl3/` | LGPL-2.1-or-later (`COPYING.LESSER`) | Used here under the GPL, as LGPL section 3 allows |
| `F80` from rust-dos, based on mwemu | `libs/dosbox/src/f80.rs`, `libs/dosbox/src/fpu.rs` | rust-dos: GPL-2.0-or-later | mwemu's licence has not been confirmed |
| H.264 decoder | `libs/h264/` | MIT OR Apache-2.0 | See `libs/h264/docs/` and `libs/h264/UPSTREAM.md` |
| MP3 decoder port | `libs/audio-codec/src/mp3/` | See its SPDX headers | |
| `font8x8_basic` | `libs/gfx/src/font.rs` | Public domain | |

Other files in `libs/dosbox` have not been compared against rust-dos. The crate
as a whole is treated as GPL-2.0-or-later, so that comparison doesn't affect the
licence stated here.

## Icons

`assets/icons` holds a collection of icons supplied by the project owner, who
reports that they are open for inclusion. The source folder had no licence file,
and these terms have not been independently confirmed. They are not covered by
the licences above. `assets/icons/index.tsv` records each file's original number.

## Game data

`data/games` holds third-party shareware, demo and freeware archives. They are
not covered by the licences above. `data/games/README.md` records the labels from
their source site, and each title's own licence, inside its ZIP, applies.
