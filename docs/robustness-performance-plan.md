# Vivi correctness, robustness and performance plan

Audit date: 2026-09-08. Scope: current vivi, its Vivid 1.5 integration, and playback through
remote vvmux reached with vvssh. The findings below describe the audit baseline. Implementation and verification results are
recorded in [implementation-results.md](implementation-results.md). Installed binaries were not replaced.

## Findings that matter most

The reported symptoms have several plausible causes, rather than one demonstrated root cause.
The current producer still blocks its command consumer on media I/O, resumes from a local clock,
and declares paused-seek completion without verifying the displayed picture. The remote test also
exposed deployment skew and loss of the remote-audio policy flag inside vvmux.

### F1 — P1: blocked media prevents command handling and bounded shutdown

`video_player.rs:732–755` sends catch-up and ordinary video synchronously from the loop that
consumes UI commands. `audio_streamer.rs:316` does the same for audio-only playback.
The SDK's `channel.rs:849–890` can wait indefinitely for flow, followed by blocking transport I/O.
A paused or stalled downstream hop can therefore prevent vivi from consuming resume, seek or quit.
Checking credit before a paused pre-roll send does not guarantee that the subsequent write cannot
block. The separate input thread queues commands but cannot execute them.

Startup is only partly isolated: `finish_send_while_observing_start` at
`video_player.rs:2252` waits on a freshly spawned sender, does not consume UI commands, and drops
its join handle on observation errors or timeout without first cancelling/joining the sender.
Audio shutdown/seek also joins workers with no native demux deadline. Vivi does not retain the
current SDK's `Session::cancel_handle()` for an independent shutdown path.

**Change:** keep control/input on a responsive coordinator; use persistent, independently bounded
video and audio senders, cancellation handles for both session and transports, and bounded native
I/O. A stop signal must not compete for media queue capacity. Quiesce and join owned workers on all
error paths. Preserve ordered media IDs and same-channel EOS.

### F2 — P1: resume rebases the group onto an unverified local clock

`PlaybackTimeline` (`playback_ui.rs:45–100`) advances a local `Instant` and is never reconciled
with the presenter's audio clock. `toggle_video_pause` (`video_player.rs:2149–2178`) freezes that
estimate after the PAUSE round trip and sends it as a new exact `PLAY.start_pts` on resume.
Audio-only playback repeats this pattern (`audio_streamer.rs:452–470`). Network delay, startup
buffering and recovery can separate this estimate from the frozen presenter position. Repeated
pause/resume can thus introduce clock jumps, skips or late video even if the UI time looks normal.

Vivid 1.5 explicitly makes active audio the master clock and says PLAY's OK means admission,
not playback started. SDK `TrackStatus` already exposes playback state, current-generation
last-decoded/last-presented PTS and presentation IDs; vivi currently ignores that information for
its timeline. A relay's virtual admission is also not proof that physical output has started.

**Change:** use generation-qualified presenter observations for authoritative paused/resumed
position. Interpolate locally only for UI display. Query once at transitions when necessary;
do not query every packet. Preserve an exact user seek target separately from the decoder's
random-access point and from the chosen displayed frame.

### F3 — P1: paused-seek completion is inferred from packet count or starvation

`PausedSeekPreRoll::step` (`video_player.rs:1991–2014`) stops after 16 submitted target-or-later
packets or 400 ms without credit. Neither proves the target picture has been presented. Network
or downstream scheduling stalls can exceed that grace. `last_pts` is the last submitted packet's
PTS, not the maximum decoded/presented PTS; reordered streams add another mismatch.
`prime_video_seek` (`video_player.rs:2100–2106`) also discards PLAY/PAUSE failures, including a
failed PAUSE after successful PLAY.

**Observed remotely:** paused seek targets around 43.76 s and 96.8 s retained pictures at
43.32 s and 96.0 s respectively. The second was still behind its requested position about
19 seconds later. This proves a paused-seek display error on the deployed stack, but does not
isolate the producer from its older gateway/presenter behavior.

**Change:** track requested target, random-access start, submitted pre-roll and observed output
as separate state. Use bounded current-generation presentation/readiness feedback, including the
outer physical presenter through the gateway. Treat starvation as pending or an actionable
timeout, never successful completion. Continue servicing resume/seek/quit while waiting.
Handle positioning failures explicitly and make every continuation generation-qualified.

### F4 — P1: vvmux removes the policy that prevents remote audio fallback

