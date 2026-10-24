# ANSCII

A native Rust terminal video player with full RGB ASCII, a folder sidebar, and a
clickable timeline. Uses FFmpeg for decoding and native audio output for sound.

```sh
cargo build --release --locked
./anscii videos

# Any folder, including nested folders
./anscii ~/Movies --recursive
```

The optimized binary is already built in this checkout. `./anscii` runs it directly.
You can also use `cargo run --release -- videos`, or install a command on your PATH:

```sh
cargo install --path . --locked
anscii ~/Movies
```

Requirements: Rust 1.88+ to build, FFmpeg/ffprobe on PATH to play, and an interactive
terminal. On macOS, install FFmpeg with `brew install ffmpeg`. After building, no
Rust toolchain or Python is needed to run the binary. The old
`python3 anscii.py videos` command forwards to the same Rust binary.

| Input | Action |
| --- | --- |
| Up / Down (or K / J) | Select a file |
| Enter | Play selected file from the beginning |
| Space | Pause / resume; replay after finishing |
| M | Mute / unmute |
| [ / ] | Lower / raise volume by 5% |
| Left / Right | Seek backward / forward 5 seconds |
| Click timeline | Jump to that point; preserves pause |
| C | Cycle color, white, amber |
| T | Cycle rendering techniques |
| 1–6 | Choose ASCII, whitespace, blocks, Braille, edges, or dither |
| + / - | Raise / lower dot, edge, and dither cutoff |
| I | Invert the image |
| R | Rescan folder |
| Q / Esc / Ctrl-C | Quit |

Clicking the timeline after the video finishes resumes playback there. Some
terminals reserve Shift-click for text selection. Below 65 columns, the sidebar
hides; keyboard selection still works. Window resizing is supported.

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

Switch styles while playing or paused. The active style appears in the header.

- **1 / ASCII**: the classic brightness-to-character ramp.
- **2 / Whitespace**: the picture is made entirely from spaces with colored
  backgrounds. White/amber mode produces shaded monochrome background pixels.
- **3 / Blocks**: half-block characters with separate top and bottom colors,
  providing twice the vertical color detail of a full-cell pixel.
- **4 / Braille**: eight independently sampled dots per character (2×4), for
  finer silhouettes and detail.
- **5 / Edges**: Sobel contours drawn with directional `|`, `-`, `/`, and `\` lines.
- **6 / Dither**: an ordered halftone pattern made from dots.

Lower the cutoff with **-** for more dots/contours, or raise it with **+** for fewer.
**I** toggles an inverted image. Cutoff affects Braille, edges, and dither; all
styles support color, white, amber, and the 256-color fallback. Braille, blocks,
and dither require a font with the corresponding Unicode glyphs.

```sh
anscii videos --style braille --mode white
anscii videos --style whitespace
anscii videos --style edges --threshold 64 --invert
```

Each style decodes only the samples it can display: one per cell for ASCII,
whitespace, edges, and dither; two vertical samples for blocks; and 2×4 for Braille.
FFmpeg performs the source averaging directly, avoiding an unnecessary 8× RGB
transfer and extra CPU averaging for full-cell styles. No terminal cells are lost.

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

Styles sample only the data they need. The compact average/luminance cache uses
4 bytes per cell; blocks and Braille read the shared subpixels directly. Audio
has a bounded half-second PCM ring buffer, with no allocations or locks in its
real-time callback. Pausing suspends native audio callbacks where supported.

### Large terminals / small fonts

Smaller fonts automatically use the extra rows and columns. There is no hidden
resolution cap or automatic reduction to 256 colors. Braille gives eight dot
positions per cell; blocks give two independently colored vertical pixels.

Terminal encoding and output run on a separate presenter thread. At most one
screen is in flight: if the terminal is busy, the app waits to compose the latest
frame instead of queuing obsolete screens. Keyboard/mouse and the audio clock
continue running during output backpressure. On macOS/Linux, terminal writes can
be interrupted on quit, including when the terminal stops reading. Grid storage
is reused, and mouse-motion reports no longer rebuild a large video image.

```sh
anscii videos --style braille
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
