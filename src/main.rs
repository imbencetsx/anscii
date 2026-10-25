mod audio;
mod media;
mod presenter;
mod render;
mod technique;
#[cfg(test)]
mod test_support;

use audio::Audio;
use clap::{Parser, ValueEnum};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use media::{Decoder, Frame, Metadata, Probe, fit};
use presenter::Presenter;
use render::{GREEN, Grid, Ink, MUTED, Renderer};
use std::{
    fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::TryRecvError,
    time::{Duration, Instant},
};
use technique::{Painter, Sampling, Settings, Style};

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum Mode {
    Color,
    White,
    Amber,
}

impl Mode {
    fn next(self) -> Self {
        match self {
            Self::Color => Self::White,
            Self::White => Self::Amber,
            Self::Amber => Self::Color,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Color => "COLOR",
            Self::White => "WHITE",
            Self::Amber => "AMBER",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum Depth {
    #[value(name = "24bit")]
    Truecolor,
    #[value(name = "256")]
    Palette,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum HardwareMode {
    Auto,
    Off,
    Vaapi,
    Cuda,
    #[value(name = "videotoolbox")]
    VideoToolbox,
}

impl HardwareMode {
    fn backend(self, candidate: bool) -> Option<&'static str> {
        match self {
            Self::Vaapi => Some("vaapi"),
            Self::Cuda => Some("cuda"),
            Self::VideoToolbox => Some("videotoolbox"),
            Self::Auto if cfg!(target_os = "macos") && candidate => Some("videotoolbox"),
            // FFmpeg checks decoder/device support and falls back to software.
            // Do not guess the vendor from /dev/dri: it misses NVIDIA, hybrid
            // GPUs, and containers with a different device configuration.
            Self::Auto if cfg!(target_os = "linux") => Some("auto"),
            _ => None,
        }
    }
}

#[derive(Parser)]
#[command(
    version,
    about = "Fast truecolor ASCII video player with synchronized native audio. Requires FFmpeg."
)]
struct Args {
    /// Folder containing videos
    #[arg(default_value = ".")]
    folder: PathBuf,
    #[arg(short, long)]
    recursive: bool,
    #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u16).range(1..=60))]
    fps: u16,
    #[arg(long, value_enum, default_value = "color")]
    mode: Mode,
    #[arg(long, value_enum, default_value = "24bit")]
    color_depth: Depth,
    /// Rendering technique (also selectable live with T or 1-6)
    #[arg(long, value_enum, default_value = "ascii")]
    style: Style,
    /// Character fill density (128 = normal); adjust live with +/-
    #[arg(long, default_value_t = 128)]
    density: u8,
    /// Invert the sampled image (toggle live with I)
    #[arg(long)]
    invert: bool,
    /// Start muted (M toggles mute; [ and ] change volume)
    #[arg(long)]
    mute: bool,
    /// Audio volume, 0-100
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(0..=100))]
    volume: u8,
    /// Skip audio decoding/output entirely
    #[arg(long)]
    no_audio: bool,
    /// Video decoding: auto selects Linux GPU acceleration or macOS VideoToolbox
    #[arg(long, value_enum, default_value = "auto")]
    hwaccel: HardwareMode,
    /// Measure renderer CPU time and terminal output without opening the TUI
    #[arg(long)]
    benchmark: bool,
    /// Terminal grid for benchmarks, e.g. 640x192
    #[arg(long, default_value = "160x48", value_parser = parse_benchmark_size)]
    benchmark_size: (u16, u16),
}

fn discover(folder: &Path, recursive: bool) -> io::Result<Vec<PathBuf>> {
    fn visit(folder: &Path, recursive: bool, files: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in fs::read_dir(folder)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() && recursive {
                visit(&path, true, files)?;
            } else if kind.is_file()
                && path.extension().is_some_and(|ext| {
                    matches!(
                        ext.to_string_lossy().to_lowercase().as_str(),
                        "mp4"
                            | "mkv"
                            | "mov"
                            | "webm"
                            | "avi"
                            | "m4v"
                            | "gif"
                            | "mpeg"
                            | "mpg"
                            | "ts"
                            | "flv"
                            | "wmv"
                    )
                })
            {
                files.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(folder, recursive, &mut files)?;
    files.sort_by_cached_key(|p| p.to_string_lossy().to_lowercase());
    Ok(files)
}

#[derive(Clone, Copy)]
struct Layout {
    width: u16,
    height: u16,
    sidebar: u16,
}

impl Layout {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            sidebar: if width >= 65 {
                (width / 4).clamp(18, 32)
            } else {
                0
            },
        }
    }
    fn left(self) -> u16 {
        self.sidebar + 2
    }
    fn timeline(self) -> (u16, u16, u16) {
        (
            self.height.saturating_sub(2),
            self.left(),
            self.width.saturating_sub(self.left() + 2).max(1),
        )
    }
    fn video_size(self, ratio: f64) -> (u16, u16) {
        fit(
            ratio,
            self.width.saturating_sub(self.left() + 2).max(1),
            self.height.saturating_sub(7).max(1),
        )
    }
    fn seek_at(self, x: u16, y: u16, duration: f64) -> Option<f64> {
        let (row, left, length) = self.timeline();
        (self.width >= 35
            && self.height >= 10
            && duration > 0.0
            && y == row
            && x >= left
            && x < left + length)
            .then(|| f64::from(x - left) / f64::from(length.saturating_sub(1).max(1)) * duration)
    }
}

