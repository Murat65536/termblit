use std::io;

use crossterm::{
    cursor::{MoveRight, MoveTo},
    queue,
    style::{Color, Print, ResetColor},
};

use super::{
    Frame, Layout,
    parallel::{available_threads, split_across},
    start_row,
};

// Source pixels per cell in the character modes, roughly square
pub const CELL_COLUMNS: usize = 2;
pub const CELL_ROWS: usize = 4;
const CELL_PIXELS: usize = CELL_COLUMNS * CELL_ROWS;

// Masks over a cell's pixels: bit (row * 2 + column) is set where the
// character draws in the foreground color
const UPPER_LEFT: u8 = 0b0000_0101;
const UPPER_RIGHT: u8 = 0b0000_1010;
const LOWER_LEFT: u8 = 0b0101_0000;
const LOWER_RIGHT: u8 = 0b1010_0000;
const BOTTOM_ROW: u8 = 0b1100_0000;
const BOTTOM_THREE_ROWS: u8 = 0b1111_1100;

// Each also covers its inverse by swapping colors, so no full block is needed
const SYMBOLS: [(char, u8); 17] = [
    (' ', 0),
    ('▘', UPPER_LEFT),
    ('▝', UPPER_RIGHT),
    ('▖', LOWER_LEFT),
    ('▗', LOWER_RIGHT),
    ('▀', UPPER_LEFT | UPPER_RIGHT),
    ('▄', LOWER_LEFT | LOWER_RIGHT),
    ('▌', UPPER_LEFT | LOWER_LEFT),
    ('▐', UPPER_RIGHT | LOWER_RIGHT),
    ('▚', UPPER_LEFT | LOWER_RIGHT),
    ('▞', UPPER_RIGHT | LOWER_LEFT),
    ('▛', UPPER_LEFT | UPPER_RIGHT | LOWER_LEFT),
    ('▜', UPPER_LEFT | UPPER_RIGHT | LOWER_RIGHT),
    ('▙', UPPER_LEFT | LOWER_LEFT | LOWER_RIGHT),
    ('▟', UPPER_RIGHT | LOWER_LEFT | LOWER_RIGHT),
    ('▂', BOTTOM_ROW),
    ('▆', BOTTOM_THREE_ROWS),
];

// A cell must improve on a flat color by this much squared error before
// it's drawn with a shape, so near-flat areas stay cheap spaces
const MIN_SHAPE_IMPROVEMENT: f32 = 64.0;

