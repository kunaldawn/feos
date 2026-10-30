# Video test fixtures

The clips in this directory are tiny (32x32, four-frame) files generated with
FFmpeg so the integration tests can exercise every container without checking
large binaries into the repository.

The `ref_*.rgba` files are the matching frame 0 (or frame 1 for
`ref_gif_1.rgba` / `ref_mp4_1.rgba`) decoded by FFmpeg into packed RGBA8888:

```sh
ffmpeg -i clip.mp4 -frames:v 1 -pix_fmt rgba -f rawvideo ref_mp4_0.rgba
```

The tests compare the decoder output against these references with a small
tolerance for IDCT rounding and H.264 chroma upsampling. Regenerate a fixture
with any `testsrc`/`smptebars` source at 32x32 and 10 fps, then rebuild the
matching reference with the command above.
