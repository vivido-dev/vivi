#!/usr/bin/env python3
"""Opt-in physical playback check in a dedicated Vivido automation window.

Start the desired route and AV1/Opus fixture first. This sends playback keys only to the supplied
automation instance/window. It records bounded metadata, never screenshots, media or credentials.
Example: python3 tests/playback_smoke.py --instance vivi-test --window 1 --cycles 50
"""
import argparse
import json
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--window", required=True)
    parser.add_argument("--cycles", type=int, default=50)
    parser.add_argument("--frame-us", type=int, default=40_000)
    parser.add_argument("--seek-targets", default="20,35", help="alternating seek positions in seconds")
    parser.add_argument("--paused-eof-pts-us", type=int, help="test a paused seek to the last picture, then back")
    args = parser.parse_args()
    targets = [int(value) for value in args.seek_targets.split(",")]
    if args.cycles < 1 or not targets:
        parser.error("positive cycles and at least one seek target are required")
    base = ["vivido", "msg", "-t", args.instance]

    def command(*parts):
        return subprocess.check_output(
            base + list(parts) + ["--window-id", args.window], text=True, timeout=10
        )

    def tracks():
        windows = json.loads(subprocess.check_output(base + ["list-windows"], text=True, timeout=10))["windows"]
        window = next(w for w in windows if str(w["window_id"]) == args.window)
        if window.get("occluded") or not window.get("visible", True):
            raise RuntimeError("test window is occluded or hidden; expose it before measuring physical presentation")
        values = json.loads(command("vivid", "tracks"))["tracks"]
        video = [t for t in values if t["kind"] == "video" and t["lifecycle"] == 1]
        audio = [t for t in values if t["kind"] == "audio" and t["lifecycle"] == 1]
        if not video or not audio:
            return None
        return max(video, key=lambda t: t["track_id"]), max(audio, key=lambda t: t["track_id"])

    def wait_for(predicate, seconds=8):
        deadline = time.monotonic() + seconds
        while True:
            state = tracks()
            if state is not None and predicate(*state):
                return state
            if time.monotonic() >= deadline:
                raise RuntimeError(f"playback transition timed out: {state}")
            time.sleep(0.05)

    initial = wait_for(lambda v, a: a["playback"]["paused"] is not None)
    if args.paused_eof_pts_us is not None:
        if not initial[1]["playback"]["paused"]:
            command("typing", " ")
        wait_for(lambda v, a: a["playback"]["paused"] is True)
        command("typing", "g999999")
        command("key", "Enter")
        wait_for(lambda v, a: v["last_presented_pts_us"] == args.paused_eof_pts_us)
        time.sleep(0.3)
        command("typing", f"g{targets[0]}")
        command("key", "Enter")
        wait_for(lambda v, a: v["last_presented_pts_us"] is not None
                 and targets[0] * 1_000_000 <= v["last_presented_pts_us"]
                 <= targets[0] * 1_000_000 + args.frame_us
                 and a["playback"]["paused"] is True)
        print(json.dumps({"paused_eof_then_seek": "passed"}), flush=True)
        initial = tracks()
    if initial[1]["playback"]["paused"]:
        command("typing", " ")
    offsets = []
    for cycle in range(args.cycles):
        wait_for(lambda v, a: a["playback"]["paused"] is False)
        command("typing", " ")
        video, audio = wait_for(lambda v, a: a["playback"]["paused"] is True)
        frozen = audio["playback"]["clock_pts_us"]
        time.sleep(0.2)
        _, still = tracks()
        assert still["playback"]["clock_pts_us"] == frozen, "paused audio clock moved"
        latency_ms = None
        if cycle % 3 == 0:
            target = targets[(cycle // 3) % len(targets)]
            started = time.monotonic()
            command("typing", f"g{target}")
            command("key", "Enter")
            video, audio = wait_for(
                lambda v, a: v["last_presented_pts_us"] is not None
                and target * 1_000_000 <= v["last_presented_pts_us"]
                <= target * 1_000_000 + args.frame_us
                and a["playback"]["paused"] is True
                and abs(a["playback"]["clock_pts_us"] - target * 1_000_000) < args.frame_us
            )
            latency_ms = round((time.monotonic() - started) * 1000, 1)
            frame = video["last_presented_pts_us"]
            time.sleep(0.2)
            assert tracks()[0]["last_presented_pts_us"] == frame, "paused target picture advanced"
        before = video["last_presented_pts_us"]
        command("typing", " ")
        wait_for(lambda v, a: a["playback"]["paused"] is False and v["last_presented_pts_us"] > before)
        time.sleep(0.4)
        video, audio = tracks()
        offset = audio["playback"]["clock_pts_us"] - video["last_presented_pts_us"]
        offsets.append(offset)
        assert abs(offset) <= 80_000, f"A/V offset exceeded 80 ms: {offset}"
        print(json.dumps({"cycle": cycle + 1, "offset_us": offset, "seek_ms": latency_ms}), flush=True)
    print(json.dumps({"cycles": args.cycles, "max_abs_offset_us": max(map(abs, offsets))}), flush=True)


if __name__ == "__main__":
    main()
