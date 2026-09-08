# Vivi playback implementation results

Date: 2026-09-08. Implements the [audit plan](robustness-performance-plan.md) against Vivid 1.5.
Changes are in the working trees; installed applications and the remote checkout were not replaced.

## Behavior and implementation

- A persistent, bounded media sender lets pause, seek, volume, resize and quit run while a media
  write waits for credit or transport. Quit is independent of the command queue and cancels the
  SDK session after a two-second cleanup grace period. Workers own cancellation and join guards.
- Pause uses the current generation's physical audio position. The gateway propagates bounded,
  owner-qualified physical playback observations through private vvmux IPC. Admission to a virtual
  presenter is no longer reported as physical presentation.
- Streaming, draining and ended-video seeks share preparation, channel replacement, demux seek
  and audio restart. Paused pre-roll completes only after the requested picture is observed.
  Recovery rewinds to a preceding keyframe and preserves outstanding seek targets.
- The virtual presenter clears EOS at generation/epoch replacement. Completion feedback carries
  the decoder reset serial and active bridge instance, preventing late EOS from ending a new seek.
  The same SDK signature is adopted by vvbridge.
- A seek retires its channel generation **before** cancelling an in-flight write. Local Unix
  sockets exposed a race where cancellation EOF otherwise marked the live track lost. The
  regression forces EOF before opening the replacement and checks another owner with reused IDs.
- Video EOS is sent before waiting for linked audio, releasing delayed final B frames. Paused EOS
  remains interactive. Completion waits observe both outputs without a blocking audio drain.
  Vivido's bounded audio completion wait now observes the physical output without requiring a
  prior DRAIN request. The gateway also avoids synchronous EOS drain in its foreground bridge;
  its background status observer carries completion.
- Vivido applies group PLAY/PAUSE to the owning surface's physical audio. Paused seek permits the
  first picture at or after the target, while retaining the frozen audio position separately from
  that original target. Audio QUERY_TRACK fields remain canonically ordered.
- Local CPAL fallback is restarted at a seek target. Unsupported wire audio codecs are rejected
  before probing, allowing eligible local fallback. Vvmux panes explicitly deny local fallback;
  remote producers cannot accidentally open the remote host's audio device after negotiation fails.
- FFmpeg bindings come from selected headers, with runtime major-version checks before struct
  access. Native input has immediate RAII ownership, interruption/deadlines, true EOF/error
  distinction, checked packet/decode allocation limits and bounded inspection accounting.
- Image reads, SVGZ expansion and aggregate SVG assets have byte/pixel/nesting budgets. Metadata
  and submission use the same bytes. Terminal text is sanitized and clipped by grapheme/cell width.
- Playback skips copying unused audio packets, moves video payloads into one persistent writer,
  bounds/coalesces UI commands and throttles status/readiness work. Lockfiles and architecture /
  build / remote-audio documentation match the current SDK and header-derived bindings.

No Vivid wire assignment or profile changed. Full owner/context/surface/track/channel identities
remain authoritative. Deploying these playback fixes requires rebuilding the affected SDK,
gateway, vvmux and physical Vivido presenter together with vivi; updating vivi alone cannot fix
an older physical presenter or remove missing position feedback in an older gateway.

## Verification

macOS socket tests ran with socket creation allowed. All-feature, all-target results:

| Project | Passed | Ignored |
| --- | ---: | ---: |
| vivi | 76 | 1 opt-in benchmark |
| vivid_sdk | 161 | 0 |
| vivid_gateway | 26 | 0 |
| vvmux workspace | 601 | 0 |
| Vivido workspace | 666 | 10 platform/integration cases |
| vvbridge | 22 | 0 |

Formatting and strict all-target/all-feature Clippy pass for the affected projects. Default
all-target suites also passed. Vivido's complete all-feature suite was rerun serially after its
existing host-method-registration test failed once under parallel execution; the focused rerun
and full serial suite passed. Windows-only tests do not execute on this host.

The user-authorized Linux staging directory is `/tmp/vivi-plan-20260908/` on the supplied host.
Linux release builds and all-target/all-feature vivi tests pass against that host's FFmpeg 8
libraries (avformat 62.3.100, avcodec 62.11.100, avutil 60.8.100, swresample 6.1.100). Its existing checkout and installed binaries remain untouched.

### Physical playback