`vvmux/src/platform/unix.rs:296–305` scrubs all inherited `VIVID_*` variables, including
`VIVID_REMOTE`. Pane setup (`vvmux/src/session.rs:12871–12884`) reissues its own endpoint and
credential but does not restore the remote policy. The reproduction pane printed
`remote_policy=unset`. Vivi (`video_player.rs:416–449`, `main.rs:100–123`) relies on that flag
to prevent CPAL fallback. A failed audio negotiation in remote vvmux can therefore try to open an
audio device on the remote machine.

**Change:** make output locality an explicit policy across persistent multiplexer sessions and
reattach, rather than an accidental inherited environment value. Conservatively disable local
fallback in gateway/vvmux contexts unless explicitly selected. Keep local direct CPAL fallback.
Do not preserve stale outer endpoints, credentials or anchor framing just to keep this flag.
Update vvmux pane policy and vivi together; vvssh already sets `VIVID_REMOTE=1` on its shell.

### F5 — P1: send-error recovery still skips to the next GOP

The NEED_KEYFRAME event path rewinds the demuxer (`video_player.rs:686–706`), but the generic
send-error path (`video_player.rs:824–847`) advances/flushes, sets `awaiting_keyframe`, and
continues forward without seeking. It discards subsequent interframes until the next keyframe;
near EOF no such frame may exist. That leaves a flushed decoder blank/frozen for a GOP or until
completion. This also treats permanent validation errors as if another channel generation could
repair them.

**Change:** unify recoverable send errors and keyframe events into one generation transition,
rewind immediately to a valid random-access unit, preserve an outstanding seek target and
coordinate the audio catch-up floor. Classify permanent errors and cap retries. Track loss remains
owner-scoped and must retain the surface/node.

### F6 — P1: hand-written FFmpeg ABI is incompatible with FFmpeg 5.x

