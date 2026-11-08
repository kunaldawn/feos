Sample videos
=============

These tiny clips exercise the video demuxers and decoders built into feos:

  animation.gif        animated GIF
  motion-jpeg.avi      Motion JPEG in AVI
  raw-video.avi        uncompressed RGB in AVI
  h264-sample.mp4      H.264 in MP4
  h264-sample.mkv      H.264 in Matroska
  h264-sample.flv      H.264 in FLV
  h264-stream.h264     Annex-B H.264 elementary stream
  mpeg1-sample.mpg     MPEG-1 video in a program stream, no sound
  vp9-unsupported.*    metadata only; VP9 pixel decoding is not built in

VP8 in WebM or IVF plays too, and so does Opus, in Ogg, WebM or MP4, though no sample of
them is kept here. A program stream with Layer II sound plays that too.

Sound files, the same two seconds in WAV, AIFF, AU, FLAC, MP3, AAC and Ogg Vorbis, are
in sound/. av-sample.mp4 has video and sound, and av-sample.srt is its subtitles, picked
up by name. Pictures (BMP, JPEG, PNG and ICO) are in pictures/.

Run `player` from the shell, or open Media Player from the Start menu. Run
`play /media/sound/tone.wav` to hear a file; Music and Image Viewer are in the Start menu.

Files on a disk can be much larger than memory: they're read as they play. In the Media
Player, Ctrl+S saves the playlist as an M3U file and Ctrl+L loads it back, and opening an M3U
file plays it. `rec 5 /tmp/in.wav` records five seconds, given QEMU's hda-duplex.