Fixture: `under_attack.webm`, 224.581 seconds, AV1 640×480 at 25 fps, Opus 48 kHz with a −7 ms
initial audio PTS. Measurements compare the physical audio clock with last presented video PTS.
Each 50-cycle run includes 17 seeks while paused, checks the frozen clock and picture, then checks
continued output after resume. Offset limits are sampled after settling, not a claim about every
instantaneous device callback or all networks.

| Route | Cycles | Maximum absolute sampled A/V offset |
| --- | ---: | ---: |
| Local direct | 10 | 38.9 ms |
| Local vvmux | 50 | 66.0 ms |
| vvssh direct, earlier full run | 50 | 60.8 ms |
| vvssh direct, final stack | 10 | 58.0 ms |
| vvssh → remote vvmux, final stack | 50 | 79.3 ms |

The final remote-vvmux run also pauses at the last picture (224.48 seconds), seeks back to
20 seconds while paused, and then performs all 50 cycles. It uses the final EOS/generation fixes.
Its 17 regular paused seeks took a median 940.6 ms and maximum 1,027.4 ms. Nested playback then
completed near EOF. The final direct-vvssh rerun also passed last-picture pause/seek-back and ten
cycles; quit left no live media tracks. Owned remote vvmux sessions and the test window were closed.

The opt-in harness is `tests/playback_smoke.py`; it requires an explicitly named dedicated
Vivido automation instance/window with playback already running. It carries only status metadata
and key commands. It rejects occluded/hidden test windows because physical presentation
timestamps deliberately stop advancing there. One later remote run was invalidated at cycle 39
by occlusion (decoding and audio continued); it was not counted as a passing run.
Media continues through the normal side channels, never the PTY.

Generated H.264/AAC B-frame fixtures exercise pausing on the final picture, seeking backward,
resuming and completing playback. Additional fixtures cover variable frame rate, a nonzero PTS
origin, audio-only WAV and local fallback. The B-frame run passed six cycles (maximum sampled offset 56 ms), including final-picture pause,
seek-back and clean EOS. Local AC3 fallback reached 5 seconds while paused and continued after
resume. WAV presenter playback froze at exactly 5 seconds after seeking, resumed, and separately
completed at EOS; its static raster backdrop remains retained. A video-only VFR fixture with
3-second PTS origin presented 5.04 seconds for a 2-second relative seek, stayed paused, and resumed.
These fixture checks use generated files outside the user's media directory.

### Allocation benchmark

Release builds, same AV1/Opus fixture, one warmup and five measured iterations. Baseline is vivi
`51385f5`, built in a separate temporary copy. Allocation counters cover Rust allocations on the
measured thread; `/usr/bin/time -l` supplies native-inclusive process peak RSS.

| Measurement | Before | After |
| --- | ---: | ---: |
| Steady video-demux allocation calls | 16,854 | 5,626 |
| Steady video-demux allocated bytes | 6,587,890 | 2,896,952 |
| Median steady demux elapsed | 10.418 ms | 10.101 ms |
| Inspection allocation calls | 16,872 | 16,873 |
| Inspection allocated bytes | 6,590,949 | 6,590,973 |
| Median inspection elapsed | 10.301 ms | 10.528 ms |
| Encoded video bytes | 2,896,745 | 2,896,745 |
| Peak process RSS | 14,712,832 B | 15,269,888 B |

Discarding unused audio payloads removes 66.6% of steady demux allocation calls and 56.0% of
allocated bytes. Timing differences are small; no throughput or peak-memory improvement is
claimed. Inspection remains a complete pass and adds a small ownership guard. End-to-end native
allocation profiling, PTY byte counts and queue occupancy were not measured by this benchmark.

## Remaining verification limits

The 0/50/150/400 ms synthetic RTT/jitter/throughput matrix, exhaustive failure injection at every
seek phase, partial-write/reconnect/detach/reattach matrix, and FFmpeg 5/6/7 plus Windows builds
remain unverified. One dedicated Vivido regression instance stalled during application shutdown
and required terminating that owned test process; Vivido-wide shutdown was not changed by this
playback work. No user's network configuration was altered. Native input interruption is
cooperative; a decoder/library call that ignores interruption is not made preemptible by these
changes. These results establish the tested routes and regressions, not a guarantee for every
codec, device, relay failure or FFmpeg distribution.
