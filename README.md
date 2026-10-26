# ANSCII

A native Rust terminal video player with full RGB ASCII, a popup video picker, and a
clickable bottom timeline. Uses FFmpeg for decoding and native audio output for sound.

```sh
cargo build --release --locked
./anscii videos

# Any folder, including nested folders
./anscii ~/Movies --recursive
```

`./anscii` runs the optimized native binary after building it on your machine.
You can also use `cargo run --release -- videos`, or install a command on your PATH:

```sh
cargo install --path . --locked
anscii ~/Movies
```

Requirements: Rust 1.88+ to build, FFmpeg/ffprobe on PATH to play, and an interactive
terminal. On macOS, install FFmpeg with `brew install ffmpeg`. After building, no
Rust toolchain or Python is needed to run the binary. The old
`python3 anscii.py videos` command forwards to the same Rust binary.

## Linux setup

The video picker (**V**), bottom play bar, click-to-seek, rendering styles, and
native audio all use the Linux code path too. Ubuntu 22.04 and 24.04 are covered
by the existing CI matrix.

Install the build and runtime dependencies, then build with Rust 1.88 or newer:

```sh
# Debian / Ubuntu
sudo apt update
sudo apt install build-essential pkg-config libasound2-dev ffmpeg

# Fedora (use this instead of the apt commands)
sudo dnf install gcc pkgconf-pkg-config alsa-lib-devel ffmpeg-free

# Arch Linux (use this instead of the apt commands)
sudo pacman -S --needed base-devel pkgconf alsa-lib ffmpeg
```

From this repository, with Rust and Cargo on PATH:

```sh
cargo install --path . --locked
anscii ~/Videos
```

ALSA development dependencies are required to build the native audio backend.
Fedora's `ffmpeg-free` has a restricted codec set; file support depends on the
installed FFmpeg build. A desktop audio device is used automatically; on a
headless server use `anscii ~/Videos --no-audio`. Mouse controls require terminal
mouse reporting; all navigation and seeking also work from the keyboard.

CI also uploads an `anscii-<runner>-<architecture>` artifact containing
`anscii.tar.gz`. Download a successful build matching your Linux distribution and
CPU architecture from the repository's Actions tab, unzip the artifact, then:

```sh
tar -xzf anscii.tar.gz
./anscii ~/Videos
```

These builds still need FFmpeg/ffprobe and the ALSA runtime libraries. CI archives
retain executable permissions and are kept for seven days. Build from source on
older distributions whose system libraries differ from the runner's.

On macOS, H.264 and HEVC video use **VideoToolbox hardware decoding by default**.
On Linux, the default asks FFmpeg to select an available accelerator for the video
and installed drivers. CPU-only systems continue in software. If an explicit
accelerator cannot produce the first frame, playback retries in software at the
same time and resolution.

```sh
anscii videos                         # automatic hardware selection
anscii videos --hwaccel off           # CPU decoding
anscii videos --hwaccel vaapi         # Intel/AMD on Linux
anscii videos --hwaccel cuda          # NVIDIA on Linux
anscii videos --hwaccel videotoolbox  # Apple on macOS
```

Linux needs an FFmpeg build with the relevant accelerator and working GPU drivers.
`ffmpeg -hwaccels` lists compiled backends, not proof that a usable device exists.
VAAPI needs access to the GPU render device (typically `/dev/dri/renderD128`);
NVIDIA needs the appropriate driver and video decoding libraries. No root access
is needed by the player. Containers must have the required device/driver access.

Hardware decoding offloads source decompression. RGB conversion, resizing, ASCII
processing, and terminal painting still have CPU costs. GPU-to-CPU transfers can
outweigh decoding savings on some systems or small videos: compare `--hwaccel off`
with the default on your machine. The player keeps the same resolution and colors.
For CPU-only machines or slower terminals, `--fps 15` reduces per-second work;
`--color-depth 256` is an explicit quality/bandwidth tradeoff, never automatic.

| Input | Action |
| --- | --- |
| V / click V videos | Open / close the video picker |
| Up / Down (or K / J) | Select a file in the picker |
| Enter | Play selected file from the beginning |
| Space | Pause / resume; replay after finishing |
| M | Mute / unmute |
| [ / ] | Lower / raise volume by 5% |
| Left / Right | Seek backward / forward 5 seconds |
| Click timeline | Jump to that point; preserves pause |
| C | Cycle color, white, amber |
| T | Cycle rendering techniques |
| 1–6 | Choose ASCII, whitespace, blocks, dense, solid, or shade |
| + / - | Increase / decrease character fill density |
| I | Invert the image |
| R | Rescan folder |
| Esc | Close the picker; quit from the player |
| Q / Ctrl-C | Quit |

Clicking the timeline after the video finishes resumes playback there. Some
terminals reserve Shift-click for text selection. The video fills the space above
the three-row bottom play bar. Press V to switch videos; Enter or a click plays the
selected file, and Esc closes the picker. Window resizing is supported.

## Sound

Sound is enabled automatically for videos with an audio track, through the default
system output device. Space pauses both audio and video. Arrow and timeline seeks
restart both streams at the selected time; style changes and resizing keep audio
running. Video follows the audio device's playback clock, accounting for queued
output latency. Muting keeps that clock running, so unmuting stays in sync.

```sh
anscii videos --volume 50
anscii videos --mute
anscii videos --no-audio
```

`--no-audio` skips audio decoding and device initialization entirely. Files without
audio play normally. If an output device or audio decoder fails, the app reports
the problem and continues video playback. Audio uses the first audio track and
resamples to the output device's format. No FFplay or MPV installation is required.
On Linux, building native output may also require your distribution's ALSA
development libraries. The optional native-audio integration test needs an audio
output device; it runs muted so it does not make test noise.

