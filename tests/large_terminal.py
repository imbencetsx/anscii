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
        return master, slave, process

    def drain(self, fd, seconds, needle=None):
        deadline = time.monotonic() + seconds
        total = updates = 0
        tail = b""
        found = False
        while time.monotonic() < deadline:
            if select.select([fd], [], [], 0.01)[0]:
                chunk = os.read(fd, 262144)
                total += len(chunk)
                data = tail + chunk
                updates += data.count(END)
                if needle and needle in data:
                    found = True
                    break
                tail = data[-7:]
        return total, updates, found

    def test_large_grid_native_audio_style_changes_and_resize(self):
        master, slave, process = self.launch(640, 192, "--mute")
        original = termios.tcgetattr(slave)
        flags = fcntl.fcntl(slave, fcntl.F_GETFL)
        try:
            self.assertTrue(self.drain(master, 5, b"LIBRARY")[2])
            os.write(master, b"\r")
            self.assertGreater(self.drain(master, 0.7)[1], 2)
            os.write(master, b" ")
            self.assertTrue(self.drain(master, 3, b"AUSED")[2])
            self.drain(master, 0.15)
            self.assertEqual(self.drain(master, 0.2)[0], 0)
            os.write(master, b"4")
            self.assertTrue(self.drain(master, 3, "⣿".encode())[2])
            self.drain(master, 0.2)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 288, 960, 0, 0))
            process.send_signal(signal.SIGWINCH)
            self.assertTrue(self.drain(master, 3, b"\x1b[2J")[2])
            self.drain(master, 0.3)
            os.write(master, b"3")
            self.assertTrue(self.drain(master, 3, "▀".encode())[2])
            self.drain(master, 0.2)
            os.write(master, b"1")
            self.drain(master, 0.4)
            self.assertEqual(self.drain(master, 0.2)[0], 0)
            os.write(master, b" ")
            self.assertGreater(self.drain(master, 0.8)[1], 5)
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
            self.assertTrue(self.drain(master, 5, b"LIBRARY")[2])
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
