# Local performance verification

Measured on this machine with an optimized Rust build, 24 October 2026.

`./anscii --benchmark` uses a 160×48 screen (159 writable columns), 600 frames per
case, and full 24-bit RGB. It includes constructing changing cells and generating
terminal escape sequences, without sending them to a terminal:

- Static: 0.010 ms/frame, **0 bytes/frame** after the initial screen.
- Sparse motion (10% of cells): 0.028 ms/frame, 20,813 bytes/frame.
- Full motion: 0.111 ms/frame, 146,549 bytes/frame.

For context, the previous Python renderer generated the same 159×48 RGB pattern
in 2.064 ms/frame and wrote 152,312 bytes/frame, even when it was unchanged. The
old and new implementations do different work (full repaint versus screen diff),
so this comparison describes the application change rather than Rust's language
speed in isolation. Values vary with the machine and load.

A separate local playback smoke comparison used the same local video, a 160×48
PTY, 24 FPS, three seconds of playback, and half a second paused after settling:

- Python: 631 KiB/s while playing, 409,629 bytes while paused, 0.441 s combined
  process CPU time, about 1,032 ms to quit.
- Rust release: 327 KiB/s while playing, **0 bytes while paused**, 0.197 s combined
  process CPU time, about 15 ms to quit.

This is a single local comparison with the PTY continuously drained, not a
statistical benchmark or a measure of a real terminal emulator's painting cost.
CPU time includes the player and its reaped subprocesses over the entire run.

The terminal integration test additionally confirms **zero terminal output while
paused** and checks that mouse seeking, color changes, resizing, replay, and exit
continue to work. Rust integration tests exercise real FFmpeg decoding, seeking,
cancelling with a full frame queue, and catching up after a simulated 700 ms delay.

These timings exclude FFmpeg decoding and terminal emulator painting. Full-motion
truecolor output still consumes terminal bandwidth. The renderer preserves exact
sampled RGB rather than reducing color accuracy to improve these numbers.

## Additional rendering techniques

The style benchmark composes 159×45 terminal cells from 2×4 RGB samples per cell,
then compares the screen, for 120 frames of mostly static synthetic input. Local
release timings after adding the styles were: ASCII 0.081 ms/frame, whitespace
0.077, blocks 0.082, Braille 0.124, edges 0.157, and dither 0.083. These include
technique processing and diff generation; they exclude FFmpeg and terminal writes.
Full-motion encoding costs additional time, as measured separately above.

The decoder now keeps 2×4 samples per terminal cell for every technique. This
increases raw frame sizes by eight compared with the original one-sample ASCII
path, while keeping the same bounded three-frame queue and recycled buffers.
It permits instantaneous style changes on the paused frame without restarting
FFmpeg or allocating another image pipeline.

## Sampler and playback optimization, with native audio

Before editing, `--benchmark` was run in release mode and a one-second macOS
`sample` trace was captured during real playback. The main thread spent most
samples in the terminal event wait. The technique benchmarks identified the
shared sampler as avoidable work: every style copied eight RGB values, calculated
top/bottom means, and stored 34 bytes per cell, even when it used none of that data.

The new sampler keeps only RGB average plus luminance (4 bytes per cell). Blocks
and Braille read subpixels directly, without building that cache. Top/bottom and
dot work is now confined to the styles that need it. Exact RGB averaging and
inversion are checked against direct sample sums. Input waits are paced to media
frame deadlines, replacing the fixed 4 ms polling interval.

Medians of five release benchmark runs on the same machine, using the built-in
159×45-cell technique benchmark (milliseconds per frame, including screen diff):

- ASCII: 0.081 → 0.055 (32% lower).
- Whitespace: 0.077 → 0.054 (30% lower).
- Blocks: 0.090 → 0.055 (39% lower).
- Braille: 0.134 → 0.085 (37% lower).
- Edges: 0.160 → 0.083 (48% lower).
- Dither: 0.094 → 0.059 (37% lower).

The unchanged terminal encoder remained about 0.11 ms/frame on the full-motion
synthetic case. These numbers measure CPU work, not terminal painting or FFmpeg.
They do not include the additional CPU required to decode and play audio.

Native audio uses a bounded half-second lock-free PCM ring. The callback reuses
fixed scratch storage, applies volume/mute, and publishes a latency-adjusted media
clock. Video follows that clock; underruns stall media time instead of allowing
video to drift ahead. Device initialization and PCM decoding run off the UI thread.
Paused playback keeps producing zero terminal output, and suspends the native
audio stream where supported. Resize and rendering-style changes preserve the
audio stream; seeking rebuilds both streams at the same media offset.
