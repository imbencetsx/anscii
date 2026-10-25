//! Techniques decode only the samples they can display. Switching styles can
//! preview the previous sampling immediately while a new decoder warms up.
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
    Dense,
    Solid,
    Shade,
}

impl Style {
    pub const ALL: [Self; 6] = [
        Self::Ascii,
        Self::Whitespace,
        Self::Blocks,
        Self::Dense,
        Self::Solid,
        Self::Shade,
    ];
    pub fn next(self) -> Self {
        Self::ALL[(self as usize + 1) % Self::ALL.len()]
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Ascii => "ASCII",
            Self::Whitespace => "SPACE",
            Self::Blocks => "BLOCKS",
            Self::Dense => "DENSE",
            Self::Solid => "SOLID",
            Self::Shade => "SHADE",
        }
    }
    pub fn sampling(self) -> Sampling {
        match self {
            Self::Blocks => Sampling::Half,
            _ => Sampling::Cell,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sampling {
    #[default]
    Cell,
    Half,
}

impl Sampling {
    pub fn factors(self) -> (usize, usize) {
        match self {
            Self::Cell => (1, 1),
            Self::Half => (1, 2),
        }
    }
    pub fn pixels(self, size: (u16, u16)) -> (u16, u16) {
        let (x, y) = self.factors();
        (
            size.0.saturating_mul(x as u16),
            size.1.saturating_mul(y as u16),
        )
    }
}

#[derive(Clone, Copy)]
pub struct Settings {
    pub style: Style,
    pub mode: Mode,
    pub depth: Depth,
    pub density: u8,
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

fn luma(rgb: [u8; 3]) -> u8 {
    ((u32::from(rgb[0]) * 54 + u32::from(rgb[1]) * 183 + u32::from(rgb[2]) * 19) / 256) as u8
}

fn source_rgb(pixels: &[u8], stride: usize, x: usize, y: usize, invert: bool) -> [u8; 3] {
    let offset = (y * stride + x) * 3;
    let rgb = [pixels[offset], pixels[offset + 1], pixels[offset + 2]];
    if invert { rgb.map(|v| 255 - v) } else { rgb }
}

#[derive(Default)]
pub struct Painter {
    samples: Vec<[u8; 3]>,
    pub sampling: Sampling,
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
        let (sx, sy) = self.sampling.factors();
        debug_assert_eq!(pixels.len(), columns * rows * sx * sy * 3);
        // A compact linear preparation pass vectorizes better than branching
        // on source layout inside each full-cell glyph conversion.
        if !matches!(settings.style, Style::Blocks) {
            self.samples.resize(columns * rows, [0; 3]);
            for y in 0..rows {
                for x in 0..columns {
                    let rgb = if self.sampling == Sampling::Cell {
                        source_rgb(pixels, columns, x, y, settings.invert)
                    } else {
                        self.average(pixels, columns, x, y, false, settings.invert)
                    };
                    self.samples[y * columns + x] = rgb;
                }
            }
        }
        for y in 0..rows.min(usize::from(grid.height.saturating_sub(origin.1 + 3))) {
            for x in 0..columns.min(usize::from(grid.width.saturating_sub(origin.0 + 1))) {
                let cell = self.cell(x, y, columns, settings, pixels);
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

    fn average(
        &self,
        pixels: &[u8],
        width: usize,
        x: usize,
        y: usize,
        bottom: bool,
        invert: bool,
    ) -> [u8; 3] {
        match self.sampling {
            Sampling::Cell => source_rgb(pixels, width, x, y, invert),
            Sampling::Half => {
                if bottom {
                    source_rgb(pixels, width, x, y * 2 + 1, invert)
                } else {
                    let a = source_rgb(pixels, width, x, y * 2, invert);
                    let b = source_rgb(pixels, width, x, y * 2 + 1, invert);
                    std::array::from_fn(|c| ((u16::from(a[c]) + u16::from(b[c])) / 2) as u8)
                }
            }
        }
    }

    fn cell(&self, x: usize, y: usize, width: usize, settings: Settings, pixels: &[u8]) -> Cell {
        let sample = if matches!(settings.style, Style::Blocks) {
            [0; 3]
        } else {
            self.samples[y * width + x]
        };
        let foreground = settings.ink(sample, false);
        let (glyph, fg, bg) = match settings.style {
            Style::Ascii | Style::Dense | Style::Solid | Style::Shade => {
                let level = (i16::from(settings.level(sample)) + i16::from(settings.density) - 128)
                    .clamp(0, 255) as usize;
                let glyph = match settings.style {
                    Style::Ascii => char::from(RAMP[level * (RAMP.len() - 1) / 255]),
                    Style::Dense => {
                        // Skip sparse punctuation for fuller midtones; keep true black empty.
                        const DENSE: &[u8] = b" .oOQ0#%@";
                        if level == 0 {
                            ' '
                        } else {
                            char::from(DENSE[2 + level * (DENSE.len() - 3) / 255])
                        }
                    }
                    Style::Solid => {
                        if level == 0 {
                            ' '
                        } else {
                            '@'
                        }
                    }
                    Style::Shade => [' ', '░', '▒', '▓', '█'][level * 4 / 255],
                    _ => unreachable!(),
                };
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
            Style::Whitespace => (' ', Ink::Default, settings.ink(sample, true)),
            // Two independent vertical pixels per character, no fake block ramp.
            Style::Blocks => (
                '▀',
                settings.ink(
                    source_rgb(
                        pixels,
                        width,
                        x,
                        y * self.sampling.factors().1,
                        settings.invert,
                    ),
                    true,
                ),
                settings.ink(
                    self.average(pixels, width, x, y, true, settings.invert),
                    true,
                ),
            ),
        };
        Cell { glyph, fg, bg }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(style: Style) -> Settings {
        Settings {
            style,
            mode: Mode::Color,
            depth: Depth::Truecolor,
            density: 128,
            invert: false,
        }
    }

    fn paint(rgb: [u8; 3], settings: Settings) -> Cell {
        let mut grid = Grid::new(4, 6);
        Painter::default().paint(&mut grid, &rgb, (1, 1), (0, 0), settings);
        grid.cells[0]
    }

    #[test]
    fn styles_accept_native_and_cached_preview_sampling() {
        for sampling in [Sampling::Cell, Sampling::Half] {
            let (sx, sy) = sampling.factors();
            let pixels: Vec<_> = (0..4 * 4 * sx * sy * 3)
                .map(|i| (i * 37 % 256) as u8)
                .collect();
            let mut grid = Grid::new(5, 7);
            let mut painter = Painter {
                sampling,
                ..Painter::default()
            };
            for style in Style::ALL {
                painter.paint(&mut grid, &pixels, (4, 4), (0, 0), settings(style));
            }
        }
    }

    #[test]
    fn character_styles_preserve_rgb_and_offer_fuller_glyphs() {
        for style in [Style::Ascii, Style::Dense, Style::Solid, Style::Shade] {
            let cell = paint([19, 127, 231], settings(style));
            assert_eq!(cell.fg, Ink::Rgb([19, 127, 231]));
            assert_eq!(cell.bg, Ink::Default);
            assert_eq!(paint([0; 3], settings(style)).glyph, ' ');
        }
        assert_eq!(
            paint([128; 3], settings(Style::Ascii)).glyph,
            char::from(RAMP[128 * (RAMP.len() - 1) / 255])
        );
        assert_eq!(paint([128; 3], settings(Style::Dense)).glyph, '0');
        assert_eq!(paint([128; 3], settings(Style::Solid)).glyph, '@');
        assert_eq!(paint([128; 3], settings(Style::Shade)).glyph, '▒');
        assert_eq!(paint([255; 3], settings(Style::Shade)).glyph, '█');
    }

    #[test]
    fn density_and_inversion_change_glyphs_without_recoloring() {
        let mut adjusted = settings(Style::Shade);
        adjusted.density = 192;
        assert_eq!(paint([128; 3], adjusted).glyph, '▓');
        assert_eq!(paint([128; 3], adjusted).fg, Ink::Rgb([128; 3]));
        adjusted.invert = true;
        assert_eq!(paint([0; 3], adjusted).fg, Ink::Rgb([255; 3]));
        for mode in [Mode::White, Mode::Amber] {
            for depth in [Depth::Truecolor, Depth::Palette] {
                for style in Style::ALL {
                    paint(
                        [19, 127, 231],
                        Settings {
                            mode,
                            depth,
                            ..settings(style)
                        },
                    );
                }
            }
        }
    }

    #[test]
    fn whitespace_and_blocks_preserve_background_and_half_colors() {
        assert_eq!(
            paint([19, 127, 231], settings(Style::Whitespace)).bg,
            Ink::Rgb([19, 127, 231])
        );
        let mut painter = Painter {
            sampling: Sampling::Half,
            ..Painter::default()
        };
        let mut grid = Grid::new(4, 6);
        painter.paint(
            &mut grid,
            &[255, 0, 0, 0, 0, 255],
            (1, 1),
            (0, 0),
            settings(Style::Blocks),
        );
        assert_eq!(
            grid.cells[0],
            Cell {
                glyph: '▀',
                fg: Ink::Rgb([255, 0, 0]),
                bg: Ink::Rgb([0, 0, 255])
            }
        );
        painter.paint(
            &mut grid,
            &[255, 0, 0, 0, 0, 255],
            (1, 1),
            (0, 0),
            settings(Style::Whitespace),
        );
        assert_eq!(grid.cells[0].bg, Ink::Rgb([127, 0, 127]));
    }
}
