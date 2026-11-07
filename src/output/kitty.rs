use std::io::{self, Write};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use crossterm::{
    queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
};
use flate2::{Compression, write::ZlibEncoder};

use super::{Frame, Layout, start_row};

// Escape sequences are limited to this many bytes of base64 payload each
const KITTY_CHUNK_SIZE: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KittyPosition {
    // The cursor stays put, so a full-height image can't scroll the screen
    Cursor,
    // The cursor moves past it like printed text
    Inline,
    // Cells wide and tall, shown by Unicode placeholders
    Virtual(u16, u16),
}

#[derive(Clone, Copy)]
pub struct KittyPlacement {
    // None for an image no later one replaces, which can't be deleted either
    pub image_id: Option<u32>,
    pub position: KittyPosition,
}

impl KittyPlacement {
    fn keys(self) -> String {
        let id = self
            .image_id
            .map_or(String::new(), |id| format!("i={id},p=1,"));
        match self.position {
            KittyPosition::Cursor => format!("{id}q=2,C=1"),
            KittyPosition::Inline => format!("{id}q=2"),
            KittyPosition::Virtual(cols, rows) => format!("{id}q=2,U=1,c={cols},r={rows}"),
        }
    }
}

pub fn encode_kitty(frame: Frame, placement: KittyPlacement, out: &mut Vec<u8>) -> io::Result<()> {
    let keys = format!(
        "a=T,f=32,o=z,s={},v={},{}",
        frame.width,
        frame.height,
        placement.keys()
    );
    encode_compressed(frame.pixels, &keys, "", out)
}

// Compressed pixels in chunks, the first with `keys` and the rest with
// `continuation_keys`
fn encode_compressed(
    pixels: &[u8],
    keys: &str,
    continuation_keys: &str,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(pixels)?;
    let payload = BASE64.encode(encoder.finish()?);

    let chunks: Vec<&[u8]> = payload.as_bytes().chunks(KITTY_CHUNK_SIZE).collect();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        if index == 0 {
            write!(out, "\x1b_G{keys},m={more};")?;
        } else {
            write!(out, "\x1b_G{continuation_keys}m={more};")?;
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    Ok(())
}

pub fn encode_kitty_shared(
    frame: Frame,
    placement: KittyPlacement,
    shm_name: &str,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    write!(
        out,
        "\x1b_Ga=T,f=32,t=s,s={},v={},S={},{};{}\x1b\\",
        frame.width,
        frame.height,
        frame.pixels.len(),
        placement.keys(),
        BASE64.encode(shm_name)
    )
}

// A rectangle of an image, in pixels
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    fn union(self, other: Rect) -> Rect {
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        Rect {
            x,
            y,
            width: (self.x + self.width).max(other.x + other.width) - x,
            height: (self.y + self.height).max(other.y + other.height) - y,
        }
    }
}

// Overwrite a rectangle of the image's pixels, which edits it in place
fn edit_keys(image_id: u32, rect: Rect) -> String {
    let Rect {
        x,
        y,
        width,
        height,
    } = rect;
    format!("a=f,i={image_id},r=1,x={x},y={y},s={width},v={height},X=1,f=32,q=2")
}

// `pixels` are the rectangle's, row by row
pub fn encode_kitty_edit(
    image_id: u32,
    rect: Rect,
    pixels: &[u8],
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let keys = format!("{},o=z", edit_keys(image_id, rect));
    // Every chunk of frame data needs a=f
    encode_compressed(pixels, &keys, "a=f,q=2,", out)
}

pub fn encode_kitty_edit_shared(
    image_id: u32,
    rect: Rect,
    size: usize,
    shm_name: &str,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    write!(
        out,
        "\x1b_G{},t=s,S={size};{}\x1b\\",
        edit_keys(image_id, rect),
        BASE64.encode(shm_name)
    )
}

// Changed rows this close together go in one rectangle, saving a command
const MERGE_GAP_ROWS: u32 = 16;

