# Vivi

`vivi` is the Vivid Protocol 1.0 image viewer and media player for
[Vivido](../vivido/). It requires a presenter that selects Vivid 1.0; there is no fallback to the
retired pre-current profile, anchor-v1, or FFmpeg-packet-v0.

Vivi streams encoded embedded audio and standalone MP3, M4A, FLAC, Ogg/Opus, Ogg/Vorbis, and WAV
access units to Vivido. Vivido decodes and plays them through the system default CoreAudio, ALSA,
or WASAPI device. This same path carries sound from a remote Linux Vivi over SSH to a local Windows
or macOS Vivido. A local Vivi can fall back to direct CPAL output when a presenter does not accept
the requested audio configuration.

Vivido exposes a private endpoint and per-window token to its child shell:

```text
VIVID_ENDPOINT=unix:/private/path/to/endpoint.sock
VIVID_ENDPOINT_BULK=unix:/optional/private/media.sock
VIVID_TOKEN=<64 hexadecimal characters>
```

Control always uses `VIVID_ENDPOINT`. If `VIVID_ENDPOINT_BULK` or `--bulk-endpoint` is set,
non-control connections prefer it and fall back to the primary endpoint only if connection setup
fails before ticket attachment. Media records never use stdout. Stdout carries only normal
terminal output and the bounded, authenticated marker-v2 sequence used for inline placement.
Under multiplexers without Vivid anchor support, Vivi suppresses the marker and places the node at
the reported grid cursor instead; vvmux consumes and securely projects marker-v2 anchors.

## Install

**Pre-requisites:**

    sudo apt install ffmpeg libasound2-dev  # linux
    brew install ffmpeg  # mac

Install:

    cargo install vivi


## Media paths

- PNG and JPEG are sent as their original encoded bytes with exact dimensions, length, and SHA-256.
- Other still formats decode to straight-alpha RGBA8. Vivi sends zstd only when the complete zstd
  record is smaller than the raw record.
- Video is inspected in a first pass to determine exact metadata and maximum access-unit size, then
  reopened and streamed as portable H.264/HEVC Annex B, VP9 frames, or AV1 low-overhead units.
- The first supported audio stream is submitted independently of video decode order. Vivi creates
  linked video/audio in one ordered control flight and sends a bounded audio pre-roll before
  `PLAY`. Vivido resamples audio for the default device and uses played audio frames as the A/V
  clock.
- Opus uses a complete canonical `OpusHead`, Vorbis uses one three-header Xiph-laced block, and
  FLAC uses the raw 34-byte STREAMINFO payload. Vivi normalizes FFmpeg/container variants before
  source creation and never applies Opus pre-skip twice.
- MP3, AAC/ALAC M4A, Opus, Vorbis, FLAC, and common PCM files keep the Vivid session open until
  buffered playback has completed. Remote audio-only playback fails clearly when the presenter
  cannot accept the audio configuration.
- Unsupported codecs, packetization, or declared color metadata values fail explicitly. When a
  video omits color metadata, Vivi warns and uses deterministic conventional defaults: BT.601 for
  SD or BT.709 for HD, with BT.709 transfer and limited range. Vivi never falls back to the retired
  FFmpeg packet profile.

The background control dispatcher preserves concurrent correlated replies and immediately routes
credit, visibility, display, keyframe-recovery, source-loss, and keepalive records. Valid inbound
traffic proves liveness. Clean `PONG` samples conservatively increase the initial `PLAY` buffer to
at least twice RTT plus 25 ms, capped at 500 ms; they do not drive ongoing adaptation. Video
production pauses or throttles while invisible and resumes from an acceptable random-access unit
after `NEED_KEYFRAME`.

After 15 seconds without inbound control traffic, Vivi sends an idle `PING`. Any valid inbound
record resets liveness; three consecutive unanswered probes terminate the session with a timeout.

## Build

Vivi uses Rust edition 2024 (Rust 1.85 or newer) and FFmpeg development libraries for
`libavformat`, `libavcodec`, `libavutil`, and `libswresample`. Direct compatibility playback uses
CPAL (CoreAudio on macOS, ALSA on Linux, and WASAPI on Windows).

```bash
cd vivi
cargo build
```

On macOS, install `ffmpeg` and `pkg-config` with Homebrew. On Debian or Ubuntu, install
`pkg-config`, the matching FFmpeg development packages including `libswresample-dev`, and
`libasound2-dev`.

On Windows, build with the MSVC Rust toolchain from a Visual Studio Developer Command Prompt:

```powershell
$env:VCPKG_ROOT = "C:\path\to\vcpkg"
vcpkg install ffmpeg:x64-windows
$env:VCPKG_DEFAULT_TRIPLET = "x64-windows"
$env:PATH = "$env:VCPKG_ROOT\installed\x64-windows\bin;$env:PATH"
cargo build --release
```

The build script derives the FFmpeg ABI layout from the selected vcpkg headers and requires the
`swresample` import library. Keep that triplet's `bin` directory on `PATH` when running Vivi so its
FFmpeg DLLs can be found.

## Usage

Inside Vivido:

```bash
vivi photo.png
vivi clip.mkv
vivi song.mp3
vivi -z 1.5 photo.webp clip.mp4
vivi --bulk-endpoint unix:/private/media.sock clip.mkv
```

For a remote login, use `vvssh`; see [Running Vivi over SSH](../docs/vivi-over-ssh.md). The wrapper
uses private stream-local forwarding and transfers the token through protected stdin setup, never
as an SSH or remote-shell argument. Audio uses the same Vivid tunnel; see
[Remote Linux audio over SSH](docs/ssh-linux-audio.md).

To run Linux Vivi in WSL under Windows Vivido, see
[Running WSL Vivi inside Windows Vivido](../docs/vivi-over-wsl.md). That setup uses WSL environment
bridging, mirrored loopback networking, and the Windows ConPTY anchor transport.

For high-bandwidth or high-latency links, `vvssh --separate-media-transport user@host` creates a
second lifecycle-bound SSH TCP connection and exports `VIVID_ENDPOINT_BULK` remotely. It is opt-in;
the ordinary single-transport path remains the default.

Conformance traces can be generated without a presenter:

```bash
vivi --dry-run --verbose photo.png
vivi --trace-dir /tmp/vivi-trace --verbose clip.mkv
```

Each trace begins with the version-1.0 `VIVD` preface and contains Vivid 1.0 records.
Dry-run and trace modes emit deterministic audio control/media records and never open an audio
output device.

`--no-wait` preserves immediate video submission and skips local video audio. It is invalid for an
audio-only file, because local playback cannot continue after Vivi exits.

## Limits

Audio device selection, volume controls, generic fragmentation, damage rectangles, shared memory,
caching, source reconfiguration, session resumption, blind keyframe seeking, adaptive playback
telemetry, and seek/loop APIs are outside the current profile.
Very large raw raster frames and access units that exceed their negotiated source ceiling are
rejected. Windows uses a private loopback TCP endpoint; non-loopback TCP endpoints are rejected by
`vvssh`.

The normative wire contract is
[vivid-protocol-1.0-spec.md](../vivid_protocol/vivid-protocol-1.0-spec.md).
