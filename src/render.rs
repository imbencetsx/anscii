use unicode_width::UnicodeWidthChar;

pub const RAMP: &[u8] = b" .,:;irsXA253hMHGS#9B&@";
pub const GREEN: Ink = Ink::Rgb([122, 224, 153]);
pub const MUTED: Ink = Ink::Rgb([150, 160, 170]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ink {
    Default,
    Rgb([u8; 3]),
    Indexed(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub glyph: char,
    pub fg: Ink,
    pub bg: Ink,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            glyph: ' ',
            fg: Ink::Default,
            bg: Ink::Default,
        }
    }
}

pub struct Grid {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<Cell>,
}

impl Grid {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::default(); usize::from(width) * usize::from(height)],
        }
    }

    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
    }

    pub fn put(&mut self, x: u16, y: u16, glyph: char, fg: Ink, bg: Ink) {
        // Reserve the final column to avoid terminal autowrap/scrolling.
        if x < self.width.saturating_sub(1) && y < self.height {
            self.cells[usize::from(y) * usize::from(self.width) + usize::from(x)] =
                Cell { glyph, fg, bg };
        }
    }

    pub fn text(&mut self, x: u16, y: u16, text: &str, fg: Ink, bg: Ink, limit: u16) {
        for (offset, glyph) in text.chars().take(usize::from(limit)).enumerate() {
            let glyph = if glyph.width() == Some(1) && !glyph.is_control() {
                glyph
            } else {
                '?'
            };
            self.put(x.saturating_add(offset as u16), y, glyph, fg, bg);
        }
    }
}

/// One screen model, one reusable output buffer, and no erase/repaint pass.
/// Unchanged frames produce zero bytes (including when paused).
pub struct Renderer {
    previous: Vec<Cell>,
    size: (u16, u16),
    output: Vec<u8>,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            previous: Vec::new(),
            size: (0, 0),
            output: Vec::with_capacity(256 * 1024),
        }
    }

    pub fn encode(&mut self, grid: &Grid) -> &[u8] {
        self.output.clear();
        let full = self.size != (grid.width, grid.height);
        if full {
            self.size = (grid.width, grid.height);
            self.previous.resize(grid.cells.len(), Cell::default());
            self.output.extend_from_slice(b"\x1b[?2026h\x1b[0m\x1b[2J");
        }
        let mut fg = Ink::Default;
        let mut bg = Ink::Default;
        let mut next_cursor = None;
        for (index, &cell) in grid.cells.iter().enumerate() {
            if (!full && self.previous[index] == cell) || (full && cell == Cell::default()) {
                continue;
            }
            if self.output.is_empty() {
                self.output.extend_from_slice(b"\x1b[?2026h");
            }
            if next_cursor != Some(index) {
                self.output.extend_from_slice(b"\x1b[");
                number(
                    &mut self.output,
                    (index / usize::from(grid.width) + 1) as u16,
                );
                self.output.push(b';');
                number(
                    &mut self.output,
                    (index % usize::from(grid.width) + 1) as u16,
                );
                self.output.push(b'H');
            }
            if fg != cell.fg {
                color(&mut self.output, cell.fg, false);
                fg = cell.fg;
            }
            if bg != cell.bg {
                color(&mut self.output, cell.bg, true);
                bg = cell.bg;
            }
            let mut utf8 = [0; 4];
            self.output
                .extend_from_slice(cell.glyph.encode_utf8(&mut utf8).as_bytes());
            self.previous[index] = cell;
            next_cursor = Some(index + 1);
        }
        if full {
            self.previous.copy_from_slice(&grid.cells);
        }
        if !self.output.is_empty() {
            self.output.extend_from_slice(b"\x1b[0m\x1b[?2026l");
        }
        &self.output
    }
}

fn number(out: &mut Vec<u8>, n: u16) {
    let mut digits = [0; 5];
    let mut value = n;
    let mut start = 5;
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[start..]);
}

fn color(out: &mut Vec<u8>, ink: Ink, background: bool) {
    match ink {
        Ink::Default => out.extend_from_slice(if background { b"\x1b[49m" } else { b"\x1b[39m" }),
        Ink::Rgb(rgb) => {
            out.extend_from_slice(if background {
                b"\x1b[48;2;"
            } else {
                b"\x1b[38;2;"
            });
            for (i, channel) in rgb.into_iter().enumerate() {
                if i != 0 {
                    out.push(b';');
                }
                number(out, u16::from(channel));
            }
            out.push(b'm');
        }
        Ink::Indexed(index) => {
            out.extend_from_slice(if background {
                b"\x1b[48;5;"
            } else {
                b"\x1b[38;5;"
            });
            number(out, u16::from(index));
            out.push(b'm');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_frames_write_nothing_and_updates_are_small() {
        let mut renderer = Renderer::new();
        let mut grid = Grid::new(160, 48);
        grid.put(40, 20, '@', Ink::Rgb([19, 127, 231]), Ink::Default);
        let initial = String::from_utf8_lossy(renderer.encode(&grid)).into_owned();
        assert!(initial.contains("38;2;19;127;231m@"));
        assert!(renderer.encode(&grid).is_empty());
        grid.put(41, 20, 'X', Ink::Rgb([0, 0, 255]), Ink::Default);
        let diff = renderer.encode(&grid);
        assert!(diff.len() < 80);
        assert!(!String::from_utf8_lossy(diff).contains("2J"));
        grid.clear();
        assert!(!renderer.encode(&grid).is_empty());
        assert!(renderer.encode(&grid).is_empty());
    }

    #[test]
    fn resizing_invalidates_the_old_screen() {
        let mut renderer = Renderer::new();
        renderer.encode(&Grid::new(160, 48));
        assert!(String::from_utf8_lossy(renderer.encode(&Grid::new(80, 24))).contains("2J"));
    }
}