// The image on screen, so only the parts that change are sent
#[derive(Default)]
pub struct ChangedPixels {
    // Empty when the screen doesn't show the last frame
    shown: Vec<u8>,
    width: u32,
    height: u32,
    // Changed pixels gathered from rows that are only partly sent
    scratch: Vec<u8>,
}

impl ChangedPixels {
    // Send all of the next frame
    pub fn forget(&mut self) {
        self.shown.clear();
    }

    // The rectangles of the frame that differ from the image shown, which it
    // then replaces. None if the frame must be sent whole.
    pub fn changes(&mut self, frame: Frame) -> Option<Vec<Rect>> {
        if self.shown.is_empty() || (self.width, self.height) != (frame.width, frame.height) {
            self.shown.clear();
            self.shown.extend_from_slice(frame.pixels);
            (self.width, self.height) = (frame.width, frame.height);
            return None;
        }

        let row_bytes = frame.width as usize * 4;
        let rows = self
            .shown
            .chunks_exact(row_bytes)
            .zip(frame.pixels.chunks_exact(row_bytes));
        let mut rects: Vec<Rect> = Vec::new();
        for (y, (shown, new)) in rows.enumerate() {
            if shown == new {
                continue;
            }
            let pixels = || shown.as_chunks::<4>().0.iter().zip(new.as_chunks::<4>().0);
            let first = pixels().position(|(a, b)| a != b).unwrap_or(0);
            let last = pixels().rposition(|(a, b)| a != b).unwrap_or(0);
            let changed = Rect {
                x: first as u32,
                y: y as u32,
                width: (last - first + 1) as u32,
                height: 1,
            };
            match rects.last_mut() {
                Some(rect) if changed.y - (rect.y + rect.height) < MERGE_GAP_ROWS => {
                    *rect = rect.union(changed);
                }
                _ => rects.push(changed),
            }
        }

        for &rect in &rects {
            for y in rect.y..rect.y + rect.height {
                let start = (y as usize * frame.width as usize + rect.x as usize) * 4;
                let span = start..start + rect.width as usize * 4;
                self.shown[span.clone()].copy_from_slice(&frame.pixels[span]);
            }
        }
        Some(rects)
    }

    // The rectangle's pixels row by row, copied only when they aren't already
    pub fn pixels_in<'a>(&'a mut self, frame: Frame<'a>, rect: Rect) -> &'a [u8] {
        let row_bytes = frame.width as usize * 4;
        let rows = rect.y as usize..(rect.y + rect.height) as usize;
        if rect.width == frame.width {
            return &frame.pixels[rows.start * row_bytes..rows.end * row_bytes];
        }
        self.scratch.clear();
        for row in frame
            .pixels
            .chunks_exact(row_bytes)
            .take(rows.end)
            .skip(rows.start)
        {
            self.scratch
                .extend_from_slice(&row[rect.x as usize * 4..(rect.x + rect.width) as usize * 4]);
        }
        &self.scratch
    }
}

// The smallest rectangle covering them all
pub fn bounding_rect(rects: &[Rect]) -> Option<Rect> {
    rects.iter().copied().reduce(Rect::union)
}

