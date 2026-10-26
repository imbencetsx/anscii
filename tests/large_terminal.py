"""Large-grid playback and blocked-output regression tests (no GUI parser)."""
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
import unittest

BINARY = Path(__file__).resolve().parents[1] / "target/release/anscii"
END = b"\x1b[?2026l"
UPDATE_TIMEOUT = 10  # Complete large screens, not a terminal-throughput benchmark.


class LargeTerminalTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.folder = Path(cls.temp.name)
        subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=24",
                        "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "8",
                        "-c:v", "libx264", "-preset", "veryfast", "-c:a", "aac", str(cls.folder / "clip.mp4")], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def launch(self, width, height, *args):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        process = subprocess.Popen([str(BINARY), str(self.folder), *args], stdin=slave, stdout=slave, stderr=slave)
        self._tails = getattr(self, "_tails", {})
        self._tails[master] = b""
        return master, slave, process

    def drain(self, fd, seconds, needle=None):
        deadline = time.monotonic() + seconds
        total = updates = 0
        tail = self._tails.get(fd, b"")
        matched = found = False
        while time.monotonic() < deadline:
            if select.select([fd], [], [], 0.01)[0]:
                chunk = os.read(fd, 262144)
                if not chunk:
                    break
                total += len(chunk)
                data = tail + chunk
                updates += data.count(END)
                # Labels may arrive before the rest of a megabyte-sized screen.
                # Acknowledge them only once that synchronized update finishes.
                end_from = 0
                if needle and not matched:
                    at = data.find(needle)
                    if at >= 0:
                        matched = True
                        end_from = at + len(needle)
                if matched and END in data[end_from:]:
                    found = True
                tail = data[-(max(len(END), len(needle or b"")) - 1):]
                self._tails[fd] = tail
                if found:
                    break
        return total, updates, found

    def wait_updates(self, fd, minimum, min_bytes=0, timeout=10):
        deadline = time.monotonic() + timeout
        total = updates = 0
        while time.monotonic() < deadline:
            count, completed, _ = self.drain(fd, 0.05)
            total += count
            updates += completed
            if updates >= minimum and total >= min_bytes:
                return
        self.fail(f"Playback stalled: {updates} completed updates, {total} bytes in {timeout}s")

    def assert_quiet(self, fd, quiet=0.5, timeout=10):
        # A paused frame or resize refinement may still be in flight. Drain it,
        # then demand a continuous quiet window; never hide ongoing redraws.
        deadline = time.monotonic() + timeout
        last_output = time.monotonic()
        total = updates = 0
        while time.monotonic() < deadline:
            count, completed, _ = self.drain(fd, 0.05)
            total += count
            updates += completed
            if count:
                last_output = time.monotonic()
            elif time.monotonic() - last_output >= quiet:
                return
        self.fail(f"Paused output did not settle: {total} bytes, {updates} updates in {timeout}s")

    def test_large_grid_native_audio_style_changes_and_resize(self):
        master, slave, process = self.launch(640, 192, "--mute")
        original = termios.tcgetattr(slave)
        flags = fcntl.fcntl(slave, fcntl.F_GETFL)
        try:
            self.assertTrue(self.drain(master, UPDATE_TIMEOUT, b"Videos")[2])
            os.write(master, b"\r")
            # Small startup/status screens are not evidence of decoded video.
            self.wait_updates(master, 3, min_bytes=128 * 1024)
            os.write(master, b" ")
            self.assertTrue(self.drain(master, UPDATE_TIMEOUT, b"AUSED")[2])
            self.assert_quiet(master)
            os.write(master, b"4")
            self.wait_updates(master, 1)
            self.assert_quiet(master)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 288, 960, 0, 0))
            process.send_signal(signal.SIGWINCH)
            self.assertTrue(self.drain(master, UPDATE_TIMEOUT, b"\x1b[2J")[2])
            self.assert_quiet(master)
            os.write(master, b"3")
            self.assertTrue(self.drain(master, UPDATE_TIMEOUT, "▀".encode())[2])
            self.assert_quiet(master)
            os.write(master, b"1")
            self.assert_quiet(master)
            os.write(master, b" ")
            self.wait_updates(master, 3)
            self.assertIsNone(process.poll())
            os.write(master, b"q")
            deadline = time.monotonic() + 2
            while process.poll() is None and time.monotonic() < deadline:
                self.drain(master, 0.01)
            process.wait(timeout=1)
            self.assertEqual(process.returncode, 0)
            self.assertEqual(termios.tcgetattr(slave), original)
            self.assertEqual(fcntl.fcntl(slave, fcntl.F_GETFL) & os.O_NONBLOCK, flags & os.O_NONBLOCK)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)
            os.close(slave)

    def test_quit_is_bounded_when_a_large_terminal_stops_reading(self):
        master, slave, process = self.launch(960, 288, "--no-audio")
        original = termios.tcgetattr(slave)
        try:
            self.assertTrue(self.drain(master, UPDATE_TIMEOUT, b"Videos")[2])
            os.write(master, b"\r")
            # Deliberately leave the terminal output buffer completely full.
            time.sleep(0.4)
            started = time.monotonic()
            os.write(master, b"q")
            process.wait(timeout=1)
            self.assertLess(time.monotonic() - started, 0.8)
            self.assertEqual(process.returncode, 0)
            self.assertEqual(termios.tcgetattr(slave), original)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)
            os.close(slave)


if __name__ == "__main__":
    unittest.main()