struct App {
    folder: PathBuf,
    files: Vec<PathBuf>,
    recursive: bool,
    selected: usize,
    path: Option<PathBuf>,
    probe: Option<Probe>,
    metadata: Metadata,
    decoder: Option<Decoder>,
    pending: Option<Frame>,
    image: Option<Frame>,
    clock: Option<(Instant, f64)>,
    position: f64,
    paused: bool,
    ended: bool,
    message: String,
    fps: u16,
    mode: Mode,
    depth: Depth,
    layout: Layout,
    resize_at: Option<Instant>,
    image_size: (u16, u16),
    decoder_size: (u16, u16),
    image_sampling: Sampling,
    decoder_sampling: Sampling,
    refreshing: bool,
    style: Style,
    density: u8,
    invert: bool,
    painter: Painter,
    audio: Option<Audio>,
    muted: bool,
    volume: u8,
    no_audio: bool,
    audio_failed: bool,
    audio_complete: bool,
    hwaccel: HardwareMode,
}

impl App {
    fn new(args: Args, size: (u16, u16)) -> io::Result<Self> {
        let folder = fs::canonicalize(args.folder)?;
        let files = discover(&folder, args.recursive)?;
        let message = if files.is_empty() {
            "No videos found. Add files, then press R."
        } else {
            "Choose a video and press Enter"
        }
        .into();
        Ok(Self {
            folder,
            files,
            recursive: args.recursive,
            selected: 0,
            path: None,
            probe: None,
            metadata: Metadata {
                ratio: 16.0 / 9.0,
                duration: 0.0,
                has_audio: false,
                videotoolbox_candidate: false,
            },
            decoder: None,
            pending: None,
            image: None,
            clock: None,
            position: 0.0,
            paused: false,
            ended: false,
            message,
            fps: args.fps,
            mode: args.mode,
            depth: args.color_depth,
            layout: Layout::new(size.0, size.1),
            resize_at: None,
            image_size: (0, 0),
            decoder_size: (0, 0),
            image_sampling: Sampling::Cell,
            decoder_sampling: args.style.sampling(),
            refreshing: false,
            style: args.style,
            density: args.density,
            invert: args.invert,
            painter: Painter::default(),
            audio: None,
            muted: args.mute,
            volume: args.volume,
            no_audio: args.no_audio,
            audio_failed: false,
            audio_complete: false,
            hwaccel: args.hwaccel,
        })
    }

    fn play(&mut self) -> io::Result<()> {
        let Some(path) = self.files.get(self.selected).cloned() else {
            return Ok(());
        };
        self.probe = None;
        self.audio = None;
        self.audio_failed = false;
        self.audio_complete = false;
        self.decoder = None;
        self.pending = None;
        self.image = None;
        self.clock = None;
        self.position = 0.0;
        self.metadata.duration = 0.0;
        self.metadata.has_audio = false;
        self.path = None;
        self.paused = false;
        self.ended = false;
        self.resize_at = None;
        self.probe = Some(Probe::start(path)?);
        self.message = "Loading video...".into();
        Ok(())
    }

    fn restart(&mut self, position: f64) -> io::Result<()> {
        self.audio = None;
        self.audio_failed = false;
        self.audio_complete = false;
        self.restart_video(position)?;
        if self.metadata.has_audio
            && !self.no_audio
            && let Some(path) = &self.path
        {
            match Audio::start(path, self.position, self.gain()) {
                Ok(audio) => self.audio = Some(audio),
                Err(error) => {
                    self.audio_failed = true;
                    self.message = format!("Audio unavailable: {error}");
                }
            }
        }
        Ok(())
    }