// Kitty shows a virtual placement wherever this character is, in the image's
// id as foreground color, with diacritics giving the cell's row and column
const PLACEHOLDER: char = '\u{10EEEE}';
// The diacritics numbering placeholder rows and columns, from Kitty's
// rowcolumn-diacritics.txt
#[rustfmt::skip]
const PLACEHOLDER_DIACRITICS: [char; 297] = [
    '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}', '\u{033f}',
    '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035b}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036a}', '\u{036b}', '\u{036c}', '\u{036d}', '\u{036e}', '\u{036f}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059c}', '\u{059d}', '\u{059e}', '\u{059f}', '\u{05a0}', '\u{05a1}',
    '\u{05a8}', '\u{05a9}', '\u{05ab}', '\u{05ac}', '\u{05af}', '\u{05c4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065a}', '\u{065b}', '\u{065d}', '\u{065e}', '\u{06d6}', '\u{06d7}', '\u{06d8}',
    '\u{06d9}', '\u{06da}', '\u{06db}', '\u{06dc}', '\u{06df}', '\u{06e0}', '\u{06e1}', '\u{06e2}',
    '\u{06e4}', '\u{06e7}', '\u{06e8}', '\u{06eb}', '\u{06ec}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073a}', '\u{073d}', '\u{073f}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074a}', '\u{07eb}', '\u{07ec}', '\u{07ed}', '\u{07ee}',
    '\u{07ef}', '\u{07f0}', '\u{07f1}', '\u{07f3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081b}', '\u{081c}', '\u{081d}', '\u{081e}', '\u{081f}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082a}', '\u{082b}', '\u{082c}',
    '\u{082d}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0f82}', '\u{0f83}', '\u{0f86}', '\u{0f87}',
    '\u{135d}', '\u{135e}', '\u{135f}', '\u{17dd}', '\u{193a}', '\u{1a17}', '\u{1a75}', '\u{1a76}',
    '\u{1a77}', '\u{1a78}', '\u{1a79}', '\u{1a7a}', '\u{1a7b}', '\u{1a7c}', '\u{1b6b}', '\u{1b6d}',
    '\u{1b6e}', '\u{1b6f}', '\u{1b70}', '\u{1b71}', '\u{1b72}', '\u{1b73}', '\u{1cd0}', '\u{1cd1}',
    '\u{1cd2}', '\u{1cda}', '\u{1cdb}', '\u{1ce0}', '\u{1dc0}', '\u{1dc1}', '\u{1dc3}', '\u{1dc4}',
    '\u{1dc5}', '\u{1dc6}', '\u{1dc7}', '\u{1dc8}', '\u{1dc9}', '\u{1dcb}', '\u{1dcc}', '\u{1dd1}',
    '\u{1dd2}', '\u{1dd3}', '\u{1dd4}', '\u{1dd5}', '\u{1dd6}', '\u{1dd7}', '\u{1dd8}', '\u{1dd9}',
    '\u{1dda}', '\u{1ddb}', '\u{1ddc}', '\u{1ddd}', '\u{1dde}', '\u{1ddf}', '\u{1de0}', '\u{1de1}',
    '\u{1de2}', '\u{1de3}', '\u{1de4}', '\u{1de5}', '\u{1de6}', '\u{1dfe}', '\u{20d0}', '\u{20d1}',
    '\u{20d4}', '\u{20d5}', '\u{20d6}', '\u{20d7}', '\u{20db}', '\u{20dc}', '\u{20e1}', '\u{20e7}',
    '\u{20e9}', '\u{20f0}', '\u{2cef}', '\u{2cf0}', '\u{2cf1}', '\u{2de0}', '\u{2de1}', '\u{2de2}',
    '\u{2de3}', '\u{2de4}', '\u{2de5}', '\u{2de6}', '\u{2de7}', '\u{2de8}', '\u{2de9}', '\u{2dea}',
    '\u{2deb}', '\u{2dec}', '\u{2ded}', '\u{2dee}', '\u{2def}', '\u{2df0}', '\u{2df1}', '\u{2df2}',
    '\u{2df3}', '\u{2df4}', '\u{2df5}', '\u{2df6}', '\u{2df7}', '\u{2df8}', '\u{2df9}', '\u{2dfa}',
    '\u{2dfb}', '\u{2dfc}', '\u{2dfd}', '\u{2dfe}', '\u{2dff}', '\u{a66f}', '\u{a67c}', '\u{a67d}',
    '\u{a6f0}', '\u{a6f1}', '\u{a8e0}', '\u{a8e1}', '\u{a8e2}', '\u{a8e3}', '\u{a8e4}', '\u{a8e5}',
    '\u{a8e6}', '\u{a8e7}', '\u{a8e8}', '\u{a8e9}', '\u{a8ea}', '\u{a8eb}', '\u{a8ec}', '\u{a8ed}',
    '\u{a8ee}', '\u{a8ef}', '\u{a8f0}', '\u{a8f1}', '\u{aab0}', '\u{aab2}', '\u{aab3}', '\u{aab7}',
    '\u{aab8}', '\u{aabe}', '\u{aabf}', '\u{aac1}', '\u{fe20}', '\u{fe21}', '\u{fe22}', '\u{fe23}',
    '\u{fe24}', '\u{fe25}', '\u{fe26}', '\u{10a0f}', '\u{10a38}', '\u{1d185}', '\u{1d186}',
    '\u{1d187}', '\u{1d188}', '\u{1d189}', '\u{1d1aa}', '\u{1d1ab}', '\u{1d1ac}', '\u{1d1ad}',
    '\u{1d242}', '\u{1d243}', '\u{1d244}',
];