## Rendering styles

Switch styles while playing or paused with T or 1–6. Style and density shortcuts
work without adding labels or menus to the video view.

- **1 / ASCII**: the classic brightness-to-character ramp.
- **2 / Whitespace**: the picture is made entirely from spaces with colored
  backgrounds. White/amber mode produces shaded monochrome background pixels.
- **3 / Blocks**: half-block characters with separate top and bottom colors,
  providing twice the vertical color detail of a full-cell pixel.
- **4 / Dense**: fuller `oOQ0#%@` characters, including in darker areas, for a
  more filled picture with the original sampled colors.
- **5 / Solid**: `@` for every non-black cell; color carries the image detail.
- **6 / Shade**: `░▒▓█` shading characters, from light texture to full coverage.

Use **+ / -** to increase/decrease character fill density in ASCII, Dense, Solid,
and Shade. The default is 128. Density changes glyph selection, preserving sampled
RGB colors. **I** toggles inversion. All styles support color, white, amber, and
256-color output. Blocks and Shade need the corresponding Unicode font glyphs.

```sh
anscii videos --style dense
anscii videos --style solid
anscii videos --style shade --density 160
```

Each style decodes one RGB sample per cell, except Blocks which uses two vertical
samples. FFmpeg averages the source directly, avoiding extra RGB transfer and CPU
averaging. No terminal cells are lost.

Style changes show a cached preview immediately. When the sample layout changes,
the video decoder refines that preview in the background at the same media time;
audio keeps running. Paused previews settle back to zero terminal output.

Color mode preserves each sampled pixel's full 24-bit RGB value. Use a truecolor
terminal for accurate colors. ASCII shapes, font, cell resolution, and terminal
color settings still affect the result. For older terminals, use the palette mode:

```sh
./anscii videos --fps 30 --mode color
./anscii videos --mode amber
./anscii videos --color-depth 256
```

Supported library extensions: MP4, MKV, MOV, WebM, AVI, M4V, GIF, MPEG, MPG, TS,
FLV, and WMV. Actual codec support depends on your FFmpeg installation.

## Performance

The renderer compares consecutive screens and writes only changed cells in one
buffered terminal update. It never clears the video area per frame. Paused and
unchanged screens produce no terminal output. RGB buffers are recycled, and a
bounded three-frame decoder queue keeps memory use independent of movie length.
FFmpeg decoding and metadata loading do not block keyboard/mouse input. Playback
uses the audio clock when available (a wall clock otherwise) and drops stale frames
when behind instead of playing in slow motion. Input polling follows frame
deadlines rather than waking every 4 ms. Window resizing is debounced to avoid
repeatedly restarting FFmpeg.

Styles sample only the data they need. The compact RGB cache uses
3 bytes per cell; blocks read the two vertical samples directly. Audio
has a bounded half-second PCM ring buffer, with no allocations or locks in its
real-time callback. Pausing suspends native audio callbacks where supported.

### Large terminals / small fonts

Smaller fonts automatically use the extra rows and columns. There is no hidden
resolution cap or automatic reduction to 256 colors. Blocks give two
independently colored vertical pixels per cell.

Terminal encoding and output run on a separate presenter thread. At most one
screen is in flight: if the terminal is busy, the app waits to compose the latest
frame instead of queuing obsolete screens. Keyboard/mouse and the audio clock
continue running during output backpressure. On macOS/Linux, terminal writes can
be interrupted on quit, including when the terminal stops reading. Grid storage
is reused, and mouse-motion reports no longer rebuild a large video image.

```sh
anscii videos --style dense
anscii videos --style blocks
anscii --benchmark --benchmark-size 640x192
anscii --benchmark --benchmark-size 960x288
```

The benchmark dimensions are terminal columns and rows, not source video pixels.
Actual speed still depends on terminal painting and output throughput; a worst-case
640×192 truecolor screen can require over 2 MB per update. `--fps 20` is an option
for especially slow terminals without reducing spatial detail.

Use a release build for playback. Increasing terminal dimensions increases the
number of ASCII cells and terminal bandwidth; `--fps 24` is the default. Highly
detailed full-motion truecolor footage can still be limited by the terminal's
painting speed.

```sh
./anscii --benchmark
```

The benchmark reports CPU time and bytes generated by the renderer for static,
sparse-motion, and full-motion synthetic screens, plus the processing cost of
each rendering technique. It does not measure FFmpeg or
the terminal emulator. See [PERFORMANCE.md](PERFORMANCE.md) for local measurements.

## Development checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
python3 tests/large_terminal.py -v

# Optional PTY test using a terminal emulator (Python is a test tool only)
uv run --no-project --with pyte==0.8.2 python tests/terminal_smoke.py -v
```

The PTY test checks playback, pause, click/keyboard seeking, RGB output, color
switching, resizing, EOF/replay, zero output while paused, and terminal restoration.
It also switches through every technique while paused, checks the actual glyphs
and background colors, and verifies that returning to ASCII clears pixel backgrounds.
The native-audio test checks device-clock playback, pause, mute, volume controls,
seeking, EOF/replay, and cleanup of both decoder processes. Unit tests verify
sample gain, output-latency accounting, underruns, and exact PCM seek trimming.
The large-grid tests cover 640×192 → 960×288 resizing, audio, native sampling
transitions, zero traffic while paused, and quitting with an entirely blocked
terminal. Reproduce controlled-throughput measurements with:

```sh
python3 scripts/benchmark_terminal.py --video path/to/clip.mp4 --size 960x288 --mibps 4
```
