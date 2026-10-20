`preview.webm` is a synthetic 0.3-second red 16×16 VP9 video with a 440 Hz Opus audio track.
It exercises preview audio setup without requiring a decoder or audio output device in tests.

Generate it with:

```sh
ffmpeg -nostdin -hide_banner -loglevel error \
  -f lavfi -i color=c=red:size=16x16:rate=10 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 0.3 -c:v libvpx-vp9 -threads 1 -c:a libopus -b:a 24k \
  -map_metadata -1 -fflags +bitexact -flags:v +bitexact -flags:a +bitexact \
  preview.webm
```
