#!/usr/bin/env python3
"""Compatibility launcher. Playback and rendering are implemented in Rust."""
import os
from pathlib import Path
import sys


def main():
    binary = Path(__file__).resolve().parent / "target" / "release" / "anscii"
    if not binary.is_file():
        print("Build first: cargo build --release --locked", file=sys.stderr)
        return 1
    os.execv(str(binary), [str(binary), *sys.argv[1:]])


if __name__ == "__main__":
    raise SystemExit(main())