`ffmpeg.rs:129–144` unconditionally puts `AVStream.codecpar` immediately after `id`. FFmpeg 5.1
puts it near the end of AVStream. `build.rs` only switches parts of AVCodecParameters based on
libavutil/libavcodec majors; it does not select or reject the incompatible AVStream layout.
Reading that layout on such a remote Linux installation can read invalid pointers. This is a
separate portability defect; the supplied remote host reports libavformat 62.3.100.
See the primary [FFmpeg 5.1 AVStream definition](https://raw.githubusercontent.com/FFmpeg/FFmpeg/n5.1/libavformat/avformat.h)
and [FFmpeg 6.1 definition](https://raw.githubusercontent.com/FFmpeg/FFmpeg/n6.1/libavformat/avformat.h).

**Change:** use header-derived bindings or a small C accessor shim compiled against selected
headers. Fail the build for unsupported major combinations; do not guess a layout when probing
fails. Test actual Linux distribution FFmpeg versions, not just macOS's current install.

### F7 — P2: native errors become EOF, and fallible initialization leaks allocations

`VideoDemuxer::next_media_packet` (`ffmpeg.rs:667`) and `AudioDemuxer::next_packet`
(`ffmpeg.rs:865`) translate every negative `av_read_frame` return to clean EOF. Interrupted or
failed reads can silently truncate playback and the full inspection pass. Initialization uses
`?` on `video_info` at line 539 after both context and packet allocation, and on `audio_info` at
line 796 after context allocation, before an owning Drop guard exists.

**Change:** distinguish EOF, retryable conditions, cancellation and real I/O errors. Wrap each
native allocation in RAII immediately. Bound open/inspect/read/seek operations with an FFmpeg
interrupt callback; use process isolation where native operations cannot honor cancellation.
Bound packet and inspection-accounting memory before copying, while retaining exact finite claims.

### F8 — P2: local audio fallback disappears after seeking

All three video seek paths assign `local_audio = None` (`video_player.rs:663,1111,1265`).
Only presenter audio is restarted. A direct local session that needed CPAL fallback becomes silent
after the first seek. The producer can also abandon presenter audio after startup timeout or seek
failure with only verbose diagnostics, obscuring the cause of apparent A/V failure.

**Change:** centralize seek transitions for streaming, audio-drain and playback-end states;
restart eligible local audio at the same target, and expose audio degradation in normal UI status.
Remote policy must be checked before every fallback, including recovery.

### F9 — P2: terminal text and image inputs have incomplete boundary limits

Audio filenames reach `Print` through `playback_ui.rs:136–142,378–388`; `truncate` at line 469
only counts Unicode scalar values. ESC/control characters are unsanitized, and CJK/combining text
is measured incorrectly. `image_viewer.rs:52` reads the whole input before checking resource
claims; SVG external assets are also fully read at line 373. Path confinement and output raster
limits already exist, but do not bound encoded bytes, SVGZ expansion or aggregate asset decoding.
Image dimensions are reopened from the path instead of parsed from the bytes that will be sent.

**Change:** sanitize terminal output and clip by grapheme/cell width. Bound encoded reads,
decompressed input and aggregate SVG assets before allocation; use the same immutable bytes for
image metadata and submission. Keep the existing path confinement and raster checks.

### F10 — P2: startup and playback do avoidable copying, thread creation and I/O

Every video startup packet clones its buffer and creates a thread (`video_player.rs:757–788`),
with readiness queries as often as 4 ms. The steady video demuxer also constructs audio payloads
that the video loop immediately discards (`ffmpeg.rs:657–717`, `video_player.rs:475–479`), while a
second demuxer reads the audio stream. H.26x first copies the packet, then allocates Annex B data.
UI commands use an unbounded channel (`playback_ui.rs:153`), and status repainting is not
restricted to changed visible content.

**Change:** persistent senders, move owned packets, skip unneeded streams before copying, reuse
conversion buffers, coalesce resize/seek requests within ordering barriers, bound command queues,
and throttle unchanged status output. Keep audio/video flow independent: a shared demuxer must
not introduce head-of-line blocking. Preserve the full inspection pass and validate any cached
inspection against file identity/content changes. Profile before larger architectural changes.

### F11 — P2: checked-in build and compatibility guidance lag the current stack

`cargo test --offline --locked --all-targets` fails because vivi's lockfile lacks the current SDK
dependency graph. `AGENTS.md` and the migration guide still defer vvmux integration even though
vvmux now implements 1.5. The current README also describes binary trace connections, while the
current shared stack uses bounded metadata-oriented tracing.

**Change:** minimally refresh the lockfile, correct guidance, and make remote vvmux a required
playback target in CI/release verification. Report executable versions/build revisions in audit
artifacts because repository HEAD alone does not identify an installed binary.

## Shared-stack review

Local source baseline:

| Component | Revision |
|---|---|
| vivi 0.3.2 | `51385f5` |
| vivid_protocol 1.5.7 | `02d62e9` |
| vivid_sdk 1.5.6 | `888538f` |
| vivid_gateway | `959ebed` |
| vvmux | `a420bf1` |

The protocol/SDK already include checked framing/media validation, stricter error handling,
flow-lock release before transport I/O, native write interruption and producer cancellation.
Use these APIs rather than another framing/authentication implementation. Preserve exact 1.5
prefaces/profiles, complete owner/context/surface/track/channel identity, monotonic media IDs,
advance-before-flush ordering and required key/full recovery units. No wire change is proposed.

Current gateway code already addresses preferred audio-clock selection, paused-position
reconciliation and pre-roll admission. Do not reintroduce those older fixes blindly.
`vivid_gateway/src/outer.rs:1780–1800` still gates resumed outer PLAY on a fresh audio submission;
test the full-buffer and already-EOS cases under delay to establish that this cannot form a credit
cycle. This is a verification target, not a demonstrated current gateway defect.

Vivido's group controls need a focused contract test: `vivido/src/vivid/mod.rs:3371,3421–3428,
3448–3456` changes scene group state but controls a physical audio output only when the named
track itself owns that output. A video-addressed PAUSE on a linked surface can therefore freeze
scene state without stopping its audio device. Vivi normally names audio for ordinary pause;
its seek/recovery helpers also name video. Test and fix group-wide physical output dispatch in
Vivido if confirmed by that regression, rather than scattering producer workarounds.

## Remote reproduction

Used the user-provided `vvssh -p 3333 192.168.2.157`, then a separate remote vvmux session named
`vivi-audit-0908`, with `~/github/vivido-private/medias/under_attack.webm`.
The remote binaries were not on PATH, so the verified `target/release/vivi` and
`target/release/vvmux` paths were used. They identify as vivi 0.3.1 and vvmux 0.4.3 and have
September 1 timestamps. Their exact build revisions are not embedded/verified.

Remote checkouts: vivi `39c07e2`, vvmux `99a8484`, SDK `d6dd00a`, gateway `0012383`, protocol
`e42a8dd`. They predate the current local SDK/gateway/vvmux robustness work. Nothing was deployed,
pulled, rebuilt or edited on the remote host.

The fixture is 224.581 seconds, AV1 640×480 at 25 fps plus Opus 48 kHz, with audio starting at
−7 ms. Normal playback and ordinary pause reached the local presenter. Two seek-while-paused
cycles retained a pre-target picture as detailed in F3. Resume after the first cycle advanced
video from 43.32 s to 55.76 s and did issue PLAY to the audio output. No permanent freeze or
physical A/V offset was conclusively measured in this short run. A/V assessment requires actual
audio-device clock/sample observations, not just packet arrival or screenshots.

Inside the remote vvmux pane, `VIVID_REMOTE` was unset, confirming F4. The temporary vvmux session
and dedicated local Vivido/vvssh window were closed after testing.

## Implementation sequence

1. **Make a trustworthy baseline.** Refresh vivi's lockfile and guidance; add versioned diagnostics.
   Build the current stack consistently in a separate staging location for direct, local-vvmux,
   vvssh-direct and vvssh-remote-vvmux comparison. Do not overwrite installed tools during diagnosis.
2. **Establish regression coverage before playback restructuring.** Add a fixture harness for
   AV1/Opus (including this fixture), H.264/AAC with B frames/long GOP, VFR, negative/nonzero start
   PTS, audio-only, video-only and local fallback. Drive real command handling and scripted channel
   stalls; current helper-level tests do not run the complete nested playback loop.
3. **Fix cancellation and control responsiveness (F1).** Introduce a coordinator and persistent
   independently bounded senders, shutdown outside media queues, bounded native operations, and
   deterministic ownership/teardown. Keep per-track queues bounded by both bytes and records.
4. **Unify pause, seek and recovery (F2/F3/F5/F8).** Use explicit states such as Preparing, Playing,
   Paused, Seeking, Recovering and Draining, each with a generation and cancellation boundary.
   Centralize the three duplicated seek paths. Use authoritative clock/presentation observations;
   distinguish command admission, decoder readiness and actual presentation. Handle failures
   transactionally and surface them without losing the retained frame or audio policy.
5. **Repair remote policy and group controls (F4 plus shared regressions).** Keep vvmux's credential
   isolation, introduce deliberate fallback policy for nested producers, and validate current
   gateway flow/clock handoffs. Fix demonstrated presenter group-control defects at their owner.
6. **Harden FFI and external inputs (F6/F7/F9).** Replace guessed layouts, add supported ABI checks,
   allocation guards, proper EOF/error handling, bounded media reads and safe terminal rendering.
7. **Optimize measured hot paths (F10).** Benchmark inspection, first-frame/seek latency, packet
   allocation volume, native/heap peak memory, sender threads, control queries, PTY repaint bytes,
   encoded throughput and queue occupancy. Apply buffer/thread/copy changes only with measurements.

## Acceptance and verification

Proposed thresholds are implementation targets, not claims about today's behavior:

- Command admission remains under 100 ms while video/audio writes are stalled; quit cancels and
  completes teardown within two seconds plus bounded process-reaping overhead.
- Pause freezes the physical audio clock and visible video, not merely producer state. Resume
  continues from the frozen PTS without a seek, keyframe wait or accumulating offset.
- A paused seek presents the chosen target frame within frame-timing tolerance and stays paused.
  No completion may be inferred solely from 400 ms starvation or 16 packets. Resume during
  pre-roll cannot be undone by a late PLAY/PAUSE from a retired operation.
- Test 50 pause/resume/seek cycles, rapid alternating seeks, seeks near EOF, long GOPs, audio
  underrun, lost credit, delayed replies, partial writes, reconnect and tab/detach/reattach.
  Include 0/50/150/400 ms RTT, jitter and constrained throughput. Keep any network impairment
  inside a test transport; do not alter the user's host network configuration.
- Measure audio device position against last presented video PTS; initially target no sustained
  offset above 80 ms after settling and no growth across cycles. Confirm the tolerance against
  decoder/device behavior. Compare all four route topologies above.
- Audio rejection in remote vvmux must never open CPAL remotely. Test direct local fallback,
  remote existing-daemon attach and nested/reattached sessions separately.
- Fault-inject every seek/recovery phase. Verify no abandoned sender, leaked native handles,
  duplicate audio workers, stale-generation effects or accidental surface/node recreation.
  Exercise two owners reusing the same local IDs and keep the unrelated owner progressing.
- Verify checked malformed/truncated inputs, interrupted reads, bounded inspection, terminal
  escapes/CJK/combining text, SVGZ/asset budgets, and Linux FFmpeg major-version combinations.
- Run formatting, all-target/all-feature tests and strict Clippy for every affected crate; run
  socket tests with socket creation allowed. Shared changes require the corresponding SDK,
  gateway, vvmux and Vivido regression suites as well as the remote fixture.

Audit baseline on this macOS host: formatting passes; after resolving the lockfile only in an
isolated temporary copy, default and all-feature tests each pass 69 tests, and strict all-target
Clippy passes. A current-binary dry run of the local `medias/under_attack.webm` also completed successfully.
Five socket tests initially failed with sandbox PermissionDenied and passed on
rerun with socket creation allowed. Windows-specific tests do not execute on this host.
Passing tests do not establish nested playback correctness: existing tests largely inspect
state helpers, offline control sequences and a synthetic presenter without real A/V output.