    fn gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            f32::from(self.volume) / 100.0
        }
    }

    fn restart_video(&mut self, position: f64) -> io::Result<()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        self.position = clamp_seek(position, self.metadata.duration, self.fps);
        self.decoder = None;
        self.pending = None;
        self.image = None;
        self.clock = None;
        self.ended = false;
        let size = self.layout.video_size(self.metadata.ratio);
        self.decoder_sampling = self.style.sampling();
        self.decoder = Some(Decoder::start(
            path,
            self.decoder_sampling.pixels(size),
            self.fps,
            self.position,
            self.hwaccel.backend(self.metadata.videotoolbox_candidate),
        )?);
        self.decoder_size = size;
        self.refreshing = false;
        self.message.clear();
        Ok(())
    }

    fn reconfigure_video(&mut self, position: f64) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        self.decoder = None;
        self.pending = None;
        self.decoder_size = self.layout.video_size(self.metadata.ratio);
        self.decoder_sampling = self.style.sampling();
        self.decoder = Some(Decoder::start(
            path,
            self.decoder_sampling.pixels(self.decoder_size),
            self.fps,
            clamp_seek(position, self.metadata.duration, self.fps),
            self.hwaccel.backend(self.metadata.videotoolbox_candidate),
        )?);
        self.refreshing = true;
        Ok(())
    }

    fn accept(&mut self, frame: Frame) {
        self.image_size = self.decoder_size;
        self.image_sampling = self.decoder_sampling;
        self.refreshing = false;
        self.position = frame.position;
        if let Some(old) = self.image.replace(frame)
            && let Some(decoder) = &self.decoder
        {
            decoder.recycle(old);
        }
    }

    fn finish(&mut self) {
        self.audio = None;
        self.ended = true;
        self.paused = true;
        if let Some(mut decoder) = self.decoder.take() {
            decoder.stop();
            self.message = decoder.error();
        }
        if self.message.is_empty() {
            self.message = "Finished. Space to replay, or click the timeline.".into();
        }
    }

    fn tick(&mut self, now: Instant) -> io::Result<bool> {
        let mut dirty = false;
        if let Some(result) = self.probe.as_mut().and_then(Probe::poll) {
            let probe = self.probe.take().unwrap();
            match result {
                Ok(metadata) => {
                    self.metadata = metadata;
                    self.path = Some(probe.path.clone());
                    self.restart(0.0)?;
                }
                Err(error) => self.message = error,
            }
            dirty = true;
        }
        if self.resize_at.is_some_and(|deadline| now >= deadline) {
            self.resize_at = None;
            if self.path.is_some()
                && self.layout.video_size(self.metadata.ratio) != self.decoder_size
            {
                let position = self
                    .audio
                    .as_ref()
                    .filter(|_| !self.paused)
                    .map_or(self.position, Audio::position);
                self.reconfigure_video(position)?;
                dirty = true;
            }
        }
        if let Some(audio) = &self.audio {
            if let Some(error) = audio.error() {
                self.clock = Some((now, audio.position().max(self.position)));
                self.audio = None;
                self.audio_failed = true;
                self.message = format!("Audio unavailable: {error}. Video continues.");
                dirty = true;
            } else if audio.ended() {
                self.clock = Some((now, audio.position()));
                self.audio = None;
                self.audio_complete = true;
                dirty = true;
            }
        }
        if self.decoder.is_none()
            || (self.ended && !self.refreshing)
            || (self.paused && self.image.is_some() && !self.refreshing)
        {
            return Ok(dirty);
        }
        if self.clock.is_none() || (self.refreshing && self.paused) {
            match self.decoder.as_mut().unwrap().receive() {
                Ok(frame) => {
                    self.clock = Some((now, frame.position));
                    self.accept(frame);
                    return Ok(true);
                }
                Err(TryRecvError::Disconnected) => {
                    self.finish();
                    return Ok(true);
                }
                Err(TryRecvError::Empty) => return Ok(dirty),
            }
        }
        let (anchor, base) = self.clock.unwrap();
        let target = if let Some(audio) = &self.audio {
            if !audio.ready() {
                return Ok(dirty);
            }
            if !audio.playing() {
                audio.play(true);
            }
            audio.position()
        } else {
            base + now.saturating_duration_since(anchor).as_secs_f64()
        };
        // Present the latest due frame. If decoding or terminal painting fell
        // behind, discard stale frames instead of slowing the movie's clock.
        // Limit each catch-up pass so input always gets another turn.
        for _ in 0..64 {
            if self.pending.is_none() {
                match self.decoder.as_mut().unwrap().receive() {
                    Ok(frame) => self.pending = Some(frame),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if target >= self.position + 1.0 / f64::from(self.fps) {
                            self.finish();
                            dirty = true;
                        }
                        break;
                    }
                }
            }
            if self.pending.as_ref().unwrap().position > target {
                break;
            }
            let frame = self.pending.take().unwrap();
            self.accept(frame);
            dirty = true;
        }
        Ok(dirty)
    }

    fn wake_after(&self, now: Instant) -> Duration {
        let mut wait = Duration::from_millis(250);
        if self.probe.is_some()
            || (self.decoder.is_some() && (self.image.is_none() || self.refreshing))
        {
            wait = Duration::from_millis(8);
        } else if self.decoder.is_some() && !self.paused {
            let target = self
                .audio
                .as_ref()
                .map(Audio::position)
                .or_else(|| {
                    self.clock.map(|(anchor, base)| {
                        base + now.saturating_duration_since(anchor).as_secs_f64()
                    })
                })
                .unwrap_or(self.position);
            let next = self
                .pending
                .as_ref()
                .map_or(self.position + 1.0 / f64::from(self.fps), |frame| {
                    frame.position
                });
            wait = Duration::from_secs_f64((next - target).clamp(0.004, 0.020));
        }
        if let Some(deadline) = self.resize_at {
            wait = wait.min(deadline.saturating_duration_since(now));
        }
        wait
    }

    fn event(&mut self, event: Event) -> io::Result<bool> {
        let old_style = self.style;
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Char('q' | 'Q') | KeyCode::Esc => return Ok(false),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(false);
                }
                KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected = (self.selected + 1).min(self.files.len().saturating_sub(1))
                }
                KeyCode::Enter => self.play()?,
                KeyCode::Char(' ') if self.path.is_some() => {
                    if self.ended {
                        self.paused = false;
                        self.restart(0.0)?;
                    } else {
                        self.paused = !self.paused;
                        if let Some(audio) = &self.audio {
                            audio.play(!self.paused && self.image.is_some() && audio.ready());
                        }
                        if !self.paused && self.image.is_some() {
                            self.clock = Some((Instant::now(), self.position));
                        }
                    }
                }
                KeyCode::Left if self.path.is_some() => self.restart(self.position - 5.0)?,
                KeyCode::Right if self.path.is_some() => self.restart(self.position + 5.0)?,
                KeyCode::Char('c' | 'C') => self.mode = self.mode.next(),
                KeyCode::Char('m' | 'M') => {
                    self.muted = !self.muted;
                    if let Some(audio) = &self.audio {
                        audio.gain(self.gain());
                    }
                }
                KeyCode::Char('[' | ']') => {
                    self.volume = if key.code == KeyCode::Char(']') {
                        self.volume.saturating_add(5).min(100)
                    } else {
                        self.volume.saturating_sub(5)
                    };
                    if let Some(audio) = &self.audio {
                        audio.gain(self.gain());
                    }
                }
                KeyCode::Char('t' | 'T') => self.style = self.style.next(),
                KeyCode::Char(digit @ '1'..='6') => {
                    self.style = Style::ALL[digit as usize - '1' as usize]
                }
                KeyCode::Char('+' | '=') => self.density = self.density.saturating_add(8),
                KeyCode::Char('-' | '_') => self.density = self.density.saturating_sub(8),
                KeyCode::Char('i' | 'I') => self.invert = !self.invert,
                KeyCode::Char('r' | 'R') => {
                    let selected = self.files.get(self.selected).cloned();
                    self.files = discover(&self.folder, self.recursive)?;
                    self.selected = selected
                        .and_then(|path| self.files.iter().position(|p| *p == path))
                        .unwrap_or(0);
                    self.message = format!("Found {} video(s)", self.files.len());
                }
                _ => {}
            },
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                if let Some(position) =
                    self.layout
                        .seek_at(mouse.column, mouse.row, self.metadata.duration)
                {
                    if self.ended {
                        self.paused = false;
                    }
                    self.restart(position)?;
                }
            }
            Event::Resize(width, height) => {
                self.layout = Layout::new(width, height);
                // A dragged window emits many resize events; only relaunch
                // FFmpeg once the user stops, rather than on every pixel.
                self.resize_at = Some(Instant::now() + Duration::from_millis(120));
            }
            _ => {}
        }
        if old_style != self.style
            && self.path.is_some()
            && self.style.sampling() != self.decoder_sampling
        {
            let position = self
                .audio
                .as_ref()
                .filter(|_| !self.paused)
                .map_or(self.position, Audio::position);
            self.reconfigure_video(position)?;
        }
        Ok(true)
    }

    fn draw(&mut self, grid: &mut Grid) {
        grid.clear();
        let layout = self.layout;
        let (width, height, sidebar) = (layout.width, layout.height, layout.sidebar);
        let plain = Ink::Default;
        grid.text(
            2,
            0,
            &format!(
                "ANSCII / {}{}",
                self.style.label(),
                if self.invert { " / INVERTED" } else { "" }
            ),
            GREEN,
            plain,
            width.saturating_sub(3),
        );
        grid.text(
            2,
            1,
            &self.folder.to_string_lossy(),
            MUTED,
            plain,
            width.saturating_sub(3),
        );
        if width < 35 || height < 10 {
            grid.text(
                1,
                3,
                "Enlarge terminal (35 x 10 minimum)",
                MUTED,
                plain,
                width.saturating_sub(2),
            );
            return;
        }
        if self.message.is_empty() {
            grid.text(2, 2, &format!("T styles: 1 ASCII  2 SPACE  3 BLOCKS  4 DENSE  5 SOLID  6 SHADE | +/- density {}", self.density), MUTED, plain, width - 3);
        } else {
            grid.text(
                2,
                2,
                &self.message,
                Ink::Rgb([240, 193, 102]),
                plain,
                width - 3,
            );
        }
        if sidebar > 0 {
            grid.text(
                2,
                3,
                &format!("LIBRARY  {:02}", self.files.len()),
                GREEN,
                plain,
                sidebar - 3,
            );
            let count = usize::from(height - 7);
            let offset = self.selected.saturating_sub(count.saturating_sub(1));
            for (row, (index, path)) in self
                .files
                .iter()
                .enumerate()
                .skip(offset)
                .take(count)
                .enumerate()
            {
                let label = path
                    .strip_prefix(&self.folder)
                    .unwrap_or(path)
                    .to_string_lossy();
                let marker = if Some(path) == self.path.as_ref() {
                    "> "
                } else {
                    "  "
                };
                let (fg, bg) = if index == self.selected {
                    (Ink::Rgb([0, 0, 0]), GREEN)
                } else {
                    (plain, plain)
                };
                let label = format!("{marker}{label}");
                for x in 1..sidebar - 1 {
                    grid.put(x, row as u16 + 4, ' ', fg, bg);
                }
                grid.text(1, row as u16 + 4, &label, fg, bg, sidebar - 2);
            }
            for y in 3..height - 3 {
                grid.put(sidebar, y, '|', MUTED, plain);
            }
        }
        let title = self
            .path
            .as_ref()
            .or_else(|| self.probe.as_ref().map(|p| &p.path))
            .or_else(|| self.files.get(self.selected))
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy())
            .unwrap_or_else(|| "Your screen is waiting.".into());
        grid.text(
            layout.left(),
            3,
            &title,
            plain,
            plain,
            width - layout.left() - 1,
        );
        if let Some(image) = &self.image {
            let (columns, rows) = self.image_size;
            let x0 = layout.left() + width.saturating_sub(layout.left() + columns + 2) / 2;
            let y0 = 4 + height.saturating_sub(7 + rows) / 2;
            self.painter.sampling = self.image_sampling;
            self.painter.paint(
                grid,
                &image.pixels,
                self.image_size,
                (x0, y0),
                Settings {
                    style: self.style,
                    mode: self.mode,
                    depth: self.depth,
                    density: self.density,
                    invert: self.invert,
                },
            );
        } else if self.path.is_none() && self.probe.is_none() {
            grid.text(
                layout.left(),
                height / 2,
                "SELECT A VIDEO / PRESS ENTER",
                GREEN,
                plain,
                width - layout.left() - 1,
            );
        }
        let status = if self.probe.is_some() {
            "LOADING"
        } else if self.ended {
            "FINISHED"
        } else if self.paused {
            "PAUSED"
        } else if self.path.is_some() {
            "PLAYING"
        } else {
            "READY"
        };
        let duration = if self.metadata.duration > 0.0 {
            timestamp(self.metadata.duration)
        } else {
            "--:--".into()
        };
        let sound = if !self.metadata.has_audio {
            "NO AUDIO".into()
        } else if self.no_audio || self.audio_failed {
            "AUDIO OFF".into()
        } else if self.muted {
            "MUTED".into()
        } else if self.audio_complete {
            "AUDIO END".into()
        } else {
            format!("AUDIO {}%", self.volume)
        };
        grid.text(
            layout.left(),
            height - 3,
            &format!(
                "{status}  {} / {duration}  {} {}  {sound}",
                timestamp(self.position),
                self.style.label(),
                self.mode.label()
            ),
            GREEN,
            plain,
            width - layout.left() - 1,
        );
        if self.metadata.duration > 0.0 {
            let (y, x, length) = layout.timeline();
            let head = ((f64::from(length - 1) * self.position / self.metadata.duration) as u16)
                .min(length - 1);
            for col in 0..length {
                grid.put(
                    x + col,
                    y,
                    if col < head {
                        '='
                    } else if col == head {
                        'o'
                    } else {
                        '-'
                    },
                    GREEN,
                    plain,
                );
            }
        }
        grid.text(1, height - 1, "Space pause  M mute  [ ] volume  Enter play  Arrows/click seek  T style  C color  I invert  Q quit", MUTED, plain, width - 2);
    }
}