// Placeholder text for a virtual placement of `cols` by `rows` cells, starting
// at the top left. Only the first cell of each row is numbered, and the rest
// count on from it.
pub fn encode_kitty_placeholders(
    image_id: u32,
    (cols, rows): (u16, u16),
    layout: Layout,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    // Ids that fit go in a 256-color index, which tmux passes on as it is.
    // Bigger ones need truecolor, with a third diacritic for the top byte.
    let (color, top_byte) = match u8::try_from(image_id) {
        Ok(index) => (Color::AnsiValue(index), None),
        Err(_) => {
            let [top, r, g, b] = image_id.to_be_bytes();
            (Color::Rgb { r, g, b }, Some(top))
        }
    };
    for row in 0..rows.min(PLACEHOLDER_DIACRITICS.len() as u16) {
        start_row(layout, usize::from(row), out)?;
        let mut text = String::from(PLACEHOLDER);
        text.push(PLACEHOLDER_DIACRITICS[usize::from(row)]);
        text.push(PLACEHOLDER_DIACRITICS[0]);
        if let Some(top) = top_byte {
            text.push(PLACEHOLDER_DIACRITICS[usize::from(top)]);
        }
        text.extend(std::iter::repeat_n(
            PLACEHOLDER,
            usize::from(cols).saturating_sub(1),
        ));
        queue!(out, SetForegroundColor(color), Print(text))?;
    }
    queue!(out, ResetColor)
}

// Wrap escape sequences, each ending in ST, so tmux passes them on to the
// terminal: in a tmux DCS with every ESC doubled. Each goes in its own, since
// tmux drops long ones.
pub fn wrap_for_tmux(sequences: &[u8], out: &mut Vec<u8>) {
    let mut rest = sequences;
    while !rest.is_empty() {
        let end = rest
            .windows(2)
            .position(|window| window == b"\x1b\\")
            .map_or(rest.len(), |start| start + 2);
        let (sequence, tail) = rest.split_at(end);
        out.extend_from_slice(b"\x1bPtmux;");
        for &byte in sequence {
            if byte == 0x1b {
                out.push(0x1b);
            }
            out.push(byte);
        }
        out.extend_from_slice(b"\x1b\\");
        rest = tail;
    }
}

