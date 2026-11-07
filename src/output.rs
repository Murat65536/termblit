use std::io;

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Print, ResetColor},
};

mod characters;
mod detect;
mod kitty;
mod sixel;

#[cfg(test)]
pub use characters::symbol_mask;
pub use characters::{
    AllCells, CELL_COLUMNS, CELL_ROWS, ChangedCells, ColorDepth, SymbolCell, encode_mono_blocks,
    encode_symbols,
};
pub use detect::detect;
pub use kitty::{
    ChangedPixels, KittyPlacement, KittyPosition, bounding_rect, encode_kitty, encode_kitty_edit,
    encode_kitty_edit_shared, encode_kitty_placeholders, encode_kitty_shared, kitty_delete,
    wrap_for_tmux,
};
pub use sixel::{SIXEL_DISABLE_SCROLLING, SIXEL_ENABLE_SCROLLING, encode_sixel};

/// A way of showing frames in the terminal
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputMode {
    /// Sixel graphics
    Sixel,
    /// The Kitty graphics protocol
    Kitty,
    /// Block characters in 24-bit color
    TruecolorCharacters,
    /// Block characters in the 256-color palette
    PaletteCharacters,
    /// Monochrome block characters, for the Linux console and the like
    BlockCharacters,
}

/// A rendered frame of tightly packed RGBA pixels
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// `width * height` pixels, four bytes each, row by row from the top
    pub pixels: &'a [u8],
}

impl Frame<'_> {
    fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        let start = (y * self.width as usize + x) * 4;
        [
            self.pixels[start],
            self.pixels[start + 1],
            self.pixels[start + 2],
        ]
    }

    // The top-left part, at most width by height
    pub fn cropped(&self, width: u32, height: u32) -> Vec<u8> {
        let width = width.min(self.width) as usize;
        let height = height.min(self.height) as usize;
        let row_bytes = self.width as usize * 4;
        self.pixels
            .chunks_exact(row_bytes)
            .take(height)
            .flat_map(|row| &row[..width * 4])
            .copied()
            .collect()
    }
}

impl OutputMode {
    /// Whether the mode draws with text characters rather than images
    pub fn uses_characters(self) -> bool {
        !matches!(self, Self::Kitty | Self::Sixel)
    }

    /// A short name for the mode
    pub fn name(self) -> &'static str {
        match self {
            Self::Sixel => "sixel",
            Self::Kitty => "kitty",
            Self::TruecolorCharacters => "truecolor",
            Self::PaletteCharacters => "palette",
            Self::BlockCharacters => "block",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    // Each on its own line of the screen, from the top
    Screen,
    // One after another from the cursor, like printed text
    Inline,
}

// Inline, colors are reset first so a scrolling newline doesn't fill the new
// line with the background color
fn start_row(layout: Layout, row: usize, out: &mut Vec<u8>) -> io::Result<()> {
    match layout {
        Layout::Screen => queue!(out, MoveTo(0, row as u16)),
        Layout::Inline if row > 0 => queue!(out, ResetColor, Print("\r\n")),
        Layout::Inline => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop() {
        let pixels: Vec<u8> = (0..4 * 2 * 4).map(|v| v as u8).collect();
        let frame = Frame {
            width: 4,
            height: 2,
            pixels: &pixels,
        };
        assert_eq!(frame.cropped(2, 1), (0..8).collect::<Vec<u8>>());
        assert_eq!(frame.cropped(10, 10), pixels);
    }
}