#[cfg(test)]
pub fn symbol_mask(symbol: char) -> Option<u8> {
    SYMBOLS
        .iter()
        .find(|&&(c, _)| c == symbol)
        .map(|&(_, mask)| mask)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellSymbol {
    pub symbol: char,
    pub foreground: [u8; 3],
    pub background: [u8; 3],
}

// Multiplying by these divides by a region's pixel count
const RECIPROCALS: [f32; CELL_PIXELS + 1] = {
    let mut table = [0.0; CELL_PIXELS + 1];
    let mut count = 1;
    while count <= CELL_PIXELS {
        table[count] = 1.0 / count as f32;
        count += 1;
    }
    table
};

// A pixel's channels in 16-bit lanes, so sums of a cell's pixels add all
// three at once without overflowing
fn pack([r, g, b]: [u8; 3]) -> u64 {
    u64::from(r) | u64::from(g) << 16 | u64::from(b) << 32
}

fn unpack(lanes: u64) -> [u32; 3] {
    [0, 16, 32].map(|shift| (lanes >> shift & 0xffff) as u32)
}

// Only pixels in `present` take part, for cells at the frame's edge
pub fn best_symbol(pixels: &[[u8; 3]; CELL_PIXELS], present: u8) -> CellSymbol {
    let pixels: [u64; CELL_PIXELS] = std::array::from_fn(|bit| {
        if present & (1 << bit) != 0 {
            pack(pixels[bit])
        } else {
            0
        }
    });
    // The sums of every subset of each half's four pixels, so a region's sum
    // is two lookups
    let half_sums = |half: &[u64]| {
        let mut sums = [0; 16];
        for subset in 1..16usize {
            sums[subset] = sums[subset & (subset - 1)] + half[subset.trailing_zeros() as usize];
        }
        sums
    };
    let (top, bottom) = (half_sums(&pixels[..4]), half_sums(&pixels[4..]));
    let sum = |mask: u8| top[usize::from(mask & 0xf)] + bottom[usize::from(mask >> 4)];
    let mean =
        |sum: u64, count: u32| unpack(sum).map(|s| ((2 * s + count) / (2 * count).max(1)) as u8);
    // Squared error is constant minus sum^2 / count for each region, so the
    // best split maximizes this
    let score = |sum: u64, count: u32| {
        let [r, g, b] = unpack(sum);
        (r * r + g * g + b * b) as f32 * RECIPROCALS[count as usize]
    };

    let (total, total_count) = (sum(present), present.count_ones());
    let mut best_score = score(total, total_count) + MIN_SHAPE_IMPROVEMENT;
    // Chosen without branches, which random-looking cells would mispredict
    let mut best = 0;
    for (index, &(_, mask)) in SYMBOLS.iter().enumerate().skip(1) {
        let inside_mask = mask & present;
        let inside_count = inside_mask.count_ones();
        let outside_count = total_count - inside_count;
        let inside = sum(inside_mask);
        let candidate = if inside_count == 0 || outside_count == 0 {
            f32::NEG_INFINITY
        } else {
            score(inside, inside_count) + score(total - inside, outside_count)
        };
        let better = candidate > best_score;
        best_score = if better { candidate } else { best_score };
        best = if better { index } else { best };
    }

    let (symbol, mask) = SYMBOLS[best];
    let inside_mask = if best == 0 { present } else { mask & present };
    let (inside, inside_count) = (sum(inside_mask), inside_mask.count_ones());
    let (outside, outside_count) = if best == 0 {
        (total, total_count)
    } else {
        (total - inside, total_count - inside_count)
    };
    CellSymbol {
        symbol,
        foreground: mean(inside, inside_count),
        background: mean(outside, outside_count),
    }
}

// A cell as the terminal shows it: the symbol, its foreground color unless
// it's a space, and the background color
pub type SymbolCell = (char, Option<Color>, Color);

// Picks the cells of a frame to draw. Generic, so drawing them all costs no
// more than it would without a filter.
pub trait CellFilter<C> {
    // Every cell is drawn, in order, so the cursor needn't be moved between them
    const DRAWS_ALL: bool;
    // Called before each frame with its size in cells
    fn start(&mut self, extent: (usize, usize));
    // Whether to draw the cell at this index, counting row by row
    fn changed(&mut self, index: usize, cell: C) -> bool;
}

pub struct AllCells;

impl<C> CellFilter<C> for AllCells {
    const DRAWS_ALL: bool = true;

    fn start(&mut self, _: (usize, usize)) {}

    fn changed(&mut self, _: usize, _: C) -> bool {
        true
    }
}

// The cells on screen, so only those that change are drawn. Only for frames
// drawn from the top of the screen.
pub struct ChangedCells<C> {
    // Row by row; empty when the screen doesn't show the last frame
    shown: Vec<C>,
    extent: (usize, usize),
}

impl<C> Default for ChangedCells<C> {
    fn default() -> Self {
        Self {
            shown: Vec::new(),
            extent: (0, 0),
        }
    }
}

impl<C> ChangedCells<C> {
    // Draw every cell of the next frame
    pub fn forget(&mut self) {
        self.shown.clear();
    }
}

impl<C: Copy + PartialEq> CellFilter<C> for ChangedCells<C> {
    const DRAWS_ALL: bool = false;

    fn start(&mut self, extent: (usize, usize)) {
        if extent != self.extent {
            self.forget();
            self.extent = extent;
        }
    }

    fn changed(&mut self, index: usize, cell: C) -> bool {
        match self.shown.get_mut(index) {
            Some(shown) if *shown == cell => false,
            Some(shown) => {
                *shown = cell;
                true
            }
            None => {
                debug_assert_eq!(index, self.shown.len());
                self.shown.push(cell);
                true
            }
        }
    }
}

// Where the next character prints, so the cursor is moved only past cells
// that aren't drawn
struct Cursor {
    position: Option<(usize, usize)>,
    screen_columns: usize,
}

impl Cursor {
    fn new(screen_columns: usize) -> Self {
        Self {
            position: None,
            screen_columns,
        }
    }

    fn go_to(&mut self, col: usize, row: usize, out: &mut Vec<u8>) -> io::Result<()> {
        match self.position {
            Some(position) if position == (col, row) => Ok(()),
            Some((from, on)) if on == row && from < col => {
                queue!(out, MoveRight((col - from) as u16))
            }
            _ => queue!(out, MoveTo(col as u16, row as u16)),
        }
    }

    // Printing in the last column leaves the cursor waiting to wrap, so where
    // the next character goes isn't known
    fn printed(&mut self, col: usize, row: usize) {
        self.position = (col + 1 < self.screen_columns).then_some((col + 1, row));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    Truecolor,
    // The 240 colors of the xterm palette that don't depend on the theme
    Palette,
}

// The cell's pixels and a mask of those inside the frame
fn cell_pixels(frame: Frame, col: usize, row: usize) -> ([[u8; 3]; CELL_PIXELS], u8) {
    let width = frame.width as usize;
    let (left, top) = (col * CELL_COLUMNS, row * CELL_ROWS);
    let columns = CELL_COLUMNS.min(width - left);
    let rows = CELL_ROWS.min(frame.height as usize - top);
    let mut pixels = [[0; 3]; CELL_PIXELS];
    let mut present = 0;
    for dy in 0..rows {
        let start = ((top + dy) * width + left) * 4;
        let row = frame.pixels[start..start + columns * 4].as_chunks::<4>().0;
        for (dx, &[r, g, b, _]) in row.iter().enumerate() {
            let bit = dy * CELL_COLUMNS + dx;
            pixels[bit] = [r, g, b];
            present |= 1 << bit;
        }
    }
    (pixels, present)
}

// Threads are worth starting only for at least this many cells each
const MIN_CELLS_PER_THREAD: usize = 2048;

// `analyse` for every cell, row by row, on several threads. None when the
// frame is too small for that to pay, so cells are better analysed as drawn.
fn analyse_cells<C: Send>(
    (cols, rows): (usize, usize),
    analyse: impl Fn(usize, usize) -> C + Sync,
) -> Option<Vec<C>> {
    let analyse_rows = |rows: std::ops::Range<usize>| {
        let mut cells = Vec::with_capacity(rows.len() * cols);
        for row in rows {
            cells.extend((0..cols).map(|col| analyse(col, row)));
        }
        cells
    };
    let threads = available_threads().min(cols * rows / MIN_CELLS_PER_THREAD);
    if threads <= 1 {
        return None;
    }
    let mut cells = Vec::with_capacity(cols * rows);
    for band in split_across(rows, threads, analyse_rows) {
        cells.extend(band);
    }
    Some(cells)
}

fn cell_extent(frame: Frame, cols: usize, rows: usize) -> (usize, usize) {
    (
        cols.min((frame.width as usize).div_ceil(CELL_COLUMNS)),
        rows.min((frame.height as usize).div_ceil(CELL_ROWS)),
    )
}

pub fn encode_symbols<F: CellFilter<SymbolCell>>(
    frame: Frame,
    (cols, rows): (usize, usize),
    depth: ColorDepth,
    layout: Layout,
    filter: &mut F,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let to_color = |[r, g, b]: [u8; 3]| match depth {
        ColorDepth::Truecolor => Color::Rgb { r, g, b },
        ColorDepth::Palette => Color::AnsiValue(ansi_colours::ansi256_from_rgb([r, g, b])),
    };
    let screen_columns = cols;
    let extent = cell_extent(frame, cols, rows);
    filter.start(extent);
    let analyse = |col, row| {
        let (pixels, present) = cell_pixels(frame, col, row);
        let cell = best_symbol(&pixels, present);
        // A space shows only the background
        let foreground = (cell.symbol != ' ').then(|| to_color(cell.foreground));
        (cell.symbol, foreground, to_color(cell.background))
    };
    let draw = Draw {
        extent,
        screen_columns,
        layout,
    };
    match analyse_cells(extent, analyse) {
        Some(cells) => draw.symbols(|col, row| cells[row * extent.0 + col], filter, out),
        None => draw.symbols(analyse, filter, out),
    }
}

// How cells are laid out on screen
struct Draw {
    // In cells
    extent: (usize, usize),
    screen_columns: usize,
    layout: Layout,
}

impl Draw {
    // Generic over where cells come from, so each source gets its own loop
    fn symbols<F: CellFilter<SymbolCell>>(
        &self,
        cell: impl Fn(usize, usize) -> SymbolCell,
        filter: &mut F,
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        let (cols, rows) = self.extent;
        let mut cursor = Cursor::new(self.screen_columns);
        // Only emit colors when they change
        let (mut foreground, mut background) = (None, None);
        for row in 0..rows {
            if F::DRAWS_ALL {
                start_row(self.layout, row, out)?;
                if self.layout == Layout::Inline {
                    (foreground, background) = (None, None);
                }
            }
            for col in 0..cols {
                let shown = cell(col, row);
                if !filter.changed(row * cols + col, shown) {
                    continue;
                }
                let (symbol, new_foreground, new_background) = shown;
                if !F::DRAWS_ALL {
                    cursor.go_to(col, row, out)?;
                }

                // Both colors go in one sequence when both change
                let set_background = background != Some(new_background);
                let set_foreground = new_foreground.filter(|&color| foreground != Some(color));
                if set_background || set_foreground.is_some() {
                    out.extend_from_slice(b"\x1b[");
                    if set_background {
                        push_color(b"48", new_background, out);
                        background = Some(new_background);
                    }
                    if let Some(color) = set_foreground {
                        if set_background {
                            out.push(b';');
                        }
                        push_color(b"38", color, out);
                        foreground = Some(color);
                    }
                    out.push(b'm');
                }
                out.extend_from_slice(symbol.encode_utf8(&mut [0; 4]).as_bytes());
                if !F::DRAWS_ALL {
                    cursor.printed(col, row);
                }
            }
        }
        queue!(out, ResetColor)
    }

    fn mono_glyphs<F: CellFilter<char>>(
        &self,
        glyph: impl Fn(usize, usize) -> char,
        filter: &mut F,
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        let (cols, rows) = self.extent;
        let mut cursor = Cursor::new(self.screen_columns);
        queue!(out, ResetColor)?;
        for row in 0..rows {
            if F::DRAWS_ALL {
                start_row(self.layout, row, out)?;
                let line: String = (0..cols).map(|col| glyph(col, row)).collect();
                queue!(out, Print(line))?;
                continue;
            }
            for col in 0..cols {
                let glyph = glyph(col, row);
                if filter.changed(row * cols + col, glyph) {
                    cursor.go_to(col, row, out)?;
                    queue!(out, Print(glyph))?;
                    cursor.printed(col, row);
                }
            }
        }
        Ok(())
    }
}

// Every byte's decimal digits, so colors are written without formatting
const DECIMAL: [([u8; 3], usize); 256] = {
    let mut table = [([0; 3], 0); 256];
    let mut value = 0;
    while value < 256 {
        let (hundreds, tens, ones) = (value / 100, value / 10 % 10, value % 10);
        let digits = [b'0' + hundreds as u8, b'0' + tens as u8, b'0' + ones as u8];
        table[value] = match value {
            0..10 => ([digits[2], 0, 0], 1),
            10..100 => ([digits[1], digits[2], 0], 2),
            _ => (digits, 3),
        };
        value += 1;
    }
    table
};

fn push_decimal(value: u8, out: &mut Vec<u8>) {
    let (digits, len) = &DECIMAL[usize::from(value)];
    out.extend_from_slice(&digits[..*len]);
}

// SGR parameters for a foreground (38) or background (48) color
fn push_color(layer: &[u8; 2], color: Color, out: &mut Vec<u8>) {
    out.extend_from_slice(layer);
    match color {
        Color::Rgb { r, g, b } => {
            out.extend_from_slice(b";2;");
            push_decimal(r, out);
            out.push(b';');
            push_decimal(g, out);
            out.push(b';');
            push_decimal(b, out);
        }
        Color::AnsiValue(index) => {
            out.extend_from_slice(b";5;");
            push_decimal(index, out);
        }
        _ => unreachable!("cells are drawn in RGB or palette colors"),
    }
}

// Glyphs with the coverage of their top and bottom halves. All are in code
// page 437, so the Linux console's font has them.
const MONO_GLYPHS: [(char, f32, f32); 7] = [
    (' ', 0.0, 0.0),
    ('░', 0.25, 0.25),
    ('▒', 0.5, 0.5),
    ('▓', 0.75, 0.75),
    ('█', 1.0, 1.0),
    ('▀', 1.0, 0.0),
    ('▄', 0.0, 1.0),
];

pub fn best_mono_glyph(top: f32, bottom: f32) -> char {
    // The first with the least error
    let mut best = (' ', f32::INFINITY);
    for &(glyph, glyph_top, glyph_bottom) in &MONO_GLYPHS {
        let error = (glyph_top - top).powi(2) + (glyph_bottom - bottom).powi(2);
        if error < best.1 {
            best = (glyph, error);
        }
    }
    best.0
}

// In the terminal's own foreground and background colors
pub fn encode_mono_blocks<F: CellFilter<char>>(
    frame: Frame,
    (cols, rows): (usize, usize),
    layout: Layout,
    filter: &mut F,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let screen_columns = cols;
    let extent = cell_extent(frame, cols, rows);
    filter.start(extent);
    // Average luminance of a cell's top or bottom half, from 0 to 1. Pixels
    // outside the frame add nothing.
    let brightness = |pixels: &[[u8; 3]; CELL_PIXELS], present: u8, half: usize| {
        let half_pixels = CELL_PIXELS / 2;
        let bits = half * half_pixels..(half + 1) * half_pixels;
        let count = (present >> bits.start & ((1 << half_pixels) - 1)).count_ones();
        let total: f32 = bits
            .map(|bit| {
                let [r, g, b] = pixels[bit];
                let luminance =
                    0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
                if present & (1 << bit) != 0 {
                    luminance
                } else {
                    0.0
                }
            })
            .sum();
        if count > 0 {
            total / count as f32 / 255.0
        } else {
            0.0
        }
    };

    let analyse = |col, row| {
        let (pixels, present) = cell_pixels(frame, col, row);
        best_mono_glyph(
            brightness(&pixels, present, 0),
            brightness(&pixels, present, 1),
        )
    };
    let draw = Draw {
        extent,
        screen_columns,
        layout,
    };
    match analyse_cells(extent, analyse) {
        Some(glyphs) => draw.mono_glyphs(|col, row| glyphs[row * extent.0 + col], filter, out),
        None => draw.mono_glyphs(analyse, filter, out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_from(width: u32, height: u32, pixels: &[[u8; 3]]) -> Vec<u8> {
        assert_eq!(pixels.len(), (width * height) as usize);
        pixels
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect()
    }

    fn reconstruct(cell: CellSymbol) -> [[u8; 3]; CELL_PIXELS] {
        let mask = symbol_mask(cell.symbol).unwrap();
        std::array::from_fn(|bit| {
            if mask & (1 << bit) != 0 {
                cell.foreground
            } else {
                cell.background
            }
        })
    }

    // Drawn from each character's Unicode name, independently of SYMBOLS
    #[test]
    fn symbol_masks_match_the_glyphs() {
        let glyphs = [
            (' ', ".. .. .. .."),
            ('▘', "#. #. .. .."),
            ('▝', ".# .# .. .."),
            ('▖', ".. .. #. #."),
            ('▗', ".. .. .# .#"),
            ('▀', "## ## .. .."),
            ('▄', ".. .. ## ##"),
            ('▌', "#. #. #. #."),
            ('▐', ".# .# .# .#"),
            ('▚', "#. #. .# .#"),
            ('▞', ".# .# #. #."),
            ('▛', "## ## #. #."),
            ('▜', "## ## .# .#"),
            ('▙', "#. #. ## ##"),
            ('▟', ".# .# ## ##"),
            ('▂', ".. .. .. ##"),
            ('▆', ".. ## ## ##"),
        ];
        assert_eq!(glyphs.len(), SYMBOLS.len());
        for (symbol, art) in glyphs {
            let mask = art
                .chars()
                .filter(|c| !c.is_whitespace())
                .enumerate()
                .fold(0u8, |mask, (bit, c)| mask | (u8::from(c == '#') << bit));
            assert_eq!(symbol_mask(symbol), Some(mask), "{symbol:?}");
        }
    }

    #[test]
    fn every_symbol_is_reproduced_exactly() {
        let (a, b) = ([200, 40, 10], [10, 60, 220]);
        for &(symbol, mask) in &SYMBOLS {
            // Each symbol and its inverse, which swaps the colors
            for mask in [mask, !mask] {
                let pixels = std::array::from_fn(|bit| if mask & (1 << bit) != 0 { a } else { b });
                let cell = best_symbol(&pixels, u8::MAX);
                assert_eq!(reconstruct(cell), pixels, "{symbol:?} {mask:08b}: {cell:?}");
            }
        }
    }

    #[test]
    fn flat_and_nearly_flat_cells_are_spaces() {
        let flat = [[90, 90, 90]; CELL_PIXELS];
        let cell = best_symbol(&flat, u8::MAX);
        assert_eq!((cell.symbol, cell.background), (' ', [90, 90, 90]));

        let mut noisy = flat;
        noisy[3] = [92, 90, 91];
        assert_eq!(best_symbol(&noisy, u8::MAX).symbol, ' ');
    }

    #[test]
    fn missing_pixels_are_ignored() {
        // Only the top two rows are inside the frame: red on the left, blue
        // on the right. The rest is black and must not pull the colors.
        let (red, blue) = ([255, 0, 0], [0, 0, 255]);
        let mut pixels = [[0; 3]; CELL_PIXELS];
        for row in 0..2 {
            pixels[row * 2] = red;
            pixels[row * 2 + 1] = blue;
        }
        let shown = reconstruct(best_symbol(&pixels, 0b0000_1111));
        assert_eq!(shown[..4], pixels[..4]);
    }

    fn encode(frame: Frame, cols: usize, rows: usize, depth: ColorDepth) -> String {
        let mut out = Vec::new();
        encode_symbols(
            frame,
            (cols, rows),
            depth,
            Layout::Screen,
            &mut AllCells,
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn colors_are_sent_only_when_they_change() {
        // Two flat gray cells, then one gray on the left and red on the right
        let (gray, red) = ([128; 3], [255, 0, 0]);
        let row = [gray, gray, gray, gray, gray, red];
        let pixels = frame_from(6, 4, &row.repeat(4));
        let frame = Frame {
            width: 6,
            height: 4,
            pixels: &pixels,
        };
        let text = encode(frame, 3, 1, ColorDepth::Truecolor);
        assert_eq!(text.matches(' ').count(), 2, "{text:?}");
        assert_eq!(text.matches("48;2;128;128;128").count(), 1, "{text:?}");
        assert_eq!(text.matches("2;255;0;0").count(), 1, "{text:?}");
    }

    #[test]
    fn colors_are_written_like_crossterm() {
        use crossterm::style::{SetBackgroundColor, SetForegroundColor};
        for value in [0, 7, 9, 10, 42, 99, 100, 128, 199, 255] {
            for color in [
                Color::Rgb {
                    r: value,
                    g: 255 - value,
                    b: value / 2,
                },
                Color::AnsiValue(value),
            ] {
                let mut ours = b"\x1b[".to_vec();
                push_color(b"48", color, &mut ours);
                ours.extend_from_slice(b"m\x1b[");
                push_color(b"38", color, &mut ours);
                ours.push(b'm');
                let mut expected = Vec::new();
                queue!(
                    expected,
                    SetBackgroundColor(color),
                    SetForegroundColor(color)
                )
                .unwrap();
                assert_eq!(
                    String::from_utf8(ours).unwrap(),
                    String::from_utf8(expected).unwrap()
                );
            }
        }
    }

    #[test]
    fn cells_analysed_on_threads_stay_in_order() {
        // Enough cells for several threads, in rows that don't split evenly
        for (cols, rows) in [(300, 67), (2, MIN_CELLS_PER_THREAD * 5 + 1), (7, 3)] {
            let cells = analyse_cells((cols, rows), |col, row| (col, row));
            if cols * rows < MIN_CELLS_PER_THREAD * 2 {
                assert_eq!(cells, None);
                continue;
            }
            let expected: Vec<_> = (0..rows)
                .flat_map(|row| (0..cols).map(move |col| (col, row)))
                .collect();
            assert_eq!(cells, Some(expected), "{cols}x{rows}");
        }
    }

    #[test]
    fn mono_shades_show_gray_levels() {
        assert_eq!(best_mono_glyph(0.0, 0.0), ' ');
        assert_eq!(best_mono_glyph(0.27, 0.22), '░');
        assert_eq!(best_mono_glyph(0.5, 0.5), '▒');
        assert_eq!(best_mono_glyph(0.7, 0.8), '▓');
        assert_eq!(best_mono_glyph(1.0, 0.95), '█');
        assert_eq!(best_mono_glyph(0.9, 0.1), '▀');
        assert_eq!(best_mono_glyph(0.0, 1.0), '▄');
    }
}
