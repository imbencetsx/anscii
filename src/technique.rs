//! Image techniques share 2x4 samples per cell, so changing styles never seeks
//! or restarts FFmpeg. Scratch storage is reused across frames.
use crate::{
    Depth, Mode,
    render::{Cell, Grid, Ink, RAMP},
};
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum Style {
    Ascii,
    Whitespace,
    Blocks,
    Braille,
    Edges,
    Dither,
}

impl Style {
    pub const ALL: [Self; 6] = [
        Self::Ascii,
        Self::Whitespace,
        Self::Blocks,
        Self::Braille,
        Self::Edges,
        Self::Dither,
    ];
    pub fn next(self) -> Self {
        Self::ALL[(self as usize + 1) % Self::ALL.len()]
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Ascii => "ASCII",
            Self::Whitespace => "SPACE",
            Self::Blocks => "BLOCKS",
            Self::Braille => "BRAILLE",
            Self::Edges => "EDGES",
            Self::Dither => "DITHER",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Settings {
    pub style: Style,
    pub mode: Mode,
    pub depth: Depth,
    pub threshold: u8,
    pub invert: bool,
}

impl Settings {
    fn level(self, rgb: [u8; 3]) -> u8 {
        if self.mode == Mode::Color {
            *rgb.iter().max().unwrap()
        } else {
            luma(rgb)
        }
    }

    fn ink(self, rgb: [u8; 3], shaded: bool) -> Ink {
        let value = if shaded { luma(rgb) } else { 255 };
        let rgb = match self.mode {
            Mode::Color => rgb,
            Mode::White => [value; 3],
            Mode::Amber => [
                value,
                (u16::from(value) * 190 / 255) as u8,
                (u16::from(value) * 85 / 255) as u8,
            ],
        };
        match self.depth {
            Depth::Truecolor => Ink::Rgb(rgb),
            Depth::Palette => Ink::Indexed(
                16 + ((u16::from(rgb[0]) + 25) / 51 * 36
                    + (u16::from(rgb[1]) + 25) / 51 * 6
                    + (u16::from(rgb[2]) + 25) / 51) as u8,
            ),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Sample {
    average: [u8; 3],
    luma: u8,
}

fn luma(rgb: [u8; 3]) -> u8 {
    ((u32::from(rgb[0]) * 54 + u32::from(rgb[1]) * 183 + u32::from(rgb[2]) * 19) / 256) as u8
}

fn source_rgb(pixels: &[u8], stride: usize, x: usize, y: usize, invert: bool) -> [u8; 3] {
    let offset = (y * stride + x) * 3;
    let rgb = [pixels[offset], pixels[offset + 1], pixels[offset + 2]];
    if invert { rgb.map(|v| 255 - v) } else { rgb }
}

fn source_average(
    pixels: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    rows: std::ops::Range<usize>,
    invert: bool,
) -> [u8; 3] {
    let count = (rows.end - rows.start) as u16 * 2;
    let mut sum = [0_u16; 3];
    for dy in rows {
        for dx in 0..2 {
            let offset = ((y * 4 + dy) * stride + x * 2 + dx) * 3;
            for channel in 0..3 {
                sum[channel] += u16::from(pixels[offset + channel]);
            }
        }
    }
    sum.map(|v| (if invert { count * 255 - v } else { v } / count) as u8)
}

#[derive(Default)]
pub struct Painter {
    samples: Vec<Sample>,
}

impl Painter {
    pub fn paint(
        &mut self,
        grid: &mut Grid,
        pixels: &[u8],
        size: (u16, u16),
        origin: (u16, u16),
        settings: Settings,
    ) {
        let (columns, rows) = (usize::from(size.0), usize::from(size.1));
        debug_assert_eq!(pixels.len(), columns * rows * 8 * 3);
        // Subpixel techniques read the shared source directly. Other styles
        // need only one RGB average + luminance (4 bytes/cell, formerly 34).
        if !matches!(settings.style, Style::Blocks | Style::Braille) {
            self.samples.resize(columns * rows, Sample::default());
            for y in 0..rows {
                for x in 0..columns {
                    let rgb = source_average(pixels, columns * 2, x, y, 0..4, settings.invert);
                    self.samples[y * columns + x] = Sample {
                        average: rgb,
                        luma: luma(rgb),
                    };
                }
            }
        }
        for y in 0..rows.min(usize::from(grid.height.saturating_sub(origin.1 + 3))) {
            for x in 0..columns.min(usize::from(grid.width.saturating_sub(origin.0 + 1))) {
                let cell = self.cell(x, y, columns, rows, settings, pixels);
                grid.put(
                    origin.0 + x as u16,
                    origin.1 + y as u16,
                    cell.glyph,
                    cell.fg,
                    cell.bg,
                );
            }
        }
    }

    fn cell(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        settings: Settings,
        pixels: &[u8],
    ) -> Cell {
        let sample = if matches!(settings.style, Style::Blocks | Style::Braille) {
            Sample::default()
        } else {
            self.samples[y * width + x]
        };
        let foreground = settings.ink(sample.average, false);
        let black = Ink::Rgb([0, 0, 0]);
        let (glyph, fg, bg) = match settings.style {
            Style::Ascii => {
                let glyph = char::from(
                    RAMP[usize::from(settings.level(sample.average)) * (RAMP.len() - 1) / 255],
                );
                (
                    glyph,
                    if glyph == ' ' {
                        Ink::Default
                    } else {
                        foreground
                    },
                    Ink::Default,
                )
            }
            // The entire image consists of spaces; the background carries color.
            Style::Whitespace => (' ', Ink::Default, settings.ink(sample.average, true)),
            // Two independent vertical pixels per character, no fake block ramp.
            Style::Blocks => (
                '▀',
                settings.ink(
                    source_average(pixels, width * 2, x, y, 0..2, settings.invert),
                    true,
                ),
                settings.ink(
                    source_average(pixels, width * 2, x, y, 2..4, settings.invert),
                    true,
                ),
            ),
            Style::Braille => {
                // Row-major samples -> Unicode's 1,4 / 2,5 / 3,6 / 7,8 bits.
                const BITS: [u8; 8] = [0, 3, 1, 4, 2, 5, 6, 7];
                let mut mask = 0_u8;
                let mut sum = [0_u16; 3];
                let mut count = 0_u16;
                for (i, bit) in BITS.into_iter().enumerate() {
                    let rgb = source_rgb(
                        pixels,
                        width * 2,
                        x * 2 + i % 2,
                        y * 4 + i / 2,
                        settings.invert,
                    );
                    if settings.level(rgb) >= settings.threshold {
                        mask |= 1 << bit;
                        for c in 0..3 {
                            sum[c] += u16::from(rgb[c]);
                        }
                        count += 1;
                    }
                }
                if mask == 0 {
                    (' ', Ink::Default, black)
                } else {
                    (
                        char::from_u32(0x2800 + u32::from(mask)).unwrap(),
                        settings.ink(sum.map(|v| (v / count) as u8), false),
                        black,
                    )
                }
            }
            Style::Edges => {
                let mut levels = [[0_i32; 3]; 3];
                let mut brightest = sample.average;
                let mut maximum_luma = sample.luma;
                for (dy, row) in levels.iter_mut().enumerate() {
                    for (dx, value) in row.iter_mut().enumerate() {
                        let nx =
                            (x as isize + dx as isize - 1).clamp(0, width as isize - 1) as usize;
                        let ny =
                            (y as isize + dy as isize - 1).clamp(0, height as isize - 1) as usize;
                        let neighbor = &self.samples[ny * width + nx];
                        *value = i32::from(neighbor.luma);
                        if neighbor.luma > maximum_luma {
                            maximum_luma = neighbor.luma;
                            brightest = neighbor.average;
                        }
                    }
                }
                let gx = -levels[0][0] + levels[0][2] - 2 * levels[1][0] + 2 * levels[1][2]
                    - levels[2][0]
                    + levels[2][2];
                let gy = -levels[0][0] - 2 * levels[0][1] - levels[0][2]
                    + levels[2][0]
                    + 2 * levels[2][1]
                    + levels[2][2];
                let glyph = if (gx.abs() + gy.abs()) / 4 < i32::from(settings.threshold.max(1)) {
                    ' '
                } else if gx.abs() > gy.abs() * 2 {
                    '|'
                } else if gy.abs() > gx.abs() * 2 {
                    '-'
                } else if gx * gy >= 0 {
                    '/'
                } else {
                    '\\'
                };
                (
                    glyph,
                    if glyph == ' ' {
                        Ink::Default
                    } else {
                        settings.ink(brightest, false)
                    },
                    black,
                )
            }
            Style::Dither => {
                const BAYER: [[i16; 4]; 4] =
                    [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
                let cutoff = (BAYER[y % 4][x % 4] * 16 + 8 + i16::from(settings.threshold) - 128)
                    .clamp(0, 254);
                let glyph = if i16::from(settings.level(sample.average)) > cutoff {
                    '•'
                } else {
                    ' '
                };
                (
                    glyph,
                    if glyph == ' ' {
                        Ink::Default
                    } else {
                        foreground
                    },
                    black,
                )
            }
        };
        Cell { glyph, fg, bg }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_sampler_matches_direct_rgb_averages_including_inversion() {
        let pixels: Vec<u8> = (0..4 * 4 * 3).map(|i| ((i * 37) % 256) as u8).collect();
        for invert in [false, true] {
            for x in 0..2 {
                let mut sum = [0_u16; 3];
                for dy in 0..4 {
                    for dx in 0..2 {
                        for (channel, value) in sum.iter_mut().enumerate() {
                            let byte = pixels[(dy * 4 + x * 2 + dx) * 3 + channel];
                            *value += u16::from(if invert { 255 - byte } else { byte });
                        }
                    }
                }
                assert_eq!(
                    source_average(&pixels, 4, x, 0, 0..4, invert),
                    sum.map(|v| (v / 8) as u8)
                );
            }
        }
        assert_eq!(std::mem::size_of::<Sample>(), 4);
    }

    fn settings(style: Style) -> Settings {
        Settings {
            style,
            mode: Mode::Color,
            depth: Depth::Truecolor,
            threshold: 128,
            invert: false,
        }
    }

    #[test]
    fn whitespace_uses_only_spaces_with_exact_background_color() {
        let mut painter = Painter::default();
        let mut grid = Grid::new(4, 6);
        painter.paint(
            &mut grid,
            &[19, 127, 231].repeat(8),
            (1, 1),
            (0, 0),
            settings(Style::Whitespace),
        );
        assert_eq!(grid.cells[0].glyph, ' ');
        assert_eq!(grid.cells[0].bg, Ink::Rgb([19, 127, 231]));
    }

    #[test]
    fn block_preserves_separate_top_and_bottom_colors() {
        let mut pixels = [255, 0, 0].repeat(4);
        pixels.extend([0, 0, 255].repeat(4));
        let mut grid = Grid::new(4, 6);
        Painter::default().paint(&mut grid, &pixels, (1, 1), (0, 0), settings(Style::Blocks));
        assert_eq!(
            grid.cells[0],
            Cell {
                glyph: '▀',
                fg: Ink::Rgb([255, 0, 0]),
                bg: Ink::Rgb([0, 0, 255])
            }
        );
    }

    #[test]
    fn braille_maps_all_eight_subpixels_to_the_correct_dot() {
        for (pixel, bit) in [0, 3, 1, 4, 2, 5, 6, 7].into_iter().enumerate() {
            let mut pixels = vec![0; 24];
            pixels[pixel * 3..pixel * 3 + 3].fill(255);
            let mut grid = Grid::new(4, 6);
            Painter::default().paint(&mut grid, &pixels, (1, 1), (0, 0), settings(Style::Braille));
            assert_eq!(grid.cells[0].glyph as u32, 0x2800 + (1 << bit));
        }
    }

    #[test]
    fn dither_preserves_half_tone_coverage_and_is_deterministic() {
        let mut grid = Grid::new(5, 7);
        let mut painter = Painter::default();
        let pixels = vec![128; 4 * 4 * 8 * 3];
        painter.paint(&mut grid, &pixels, (4, 4), (0, 0), settings(Style::Dither));
        assert_eq!(grid.cells.iter().filter(|c| c.glyph == '•').count(), 8);
        let first = grid.cells.clone();
        painter.paint(&mut grid, &pixels, (4, 4), (0, 0), settings(Style::Dither));
        assert_eq!(first, grid.cells);
    }

    #[test]
    fn edges_detect_orientation_and_inversion_flips_empty_braille() {
        let mut painter = Painter::default();
        let mut grid = Grid::new(5, 6);
        let pixels: Vec<_> = (0..4)
            .flat_map(|_| (0..6).flat_map(|x| [if x < 2 { 0 } else { 255 }; 3]))
            .collect();
        painter.paint(&mut grid, &pixels, (3, 1), (0, 0), settings(Style::Edges));
        assert_eq!(grid.cells[1].glyph, '|');
        let mut inverted = settings(Style::Braille);
        inverted.invert = true;
        painter.paint(&mut grid, &[0; 24], (1, 1), (0, 0), inverted);
        assert_eq!(grid.cells[0].glyph, '⣿');
    }
}
