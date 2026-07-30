# Remote Linux Audio Through Vivido

Vivi sends encoded audio over the Vivid 1.5 realtime track connection:

```text
remote Vivi -> vvssh -> local Vivido decoder/output -> speakers
```

The SSH host needs Vivi and FFmpeg libraries, but no PulseAudio server, ALSA device, or remote
audio configuration.

Start the session from a shell inside local Vivido:

```sh
vvssh user@linux-host
```

`vvssh` forwards `VIVID_ENDPOINT_CONTROL`, optionally forwards `VIVID_ENDPOINT_BULK`, derives the
realtime fallback, transfers `VIVID_ROOT_SECRET` through its protected setup path, and exports
`VIVID_REMOTE=1`. The secret is never an SSH or remote-shell argument.

On Windows it also exports `VIVID_ANCHOR_TRANSPORT=conpty`, selecting the bounded marker-v3
envelope needed across the pseudoconsole.

Inside the remote shell:

```sh
test -S "${VIVID_ENDPOINT_CONTROL#unix:}" && printf 'Vivid control forward is ready\n'
test "$VIVID_REMOTE" = 1 && printf 'Remote audio mode is active\n'
vivi clip.mp4
vivi song.mp3
```

If the presenter rejects the audio configuration, remote audio-only playback fails explicitly and
remote video does not open a device on the SSH host. A local, non-SSH invocation may instead use
CPAL fallback. Track loss is scoped: failure of the audio track does not delete the video surface
or its scene node.

Use `vivi --verbose` to inspect profile, track, channel, and playback diagnostics. Also verify that
local Vivido can open its default output device and that its FFmpeg runtime libraries are
discoverable.