pub fn kitty_delete(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn kitty_round_trip() {
        // Noise doesn't compress, so this needs several chunks
        let mut state = 0x2545_f491_u32;
        let pixels: Vec<u8> = (0..200 * 150 * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let frame = Frame {
            width: 200,
            height: 150,
            pixels: &pixels,
        };
        let mut out = Vec::new();
        let placement = KittyPlacement {
            image_id: Some(7),
            position: KittyPosition::Cursor,
        };
        encode_kitty(frame, placement, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        let commands: Vec<&str> = text
            .split("\x1b\\")
            .filter(|c| !c.is_empty())
            .map(|c| c.strip_prefix("\x1b_G").unwrap())
            .collect();
        assert!(commands.len() > 1);
        assert!(commands[0].starts_with("a=T,f=32,o=z,s=200,v=150,i=7,p=1,"));

        let mut payload = String::new();
        for (index, command) in commands.iter().enumerate() {
            let (keys, data) = command.split_once(';').unwrap();
            let last = index + 1 == commands.len();
            assert!(keys.ends_with(if last { "m=0" } else { "m=1" }));
            assert!(data.len() <= KITTY_CHUNK_SIZE);
            payload.push_str(data);
        }

        let compressed = BASE64.decode(payload).unwrap();
        let mut decoded = Vec::new();
        flate2::read::ZlibDecoder::new(&compressed[..])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn kitty_placeholders() {
        let placeholder = |cell: &vt100::Cell| cell.contents().to_string();
        let mut out = Vec::new();
        encode_kitty_placeholders(42, (3, 2), Layout::Screen, &mut out).unwrap();
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::new(4).unwrap(),
            std::num::NonZeroU16::new(5).unwrap(),
            0,
        );
        parser.process(&out);
        let screen = parser.screen();
        for (row, diacritic) in [(0, '\u{0305}'), (1, '\u{030d}')] {
            let first = screen.cell(row, 0).unwrap();
            assert_eq!(placeholder(first), format!("\u{10EEEE}{diacritic}\u{0305}"));
            assert_eq!(first.fgcolor(), vt100::Color::Idx(42));
            for col in 1..3 {
                assert_eq!(placeholder(screen.cell(row, col).unwrap()), "\u{10EEEE}");
            }
            assert_eq!(placeholder(screen.cell(row, 3).unwrap()), "");
        }

        let mut out = Vec::new();
        encode_kitty_placeholders(0x0201_0203, (1, 1), Layout::Screen, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b[38;2;1;2;3m"), "{text:?}");
        assert!(
            text.contains("\u{10EEEE}\u{0305}\u{0305}\u{030e}"),
            "{text:?}"
        );
    }

    #[test]
    fn tmux_passthrough() {
        let mut out = Vec::new();
        wrap_for_tmux(b"\x1b_Ga=T;AAAA\x1b\\\x1b_Gm=0;BB\x1b\\", &mut out);
        assert_eq!(
            out,
            b"\x1bPtmux;\x1b\x1b_Ga=T;AAAA\x1b\x1b\\\x1b\\\x1bPtmux;\x1b\x1b_Gm=0;BB\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn kitty_shared_memory_command() {
        let pixels = vec![0; 3 * 2 * 4];
        let frame = Frame {
            width: 3,
            height: 2,
            pixels: &pixels,
        };
        let mut out = Vec::new();
        let placement = KittyPlacement {
            image_id: Some(7),
            position: KittyPosition::Cursor,
        };
        encode_kitty_shared(frame, placement, "/dcat-1-7", &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let command = text
            .strip_prefix("\x1b_G")
            .and_then(|t| t.strip_suffix("\x1b\\"))
            .unwrap();
        let (keys, payload) = command.split_once(';').unwrap();
        assert_eq!(keys, "a=T,f=32,t=s,s=3,v=2,S=24,i=7,p=1,q=2,C=1");
        assert_eq!(BASE64.decode(payload).unwrap(), b"/dcat-1-7");
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore = "timing only; run with --release --ignored"]
    fn kitty_encode_time() {
        for (width, height) in [(1200u32, 800u32), (1920, 1080), (2560, 1440), (3840, 2160)] {
            let pixels: Vec<u8> = (0..width * height)
                .flat_map(|i| {
                    let (x, y) = (
                        (i % width) as f32 / width as f32,
                        (i / width) as f32 / height as f32,
                    );
                    let d = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
                    if d < 0.3 {
                        [
                            (200.0 * (1.0 - d)) as u8,
                            (180.0 * y) as u8,
                            (150.0 * x) as u8,
                            255,
                        ]
                    } else {
                        [0, 0, 0, 255]
                    }
                })
                .collect();
            let frame = Frame {
                width,
                height,
                pixels: &pixels,
            };
            let mut out = Vec::new();
            let start = std::time::Instant::now();
            for _ in 0..5 {
                out.clear();
                let placement = KittyPlacement {
                    image_id: Some(1),
                    position: KittyPosition::Cursor,
                };
                encode_kitty(frame, placement, &mut out).unwrap();
            }
            eprintln!(
                "kitty {width}x{height}: {:?} per frame, {} KB",
                start.elapsed() / 5,
                out.len() / 1024
            );
        }
    }
}
