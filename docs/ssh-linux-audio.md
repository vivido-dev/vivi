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

### Audio connection abort near the end of a video

Older vivi builds dropped the linked audio channel immediately after sending `CHANNEL_EOS`.
Sending EOS only queues that record; it does not acknowledge receipt by the presenter. Through
SSH forwarding, shutting down the socket at that point could discard the final audio packets and
EOS. A Windows presenter reported this as track loss with error 18 and Winsock error 10053, even
though the failing operation was a transport read rather than decoding.

Vivi now retains the audio channel until playback completes or a seek retires its generation.
Rebuild vivi on the remote machine to pick up this fix; changing SSH keepalive settings or the
local audio device is not required for this failure. The regression was reproduced from macOS
to Windows and verified with clean audio and video EOS after the fix.

### Slow seek over SSH

Seeking compressed video requires sending reference pictures from the preceding keyframe to
the requested position. Older vivi builds queried presenter readiness for each reference picture,
even after publishing the seek target. SSH round trips could turn this pre-roll into several
seconds of black video. Once the target is published, vivi now sends those references through
bounded catch-up delivery and waits for readiness at the target. Initial surface activation
retains its readiness check. Rebuild vivi on the remote machine to pick up this fix.

Current Vivid 1.5 vvmux is also supported in the remote shell:

```sh
vvmux new --session media
# In its pane:
printf 'Local fallback policy: %s\n' "$VIVID_AUDIO_FALLBACK"
vivi clip.mp4
```

The pane policy is `deny`; vvmux deliberately reissues it after scrubbing inherited Vivid
credentials and endpoints. It remains in effect after detach/reattach. Only an explicit
`VIVID_AUDIO_FALLBACK=allow` override enables a local device in this context.

Use matching current vivi, SDK/gateway, vvmux and Vivido builds. Physical pause/seek feedback must
reach vivi through the gateway; an older presenter without the playback clock map produces an
explicit observation timeout instead of guessing a resume position from packet admission.
