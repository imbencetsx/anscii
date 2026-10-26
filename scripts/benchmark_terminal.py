"""Measure a large PTY with a controlled read rate, without a GUI emulator.

Example: python3 scripts/benchmark_terminal.py --size 640x192 --mibps 8
Output is terminal delivery/response time, not measured GPU painting FPS.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import resource
import select
import shutil
import struct
import subprocess
import tempfile
import termios
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path, default=Path("target/release/anscii"))
parser.add_argument("--video", type=Path, default=Path("videos/demo.mp4"))
parser.add_argument("--size", default="640x192")
parser.add_argument("--seconds", type=float, default=3)
parser.add_argument("--mibps", type=float, default=8)
parser.add_argument("--style", default="ascii", choices=["ascii", "whitespace", "blocks", "dense", "solid", "shade"])
args = parser.parse_args()
width, height = map(int, args.size.split("x"))
rate = args.mibps * 1024 * 1024
with tempfile.TemporaryDirectory() as folder:
    shutil.copyfile(args.video, Path(folder) / ("clip" + args.video.suffix))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
    original = termios.tcgetattr(slave)
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    process = subprocess.Popen([str(args.binary.resolve()), folder, "--no-audio", "--style", args.style],
                               stdin=slave, stdout=slave, stderr=slave)
    credit = 65536.0
    last = time.monotonic()
    tail = b""

    def drain(duration, stop_at=None):
        global credit, last, tail
        until = time.monotonic() + duration
        count = updates = 0
        matched = False
        while time.monotonic() < until:
            now = time.monotonic()
            credit = min(65536, credit + (now - last) * rate)
            last = now
            if credit < 1024:
                time.sleep(0.001)
                continue
            if select.select([master], [], [], 0.002)[0]:
                chunk = os.read(master, min(int(credit), 65536))
                credit -= len(chunk)
                count += len(chunk)
                data = tail + chunk
                updates += data.count(b"\x1b[?2026l")
                if stop_at and stop_at in data:
                    matched = True
                    break
                tail = data[-7:]
        return count, updates, matched

    try:
        drain(0.5)
        os.write(master, b"\r")
        drain(0.3)
        count, updates, _ = drain(args.seconds)
        os.write(master, b" ")
        started = time.monotonic()
        _, _, paused = drain(3, b"AUSED")
        pause_ms = (time.monotonic() - started) * 1000
        drain(0.15)
        paused_bytes, _, _ = drain(0.25)
        os.write(master, b"q")
        started = time.monotonic()
        while process.poll() is None and time.monotonic() - started < 3:
            drain(0.005)
        process.wait(timeout=1)
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        print(json.dumps(dict(binary=str(args.binary), size=args.size, style=args.style, sink_mibps=args.mibps,
            updates_per_second=round(updates / args.seconds, 2), kib_per_second=round(count / args.seconds / 1024),
            pause_ms=round(pause_ms), pause_seen=paused, quit_ms=round((time.monotonic() - started) * 1000),
            paused_bytes=paused_bytes, child_cpu_seconds=round(after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime, 3),
            restored=termios.tcgetattr(slave) == original, exit_code=process.returncode)))
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)
