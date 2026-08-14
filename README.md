# Vivi

`vivi` is the Vivid Protocol 1.5 image viewer and media player for
[Vivido](../vivido/). It is a clean 1.5 producer and does not negotiate or emit Vivid 1.1.

Vivido exports private transport discovery and a root secret to its terminal child:

```text
VIVID_ENDPOINT_CONTROL=unix:/private/path/to/control.sock
VIVID_ENDPOINT_REALTIME=unix:/optional/private/realtime.sock
VIVID_ENDPOINT_BULK=unix:/optional/private/bulk.sock
VIVID_ROOT_SECRET=<64 hexadecimal characters>
```

The realtime endpoint falls back to bulk and then control; bulk falls back to control. The root
secret is read only from the environment and has no command-line option. Media bytes use
authenticated track connections, never the terminal PTY. Stdout contains ordinary terminal output
and, when safe, one bounded authenticated anchor-v3 marker.

## Media model

- Every visual input owns a stable `generic-content-v1` surface and terminal scene node.
- PNG and JPEG normally use a live encoded-image poster track with exact dimensions, length, and
  SHA-256. A presenter cache hit needs no track connection. Unsupported encoded-image
  configurations fall back to a complete RGBA8 raster track. SVG and SVGZ inputs are rendered
  locally with resvg and use the same RGBA8 path; relative image assets are confined to the SVG's
  directory tree.
- Video uses a timed primary-video track on the bulk lane. Vivi performs a complete inspection pass
  before allocation to establish canonical codec metadata, maximum access-unit size, and finite
  rate/bitrate claims.
- Presenter audio uses an independent timed audio track on the realtime lane. Audio pre-roll cannot
  be blocked by video flow, and the active audio slot is the surface playback clock.
- Opus uses canonical `OpusHead`, Vorbis uses Xiph-laced initialization headers, and FLAC uses the
  raw 34-byte STREAMINFO payload.
- Track activation waits for current-generation decoded output and applies video/audio slot changes
  atomically. EOS is ordered on each track channel and buffered playback completes afterward.
- A local Vivi may fall back to CPAL when presenter audio is unsupported. Remote sessions never
  open a remote audio device.

Terminal placement uses marker v3 with complete context identity. Windows SSH sessions select the
bounded ConPTY form with `VIVID_ANCHOR_TRANSPORT=conpty`. Vivi suppresses markers under an
untrusted tmux/screen intermediary and uses a viewport-grid node instead.

## Build

Vivi uses Rust edition 2024 and FFmpeg development libraries for `libavformat`, `libavcodec`,
`libavutil`, and `libswresample`.

```bash
cd vivi
cargo build
```

Typical dependencies:

```bash
sudo apt install pkg-config libavformat-dev libavcodec-dev libavutil-dev libswresample-dev libasound2-dev
brew install ffmpeg pkg-config
```

On Windows, use the MSVC Rust toolchain and an FFmpeg vcpkg triplet. The build stages the required
FFmpeg DLLs beside Cargo's `vivi.exe`, so a successful build runs without a developer-specific
`PATH`.

## Usage

Inside Vivido:

```bash
vivi photo.png
vivi drawing.svg
vivi clip.mkv
vivi song.mp3
vivi -z 1.5 photo.webp clip.mp4
vivi -i clip.mkv
vivi --bulk-endpoint unix:/private/media.sock clip.mkv
```

Video and audio use the full terminal window by default. `-i`/`--inline` preserves the previous
inline placement and non-interactive playback behavior. Full-window playback uses the same keys as
Kitim: `f`/`b` seek 10 seconds, Left/Right seek 5 seconds, Up/Down change volume by 5%, `g` opens a
timestamp prompt, and `q` or Ctrl-C stops the current file. Images always remain inline.

For remote use, run `vvssh`; see [Remote Linux audio](docs/ssh-linux-audio.md). A separate media
transport may provide `VIVID_ENDPOINT_BULK`; realtime audio inherits the protocol fallback chain.

Installing Vivi also installs the small Linux `vvreceive` companion. A current `vvssh` starts it
quietly before the login shell so a confirmed local file drop can be copied into that shell's
current directory. Older remote installations keep the existing filename-paste behavior; pass
`vvssh --no-receive-drops` to suppress helper startup explicitly.

Generate protocol fixtures without a presenter:

```bash
vivi --dry-run --verbose photo.png
vivi --trace-dir /tmp/vivi-trace --verbose clip.mkv
```

Traces contain separate Vivid 1.5 control and track connections with exact 1.5 prefaces. Dry-run
and trace modes do not read a root secret or open an audio device.

`--no-wait` still submits ordered media and EOS, but skips presentation/playback completion waits
and local video audio. Audio-only input still uses a presenter audio track; it does not fall back
to a local audio device in this mode.

See [Migrating Vivi from Vivid 1.1 to 1.5](docs/vivid-1.1-to-1.5-migration.md) for the breaking
discovery and object-model changes. The normative contract is
[Vivid Protocol 1.5](../vivid_protocol/vivid-protocol-1.5-spec.md).
