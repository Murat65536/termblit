use std::io;

use crossterm::{
    queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
};

use super::{Frame, Layout, start_row};

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

// Only pixels in `present` take part, for cells at the frame's edge
pub fn best_symbol(pixels: &[[u8; 3]; CELL_PIXELS], present: u8) -> CellSymbol {
    let sums = |mask: u8| {
        let mut sum = [0f32; 3];
        let mut count = 0f32;
        for (bit, pixel) in pixels.iter().enumerate() {
            if mask & present & (1 << bit) != 0 {
                for (total, &value) in sum.iter_mut().zip(pixel) {
                    *total += f32::from(value);
                }
                count += 1.0;
            }
        }
        (sum, count)
    };
    let mean = |(sum, count): ([f32; 3], f32)| sum.map(|s| (s / count).round() as u8);
    // Squared error is constant minus sum^2 / count for each region, so the
    // best split maximizes this
    let score = |(sum, count): ([f32; 3], f32)| sum.iter().map(|s| s * s / count).sum::<f32>();

    let all = sums(u8::MAX);
    let flat = CellSymbol {
        symbol: ' ',
        foreground: mean(all),
        background: mean(all),
    };
    let mut best = (score(all) + MIN_SHAPE_IMPROVEMENT, flat);

    for &(symbol, mask) in &SYMBOLS[1..] {
        let (inside, outside) = (sums(mask), sums(!mask));
        if inside.1 == 0.0 || outside.1 == 0.0 {
            continue;
        }
        let candidate = score(inside) + score(outside);
        if candidate > best.0 {
            best = (
                candidate,
                CellSymbol {
                    symbol,
                    foreground: mean(inside),
                    background: mean(outside),
                },
            );
        }
    }
    best.1
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    Truecolor,
    // The 240 colors of the xterm palette that don't depend on the theme
    Palette,
}

// The cell's pixels and a mask of those inside the frame
fn cell_pixels(frame: Frame, col: usize, row: usize) -> ([[u8; 3]; CELL_PIXELS], u8) {
    let mut pixels = [[0; 3]; CELL_PIXELS];
    let mut present = 0;
    for dy in 0..CELL_ROWS {
        for dx in 0..CELL_COLUMNS {
            let (x, y) = (col * CELL_COLUMNS + dx, row * CELL_ROWS + dy);
            if x < frame.width as usize && y < frame.height as usize {
                let bit = dy * CELL_COLUMNS + dx;
                pixels[bit] = frame.pixel(x, y);
                present |= 1 << bit;
            }
        }
    }
    (pixels, present)
}

fn cell_extent(frame: Frame, cols: usize, rows: usize) -> (usize, usize) {
    (
        cols.min((frame.width as usize).div_ceil(CELL_COLUMNS)),
        rows.min((frame.height as usize).div_ceil(CELL_ROWS)),
    )
}

pub fn encode_symbols(
    frame: Frame,
    (cols, rows): (usize, usize),
    depth: ColorDepth,
    layout: Layout,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let to_color = |[r, g, b]: [u8; 3]| match depth {
        ColorDepth::Truecolor => Color::Rgb { r, g, b },
        ColorDepth::Palette => Color::AnsiValue(ansi_colours::ansi256_from_rgb([r, g, b])),
    };
    let (cols, rows) = cell_extent(frame, cols, rows);

    // Only emit colors when they change
    let (mut foreground, mut background) = (None, None);
    for row in 0..rows {
        start_row(layout, row, out)?;
        if layout == Layout::Inline {
            (foreground, background) = (None, None);
        }
        for col in 0..cols {
            let (pixels, present) = cell_pixels(frame, col, row);
            let cell = best_symbol(&pixels, present);

            let new_background = to_color(cell.background);
            if background != Some(new_background) {
                queue!(out, SetBackgroundColor(new_background))?;
                background = Some(new_background);
            }
            // A space shows only the background
            if cell.symbol != ' ' {
                let new_foreground = to_color(cell.foreground);
                if foreground != Some(new_foreground) {
                    queue!(out, SetForegroundColor(new_foreground))?;
                    foreground = Some(new_foreground);
                }
            }
            queue!(out, Print(cell.symbol))?;
        }
    }
    queue!(out, ResetColor)
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
    let error = |&(_, glyph_top, glyph_bottom): &(char, f32, f32)| {
        (glyph_top - top).powi(2) + (glyph_bottom - bottom).powi(2)
    };
    MONO_GLYPHS
        .iter()
        .min_by(|a, b| error(a).total_cmp(&error(b)))
        .map_or(' ', |&(glyph, _, _)| glyph)
}

// In the terminal's own foreground and background colors
pub fn encode_mono_blocks(
    frame: Frame,
    (cols, rows): (usize, usize),
    layout: Layout,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let (cols, rows) = cell_extent(frame, cols, rows);
    // Average luminance of a cell's top or bottom half, from 0 to 1
    let brightness = |pixels: &[[u8; 3]; CELL_PIXELS], present: u8, half: usize| {
        let half_rows = CELL_ROWS / 2;
        let (mut total, mut count) = (0.0, 0.0);
        for dy in half * half_rows..(half + 1) * half_rows {
            for dx in 0..CELL_COLUMNS {
                let bit = dy * CELL_COLUMNS + dx;
                if present & (1 << bit) != 0 {
                    let [r, g, b] = pixels[bit];
                    total += 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
                    count += 1.0;
                }
            }
        }
        if count > 0.0 {
            total / count / 255.0
        } else {
            0.0
        }
    };

    queue!(out, ResetColor)?;
    for row in 0..rows {
        start_row(layout, row, out)?;
        let line: String = (0..cols)
            .map(|col| {
                let (pixels, present) = cell_pixels(frame, col, row);
                best_mono_glyph(
                    brightness(&pixels, present, 0),
                    brightness(&pixels, present, 1),
                )
            })
            .collect();
        queue!(out, Print(line))?;
    }
    Ok(())
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
        encode_symbols(frame, (cols, rows), depth, Layout::Screen, &mut out).unwrap();
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