fn clamp_seek(position: f64, duration: f64, fps: u16) -> f64 {
    if duration > 0.0 {
        position.clamp(0.0, (duration - 1.0 / f64::from(fps)).max(0.0))
    } else {
        position.max(0.0)
    }
}

fn timestamp(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

struct Terminal;

impl Terminal {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;
        Ok(guard)
    }
}

fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let mut commands = vec![0x18]; // cancel a partially delivered CSI before cleanup
    let _ = crossterm::queue!(
        commands,
        crossterm::terminal::EndSynchronizedUpdate,
        crossterm::style::ResetColor,
        Show,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    presenter::restore(&commands);
}

impl Drop for Terminal {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn run(args: Args) -> io::Result<()> {
    let _terminal = Terminal::enter()?;
    let mut app = App::new(args, terminal::size()?)?;
    let presenter = Presenter::new()?;
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(app.fps));
    let mut next_present = Instant::now();
    let mut dirty = true;
    loop {
        // No busy loop: idle/paused screens block until a key/mouse/resize event.
        let wait = app.wake_after(Instant::now());
        let ready = presenter.ready();
        let wait = if dirty && ready {
            wait.min(next_present.saturating_duration_since(Instant::now()))
        } else if dirty {
            wait.min(Duration::from_millis(8))
        } else {
            wait
        };
        if event::poll(wait)? {
            // Bound event batches, too: holding a key must not starve playback.
            for _ in 0..32 {
                let event = event::read()?;
                // Mouse motion/focus/release reports cannot change this UI.
                // Rebuilding a huge image for each report wastes entire frames.
                let actionable = match &event {
                    Event::Key(key) => key.kind != KeyEventKind::Release,
                    Event::Mouse(mouse) => mouse.kind == MouseEventKind::Down(MouseButton::Left),
                    Event::Resize(..) => true,
                    _ => false,
                };
                if !actionable {
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                    continue;
                }
                let result = app.event(event);
                match result {
                    Ok(false) => return Ok(()),
                    Err(error) => app.message = error.to_string(),
                    Ok(true) => {}
                }
                dirty = true;
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        match app.tick(Instant::now()) {
            Ok(changed) => dirty |= changed,
            Err(error) => {
                app.message = error.to_string();
                app.decoder = None;
                app.audio = None;
                dirty = true;
            }
        }
        let now = Instant::now();
        if dirty
            && now >= next_present
            && let Some(mut grid) = presenter.grid(app.layout.width, app.layout.height)?
        {
            app.draw(&mut grid);
            presenter.submit(grid);
            dirty = false;
            next_present = (next_present + frame_interval).max(now);
        }
    }
}

fn parse_benchmark_size(value: &str) -> Result<(u16, u16), String> {
    let (w, h) = value.split_once('x').ok_or("Expected COLUMNSxROWS")?;
    let width = w.parse::<u16>().map_err(|e| e.to_string())?;
    let height = h.parse::<u16>().map_err(|e| e.to_string())?;
    if !(4..=4096).contains(&width) || !(8..=2048).contains(&height) {
        return Err("Benchmark size must be 4x8 through 4096x2048".into());
    }
    Ok((width, height))
}

fn benchmark(width: u16, height: u16) {
    let mut renderer = Renderer::new();
    let mut grid = Grid::new(width, height);
    for y in 0..height {
        for x in 0..width - 1 {
            grid.put(
                x,
                y,
                '@',
                Ink::Rgb([(x % 256) as u8, ((y * 5) % 256) as u8, 231]),
                Ink::Default,
            );
        }
    }
    renderer.encode(&grid);
    let frames = (4_600_000 / (u32::from(width) * u32::from(height))).clamp(12, 600) as u16;
    for (label, moving) in [
        ("static", false),
        ("sparse motion (10% cells)", true),
        ("full motion", true),
    ] {
        let start = Instant::now();
        let mut bytes = 0;
        for frame in 0..frames {
            if moving {
                for y in 0..height {
                    for x in 0..width - 1 {
                        if label == "full motion" || x % 10 == frame % 10 {
                            grid.put(
                                x,
                                y,
                                '@',
                                Ink::Rgb([
                                    ((x + frame) % 256) as u8,
                                    ((y * 5 + frame) % 256) as u8,
                                    231,
                                ]),
                                Ink::Default,
                            );
                        }
                    }
                }
            }
            bytes += std::hint::black_box(renderer.encode(&grid)).len();
        }
        println!(
            "{label}: {:.3} ms/frame, {} bytes/frame ({width}x{height}, {frames} frames)",
            start.elapsed().as_secs_f64() * 1000.0 / f64::from(frames),
            bytes / usize::from(frames)
        );
    }
}

fn benchmark_techniques(width: u16, height: u16) {
    let mut renderer = Renderer::new();
    let mut grid = Grid::new(width, height);
    let size = (width - 1, height - 3);
    let frames = (4_000_000 / (usize::from(size.0) * usize::from(size.1))).clamp(12, 120);
    let mut painter = Painter::default();
    for style in Style::ALL {
        let (sx, sy) = style.sampling().factors();
        painter.sampling = style.sampling();
        let mut pixels: Vec<u8> = (0..usize::from(size.0) * usize::from(size.1) * sx * sy)
            .flat_map(|i| {
                [
                    (i % 256) as u8,
                    ((i / (usize::from(size.0) * sx)) % 256) as u8,
                    ((i * 7) % 256) as u8,
                ]
            })
            .collect();
        let start = Instant::now();
        for frame in 0..frames {
            let at = frame % pixels.len();
            pixels[at] = pixels[at].wrapping_add(1);
            painter.paint(
                &mut grid,
                std::hint::black_box(&pixels),
                size,
                (0, 0),
                Settings {
                    style,
                    mode: Mode::Color,
                    depth: Depth::Truecolor,
                    density: 128,
                    invert: false,
                },
            );
            std::hint::black_box(renderer.encode(&grid));
        }
        println!(
            "{} technique + diff: {:.3} ms/frame ({}x{} cells, {sx}x{sy} samples, {frames} frames)",
            style.label(),
            start.elapsed().as_secs_f64() * 1000.0 / frames as f64,
            size.0,
            size.1,
        );
    }
}

fn main() {
    let args = Args::parse();
    if args.benchmark {
        benchmark(args.benchmark_size.0, args.benchmark_size.1);
        benchmark_techniques(args.benchmark_size.0, args.benchmark_size.1);
        return;
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("anscii: run in an interactive terminal");
        std::process::exit(1);
    }
    for tool in ["ffmpeg", "ffprobe"] {
        if Command::new(tool)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("anscii: {tool} is required (macOS: brew install ffmpeg)");
            std::process::exit(1);
        }
    }
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original_hook(info);
    }));
    if let Err(error) = run(args) {
        eprintln!("anscii: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for_playback(app: &mut App, stage: &str, ready: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready(app) {
            assert!(
                !app.ended && Instant::now() < deadline,
                "{stage}: position={}, ended={}, message={:?}",
                app.position,
                app.ended,
                app.message
            );
            app.tick(Instant::now()).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn hardware_auto_is_platform_and_codec_aware_and_can_be_disabled() {
        assert_eq!(HardwareMode::Off.backend(true), None);
        assert_eq!(
            HardwareMode::Auto.backend(false),
            if cfg!(target_os = "linux") {
                Some("auto")
            } else {
                None
            }
        );
        assert_eq!(
            HardwareMode::Auto.backend(true),
            if cfg!(target_os = "macos") {
                Some("videotoolbox")
            } else if cfg!(target_os = "linux") {
                Some("auto")
            } else {
                None
            }
        );
        assert_eq!(
            HardwareMode::VideoToolbox.backend(false),
            Some("videotoolbox")
        );
        assert_eq!(HardwareMode::Vaapi.backend(false), Some("vaapi"));
        assert_eq!(HardwareMode::Cuda.backend(false), Some("cuda"));
        assert_eq!(Args::parse_from(["anscii"]).hwaccel, HardwareMode::Auto);
        assert_eq!(
            Args::parse_from(["anscii", "--hwaccel", "off"]).hwaccel,
            HardwareMode::Off
        );
    }

    #[test]
    fn playback_pause_seek_and_catch_up_use_the_media_clock() {
        use crate::test_support::TestVideo;
        if !TestVideo::available() {
            eprintln!("FFmpeg unavailable; skipping integration test");
            return;
        }
        let video = TestVideo::new();
        // Clock/catch-up behavior must not depend on a hosted runner's GPU
        // drivers or the time FFmpeg spends trying unavailable accelerators.
        // Hardware selection and initial-failure fallback have separate tests.
        let args = Args::parse_from([
            "anscii",
            video.folder.to_str().unwrap(),
            "--hwaccel",
            "off",
            "--no-audio",
        ]);
        let mut app = App::new(args, (100, 30)).unwrap();
        app.play().unwrap();
        wait_for_playback(&mut app, "initial decoded frame", |app| app.image.is_some());
        app.paused = true;
        let paused_position = app.position;
        app.tick(Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(app.position, paused_position);
        for digit in ['4', '3', '1'] {
            app.event(Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char(digit),
                KeyModifiers::NONE,
            )))
            .unwrap();
            assert!(app.image.is_some()); // cached preview survives reconfiguration
            wait_for_playback(&mut app, "style preview", |app| !app.refreshing);
            assert_eq!(app.position, paused_position);
            assert_eq!(app.image_sampling, app.style.sampling());
            let (sx, sy) = app.image_sampling.factors();
            assert_eq!(
                app.image.as_ref().unwrap().pixels.len(),
                usize::from(app.image_size.0) * usize::from(app.image_size.1) * sx * sy * 3
            );
        }
        app.restart(1.0).unwrap();
        assert!(app.paused);
        wait_for_playback(&mut app, "seek preview", |app| app.image.is_some());
        assert_eq!(app.position, 1.0);
        app.paused = false;
        app.clock = Some((Instant::now(), 1.0));
        // Simulate 700 ms of terminal/decode delay. A slow-motion player would
        // advance one frame; this one catches up to 1.7 s by dropping stale frames.
        let deadline = Instant::now() + Duration::from_secs(15);
        while app.position < 1.65 && !app.ended && Instant::now() < deadline {
            app.tick(Instant::now() + Duration::from_millis(700))
                .unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            app.position >= 1.65,
            "did not catch up: position={}, message={:?}",
            app.position,
            app.message
        );
        assert_eq!(
            app.image.as_ref().unwrap().pixels.len(),
            usize::from(app.image_size.0) * usize::from(app.image_size.1) * 3
        );
    }

    #[test]
    fn timeline_hit_testing_matches_visible_endpoints() {
        for size in [(100, 30), (60, 20), (180, 48)] {
            let layout = Layout::new(size.0, size.1);
            let (row, left, length) = layout.timeline();
            assert_eq!(layout.seek_at(left, row, 100.0), Some(0.0));
            assert_eq!(layout.seek_at(left + length - 1, row, 100.0), Some(100.0));
            assert_eq!(layout.seek_at(left - 1, row, 100.0), None);
            assert_eq!(layout.seek_at(left, row - 1, 100.0), None);
        }
        assert_eq!(clamp_seek(100.0, 100.0, 24), 100.0 - 1.0 / 24.0);
        assert_eq!(clamp_seek(-5.0, 100.0, 24), 0.0);
    }

    #[test]
    fn exact_color_survives_frame_composition() {
        let args = Args::parse_from(["anscii"]);
        let mut app = App::new(args, (100, 30)).unwrap();
        app.image = Some(Frame {
            pixels: vec![19, 127, 231, 0, 0, 255],
            position: 0.0,
        });
        app.image_size = (2, 1);
        let mut grid = Grid::new(100, 30);
        app.draw(&mut grid);
        assert!(
            grid.cells
                .iter()
                .any(|cell| cell.fg == Ink::Rgb([19, 127, 231]))
        );
        assert!(
            grid.cells
                .iter()
                .any(|cell| cell.fg == Ink::Rgb([0, 0, 255]) && cell.glyph == '@')
        );
    }
}
