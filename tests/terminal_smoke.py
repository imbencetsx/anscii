"""Exercise the release binary through a real PTY (Python is only a test tool)."""
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
import codecs
import pyte

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/anscii"


class TerminalTests(unittest.TestCase):
    def test_native_audio_clock_pause_mute_seek_and_cleanup(self):
        with tempfile.TemporaryDirectory() as folder:
            clip = Path(folder) / "audio clip.mp4"
            subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=24",
                            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "4",
                            "-c:v", "libx264", "-c:a", "aac", str(clip)], check=True)
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
            # Exercise the actual native device clock without making test noise.
            process = subprocess.Popen([str(BINARY), folder, "--mute", "--volume", "0"],
                                       stdin=slave, stdout=slave, stderr=slave)
            screen = pyte.Screen(100, 30)
            stream = pyte.Stream(screen)
            decoder = codecs.getincrementaldecoder("utf-8")("replace")

            def pump(duration):
                total = 0
                deadline = time.monotonic() + duration
                while time.monotonic() < deadline:
                    if select.select([master], [], [], 0.02)[0]:
                        chunk = os.read(master, 262144)
                        total += len(chunk)
                        stream.feed(decoder.decode(chunk))
                return total

            def wait_for(text, timeout=7):
                deadline = time.monotonic() + timeout
                while text not in "\n".join(screen.display) and time.monotonic() < deadline:
                    pump(0.03)
                self.assertIn(text, "\n".join(screen.display))

            try:
                wait_for("LIBRARY")
                os.write(master, b"\r")
                wait_for("MUTED")
                wait_for("00:01 / 00:04")
                self.assertNotIn("Audio unavailable", "\n".join(screen.display))
                os.write(master, b" ")
                wait_for("PAUSED")
                pump(0.1)
                frozen = list(screen.display)
                self.assertEqual(pump(0.25), 0)
                self.assertEqual(frozen, screen.display)
                os.write(master, b"m")
                wait_for("AUDIO 0%")  # gain stays zero for the entire test
                os.write(master, b"m]")
                wait_for("MUTED")
                pump(0.08)
                os.write(master, b"[m")
                wait_for("AUDIO 0%")
                os.write(master, b"m\x1b[C")
                wait_for("00:03 / 00:04")
                self.assertIn("PAUSED", "\n".join(screen.display))
                os.write(master, b"\x1b[D")
                wait_for("00:00 / 00:04")
                pump(0.15)
                self.assertEqual(pump(0.15), 0)
                os.write(master, b" ")
                wait_for("PLAYING")
                wait_for("00:01 / 00:04")
                os.write(master, b"\x1b[C")
                wait_for("FINISHED")
                os.write(master, b" ")
                wait_for("PLAYING")
                wait_for("00:01 / 00:04")
                children = subprocess.run(["pgrep", "-P", str(process.pid)], capture_output=True, text=True).stdout.split()
                os.write(master, b"q")
                deadline = time.monotonic() + 3
                while process.poll() is None and time.monotonic() < deadline:
                    pump(0.02)
                process.wait(timeout=1)
                self.assertEqual(process.returncode, 0)
                for pid in children:
                    with self.assertRaises(ProcessLookupError):
                        os.kill(int(pid), 0)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                os.close(master)
                os.close(slave)

    def test_play_pause_click_seek_color_resize_eof_and_quit(self):
        with tempfile.TemporaryDirectory() as folder:
            clip = Path(folder) / "clip with spaces.mp4"
            subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i",
                            "testsrc2=size=160x90:rate=24", "-t", "3", "-c:v", "mpeg4",
                            str(clip)], check=True)
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
            original = termios.tcgetattr(slave)
            process = subprocess.Popen([str(BINARY), folder], stdin=slave, stdout=slave,
                                       stderr=slave, env={**os.environ, "TERM": "xterm-256color"})
            pending = bytearray()
            screen = pyte.Screen(100, 30)
            stream = pyte.Stream(screen)
            decoder = codecs.getincrementaldecoder("utf-8")("replace")

            def receive():
                chunk = os.read(master, 262144)
                stream.feed(decoder.decode(chunk))
                pending.extend(chunk)
                return len(chunk)

            def read_until(needle, timeout=5):
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    # Inspect reconstructed screen content: incremental updates
                    # deliberately split words around unchanged characters.
                    if needle.isalpha() and needle.decode() in "\n".join(screen.display):
                        return bytes(pending)
                    index = pending.find(needle)
                    if not needle.isalpha() and index >= 0:
                        end = index + len(needle)
                        result = bytes(pending[:end])
                        del pending[:end]
                        return result
                    if select.select([master], [], [], 0.05)[0]:
                        receive()
                self.fail(f"Missing {needle!r}; last output: {pending[-1000:]!r}")

            def drain(duration):
                deadline = time.monotonic() + duration
                total = len(pending)
                pending.clear()
                while time.monotonic() < deadline:
                    if select.select([master], [], [], 0.01)[0]:
                        total += receive()
                        pending.clear()
                return total

            def style(key, label):
                os.write(master, key)
                deadline = time.monotonic() + 3
                while label not in screen.display[0] and time.monotonic() < deadline:
                    if select.select([master], [], [], 0.05)[0]:
                        receive()
                self.assertIn(label, screen.display[0])
                drain(0.08)
                self.assertIn("PAUSED", "\n".join(screen.display))
                return [screen.buffer[y][x] for y in range(4, 27) for x in range(27, 98)]

            try:
                read_until(b"LIBRARY")
                os.write(master, b"\r")
                read_until(b"PLAYING")
                read_until(b"\x1b[38;2;")
                os.write(master, b" ")
                read_until(b"PAUSED")
                drain(0.1)
                self.assertEqual(drain(0.25), 0, "paused frames must produce no terminal traffic")
                cells = style(b"2", "SPACE")
                self.assertTrue(all(cell.data == " " for cell in cells))
                self.assertTrue(any(cell.bg != "default" for cell in cells))
                self.assertTrue(any(cell.data == "▀" for cell in style(b"3", "BLOCKS")))
                self.assertTrue(any(cell.data in "oOQ0#%@" for cell in style(b"4", "DENSE")))
                os.write(master, b"+i")
                read_until(b"INVERTED")
                drain(0.08)
                os.write(master, b"i-")
                cells = style(b"5", "SOLID")
                self.assertTrue(any(cell.data == "@" for cell in cells))
                self.assertTrue(all(cell.data in " @" for cell in cells))
                self.assertTrue(any(cell.data in "░▒▓█" for cell in style(b"6", "SHADE")))
                cells = style(b"1", "ASCII")
                self.assertTrue(all(cell.bg == "default" for cell in cells), "background from pixel styles leaked")
                self.assertEqual(drain(0.15), 0)
                # SGR mouse reporting is enabled by Crossterm on all platforms.
                # Timeline runs from x=27 to x=97 (zero-based); midpoint=62.
                os.write(master, b"\x1b[<0;63;29M\x1b[<0;63;29m")
                read_until(b"=")
                read_until(b"\x1b[38;2;")  # sought preview arrives while paused
                drain(0.1)
                self.assertEqual(drain(0.15), 0)
                os.write(master, b"\x1b[C")
                read_until(b"=")
                os.write(master, b"\x1b[D")
                read_until(b"-")
                os.write(master, b"c")
                read_until(b"WHITE")
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 20, 60, 0, 0))
                screen.resize(lines=20, columns=60)
                process.send_signal(signal.SIGWINCH)
                read_until(b"\x1b[2J")
                drain(0.3)
                os.write(master, b" ")
                read_until(b"PLAYING")
                read_until(b"FINISHED", timeout=6)
                os.write(master, b" ")
                read_until(b"PLAYING")
                os.write(master, b"q")
                deadline = time.monotonic() + 3
                while process.poll() is None and time.monotonic() < deadline:
                    drain(0.02)
                process.wait(timeout=1)
                self.assertEqual(process.returncode, 0)
                restored = termios.tcgetattr(slave)
                self.assertEqual(original, restored, "terminal must leave raw mode on exit")
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                os.close(master)
                os.close(slave)


if __name__ == "__main__":
    unittest.main()
