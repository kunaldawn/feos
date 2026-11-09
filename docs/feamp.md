# FeAmp and MilkDrop

FeAmp is FeOS's classic skinned music player: the three stacked panels of a
late-90s player (main window, ten-band equalizer, playlist) drawn entirely in
code, with MilkDrop visualization. Open it from the Start menu (Multimedia) or
the desktop icon.

## Pieces

| Where | What |
|---|---|
| `apps/commands/src/feamp/mod.rs` | State, input, playback, the window glue (`FeAmp`, command `feamp`) |
| `apps/commands/src/feamp/skin.rs` | Layout, hit testing and drawing of every panel |
| `apps/commands/src/feamp/font.rs` | Its own 5x7 and 3x5 pixel fonts |
| `apps/commands/src/feamp/tags.rs` | ID3v2 titles and MP3 durations for the playlist |
| `apps/commands/src/feamp/vis.rs` | The MilkDrop window (`milkdrop`) and its render task |
| `apps/commands/src/equalizer.rs` | The equalizer: ten RBJ peaking biquads, preamp, balance |
| `libs/milkdrop` | The MilkDrop engine: EEL expressions, `.milk` parser, audio analysis, software renderer |
| `libs/math` | `f64`/`f32` sqrt, exp, ln, pow and trigonometry, which `core` lacks |
| `libs/mixer` (`scope`) | What's being heard, before the master volume, for visualizers |
| `assets/music` | Two original demo tracks, synthesized by `scripts/make-demo-music.py` |
| `assets/milkdrop` | 49 presets from MilkDrop's original pack (see its `README.txt`) |

FeAmp's window is frameless (`window.frameless` in its manifest): it draws its
own title bars and moves, shades, minimizes and resizes itself through
`WindowContext::request`. Sound plays on `AudioTask` through `FEAMP_PIPE`,
which applies the equalizer to decoded samples before the mixer.

## Using it

| Key or control | Does |
|---|---|
| Z X C V B | previous, play, pause, stop, next |
| Left / Right | seek 5 s back / forward |
| Up / Down, mouse wheel | volume |
| S, R | shuffle, repeat |
| L, eject, ADD | add every sound file on the system |
| Ctrl+D, the D letter | double size |
| Ctrl+W, double-click the title | shade to a strip |
| Alt+G / Alt+E, EQ / PL buttons | show or hide the equalizer / playlist |
| Ctrl+K, the V letter | open MilkDrop |
| Click the time | elapsed or remaining |
| Click the small spectrum | spectrum, oscilloscope, off |
| Playlist: double-click, Enter, Delete, PgUp/PgDn | play, play, remove, move |

Dragging volume, balance, seek or an EQ band shows its value in the title
display, as players of the time did. Files dropped on the window are added and
played.

In MilkDrop: Space or N next preset, Backspace or P previous, R random order,
L lock the preset, Enter show its name, F frame rate, F1 help, click next.
Presets change every 20 seconds unless locked.

## Visualization plugins

Winamp-era visualization plugins were native Windows DLLs, which can't run
here. What made MilkDrop what it was is its presets, and those are portable:
`libs/milkdrop` runs `.milk` files as MilkDrop did, with per-frame and
per-pixel equations, the warp mesh, waveforms in all eight modes, MilkDrop 2's
custom waves and shapes, motion vectors, borders, video echo and the output
filters. MilkDrop 2 presets that use HLSL pixel shaders load and run everything
except the shaders. Presets are read from `/media/milkdrop`, or from anywhere on
the system when that folder is missing or empty.

To see presets without booting, render them on the host:

```sh
ffmpeg -i song.mp3 -f s16le -ac 2 -ar 44100 song.raw
cargo run --release --example render -p feos-milkdrop --target x86_64-unknown-linux-gnu -- \
    --audio song.raw --seconds 8 --out /tmp/shots assets/milkdrop/*.milk
```

`FEAMP_PREVIEW=/tmp/feamp.ppm cargo test -p feos-apps --target x86_64-unknown-linux-gnu feamp`
saves a frame of FeAmp itself.
