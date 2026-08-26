# Migrating Vivi from Vivid 1.1 to 1.5

Vivi 0.3 is a clean Vivid Protocol 1.5 cutover. It has no 1.1 parser, downgrade, environment alias,
or wire-compatible mode.

## Discovery and authentication

| Vivid 1.1 | Vivid 1.5 |
|---|---|
| `VIVID_ENDPOINT` / `--endpoint` | `VIVID_ENDPOINT_CONTROL` / `--control-endpoint` |
| no realtime endpoint | `VIVID_ENDPOINT_REALTIME` / `--realtime-endpoint` |
| `VIVID_ENDPOINT_BULK` | unchanged; falls back to control |
| `VIVID_TOKEN` / `--token` | `VIVID_ROOT_SECRET`; environment only |

Realtime falls back to bulk and then control. Root secrets must be installed by Vivido or `vvssh`
through the protected environment/setup path; do not place them in argv, logs, or traces.

## Object and media mapping

| Vivid 1.1 | Vivid 1.5 |
|---|---|
| source | stable surface plus immutable slot tracks |
| linked video/audio sources | video and audio tracks activated atomically on one surface |
| media ticket and attachment | authenticated track channel and channel generation |
| rolling byte/packet credit | absolute cumulative channel flow maxima |
| source visibility/readiness | generation-local track milestones |
| control-stream EOS/barrier | ordered same-channel `CHANNEL_EOS` |
| anchor marker v2 | context-qualified authenticated marker v3 |

Implementations must preserve complete `(session, context, surface, track, generation)` identity.
Recreating or recovering a track must not recreate the surface or node. Media IDs continue
increasing across channel generations; recovered video starts with a key unit and raster/image
tracks start with complete content.

Vivi requires `vivid-core-control-v1`, `terminal-surface-v1`, `live-media-v1`, `timed-media-v1`,
and `observability-v1`. Visual surfaces use `generic-content-v1`.

## Traces and automation

Dry-run traces now contain one 1.5 control trace and separate authenticated track traces. Record
names and diagnostics use surfaces, tracks, channels, absolute flow, activation, and milestones.
Tools that decoded 1.1 sources, tickets, feature IDs, credit records, or marker v2 must be updated;
there are no compatibility projections.

`vvmux` cannot carry this Vivi version until its separate Vivid 1.5 migration is complete.
