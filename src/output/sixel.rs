use std::{io, ops::Range, sync::OnceLock};

use super::{
    Frame,
    parallel::{available_threads, split_across},
};

// 4x4 ordered dither thresholds, in sixteenths
const BAYER_4X4: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

// Sixel palette: 6 levels of red and blue and 7 of green, which the eye is most sensitive to
const SIXEL_LEVELS: [usize; 3] = [6, 7, 6];
const SIXEL_PALETTE_SIZE: usize = SIXEL_LEVELS[0] * SIXEL_LEVELS[1] * SIXEL_LEVELS[2];

fn sixel_palette_color(index: usize) -> [u8; 3] {
    let [r_levels, g_levels, b_levels] = SIXEL_LEVELS;
    let level = |value: usize, levels: usize| (value * 255 / (levels - 1)) as u8;
    [
        level(index / (g_levels * b_levels), r_levels),
        level(index / b_levels % g_levels, g_levels),
        level(index % b_levels, b_levels),
    ]
}

// Palette level for each dither threshold and channel value, so quantizing a
// pixel is three table lookups
fn sixel_level_table(levels: usize) -> [[u8; 256]; 16] {
    let mut table = [[0u8; 256]; 16];
    for (threshold_index, row) in table.iter_mut().enumerate() {
        let threshold = (threshold_index as f32 + 0.5) / 16.0;
        for (value, level) in row.iter_mut().enumerate() {
            let scaled = value as f32 / 255.0 * (levels - 1) as f32;
            *level = ((scaled + threshold - 0.5).round() as usize).min(levels - 1) as u8;
        }
    }
    table
}

// The tables for red, green and blue
fn sixel_level_tables() -> &'static [[[u8; 256]; 16]; 3] {
    static TABLES: OnceLock<[[[u8; 256]; 16]; 3]> = OnceLock::new();
    TABLES.get_or_init(|| SIXEL_LEVELS.map(sixel_level_table))
}

// Map each pixel of these rows to a palette entry, dithering so gradients
// don't band
fn sixel_quantize_rows(frame: Frame, rows: Range<usize>) -> Vec<u8> {
    let [_, g_levels, b_levels] = SIXEL_LEVELS;
    let [r_table, g_table, b_table] = sixel_level_tables();

    let width = frame.width as usize;
    let mut indices = Vec::with_capacity(width * rows.len());
    let pixels = &frame.pixels[rows.start * width * 4..rows.end * width * 4];
    for (row, y) in pixels.chunks_exact(width * 4).zip(rows) {
        let thresholds = &BAYER_4X4[y % 4];
        for (x, [r, g, b, _]) in row.as_chunks::<4>().0.iter().enumerate() {
            let t = thresholds[x % 4] as usize;
            let index = usize::from(r_table[t][*r as usize]) * g_levels * b_levels
                + usize::from(g_table[t][*g as usize]) * b_levels
                + usize::from(b_table[t][*b as usize]);
            indices.push(index as u8);
        }
    }
    indices
}

#[cfg(test)]
fn sixel_quantize(frame: Frame) -> Vec<u8> {
    sixel_quantize_rows(frame, 0..frame.height as usize)
}

// Threads are worth starting only for at least this many pixels each
const MIN_PIXELS_PER_THREAD: usize = 32 * 1024;

pub fn encode_sixel(frame: Frame, out: &mut Vec<u8>) -> io::Result<()> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let threads = available_threads().min(width * height / MIN_PIXELS_PER_THREAD);
    let parts = split_across(height.div_ceil(6), threads, |bands| {
        encode_bands(frame, bands)
    });

    // Start sixel data with 1:1 pixel aspect ratio and the image size
    out.extend_from_slice(b"\x1bPq\"1;1;");
    push_number(width, out);
    out.push(b';');
    push_number(height, out);

    let mut used = [false; SIXEL_PALETTE_SIZE];
    for part in &parts {
        for (used, &part_used) in used.iter_mut().zip(&part.used) {
            *used |= part_used;
        }
    }
    for (index, _) in used.iter().enumerate().filter(|&(_, &used)| used) {
        let [r, g, b] = sixel_palette_color(index).map(|c| usize::from(c) * 100 / 255);
        out.push(b'#');
        push_number(index, out);
        out.extend_from_slice(b";2;");
        for (i, value) in [r, g, b].into_iter().enumerate() {
            if i > 0 {
                out.push(b';');
            }
            push_number(value, out);
        }
    }

    for part in parts {
        out.extend_from_slice(&part.data);
    }
    out.extend_from_slice(b"\x1b\\");
    Ok(())
}

// The sixel data for some six-row bands, and the palette entries it uses
struct Bands {
    used: [bool; SIXEL_PALETTE_SIZE],
    data: Vec<u8>,
}

fn encode_bands(frame: Frame, bands: Range<usize>) -> Bands {
    let width = frame.width as usize;
    let rows = bands.start * 6..(bands.end * 6).min(frame.height as usize);
    let height = rows.len();
    let indices = sixel_quantize_rows(frame, rows);

    let mut used = [false; SIXEL_PALETTE_SIZE];
    for &index in &indices {
        used[index as usize] = true;
    }

    // Each six-row band draws one line of sixels per color, overprinting
    let mut data = Vec::new();
    let mut band_bits = vec![0u8; SIXEL_PALETTE_SIZE * width];
    let mut spans = vec![(usize::MAX, 0usize); SIXEL_PALETTE_SIZE];
    let mut band_colors = Vec::with_capacity(SIXEL_PALETTE_SIZE);
    for band_top in (0..height).step_by(6) {
        for row in 0..6.min(height - band_top) {
            let y = band_top + row;
            for (x, &color) in indices[y * width..(y + 1) * width].iter().enumerate() {
                let color = color as usize;
                band_bits[color * width + x] |= 1 << row;
                let span = &mut spans[color];
                if span.0 == usize::MAX {
                    band_colors.push(color);
                    span.0 = x;
                }
                span.0 = span.0.min(x);
                span.1 = span.1.max(x);
            }
        }

        band_colors.sort_unstable();
        for (i, &color) in band_colors.iter().enumerate() {
            if i > 0 {
                // Back to the start of the band for the next color
                data.push(b'$');
            }
            data.push(b'#');
            push_number(color, &mut data);
            // Blank up to the color's first column, and nothing after its last
            let (start, end) = spans[color];
            push_run(b'?', start, &mut data);
            let row = &mut band_bits[color * width..(color + 1) * width];
            write_sixel_runs(&row[start..=end], &mut data);

            // Reset only what this band used
            row[start..=end].fill(0);
            spans[color] = (usize::MAX, 0);
        }
        band_colors.clear();
        data.push(b'-');
    }
    Bands { used, data }
}

fn push_number(value: usize, out: &mut Vec<u8>) {
    let mut digits = [0; 20];
    let mut start = digits.len();
    let mut rest = value;
    loop {
        start -= 1;
        digits[start] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[start..]);
}

// A character repeated, run-length encoded when that's shorter
fn push_run(character: u8, run: usize, out: &mut Vec<u8>) {
    if run > 3 {
        out.push(b'!');
        push_number(run, out);
        out.push(character);
    } else {
        out.extend(std::iter::repeat_n(character, run));
    }
}

// Write one line of sixels, run-length encoding repeats
fn write_sixel_runs(bits: &[u8], out: &mut Vec<u8>) {
    let mut x = 0;
    while x < bits.len() {
        let value = bits[x];
        let run = bits[x..].iter().take_while(|&&b| b == value).count();
        push_run(b'?' + value, run, out);
        x += run;
    }
}

// Stops sixel images from scrolling the screen and places them at the top left
pub const SIXEL_DISABLE_SCROLLING: &str = "\x1b[?80h";
pub const SIXEL_ENABLE_SCROLLING: &str = "\x1b[?80l";

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_sixel(data: &str, width: usize, height: usize) -> Vec<Option<usize>> {
        let body = data
            .strip_prefix(&format!("\x1bPq\"1;1;{width};{height}"))
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let mut image = vec![None; width * height];
        let (mut x, mut band, mut color) = (0, 0, 0);
        let bytes = body.as_bytes();
        let mut i = 0;
        let number = |i: &mut usize| {
            let start = *i;
            while *i < bytes.len() && bytes[*i].is_ascii_digit() {
                *i += 1;
            }
            body[start..*i].parse::<usize>().unwrap()
        };
        while i < bytes.len() {
            match bytes[i] {
                b'#' => {
                    i += 1;
                    color = number(&mut i);
                    if bytes.get(i) == Some(&b';') {
                        // Palette definition
                        while i < bytes.len() && (bytes[i] == b';' || bytes[i].is_ascii_digit()) {
                            i += 1;
                        }
                    }
                }
                b'$' => {
                    x = 0;
                    i += 1;
                }
                b'-' => {
                    x = 0;
                    band += 1;
                    i += 1;
                }
                b'!' | b'?'..=b'~' => {
                    let run = if bytes[i] == b'!' {
                        i += 1;
                        number(&mut i)
                    } else {
                        1
                    };
                    let bits = bytes[i] - b'?';
                    i += 1;
                    for _ in 0..run {
                        for row in 0..6 {
                            let y = band * 6 + row;
                            if bits & (1 << row) != 0 && y < height {
                                assert!(image[y * width + x].is_none(), "pixel drawn twice");
                                image[y * width + x] = Some(color);
                            }
                        }
                        x += 1;
                    }
                }
                other => panic!("unexpected byte {other}"),
            }
        }
        image
    }

    #[test]
    fn sixel_round_trip() {
        // Gradients across several bands, with a partial last band. The
        // bigger is split across threads.
        for (width, height) in [(37usize, 15usize), (401, 257)] {
            assert_eq!(width * height >= MIN_PIXELS_PER_THREAD * 2, width > 100);
            let pixels: Vec<u8> = (0..width * height)
                .flat_map(|i| {
                    let (x, y) = (i % width, i / width);
                    [(x * 7) as u8, (y * 17) as u8, 128, 255]
                })
                .collect();
            let frame = Frame {
                width: width as u32,
                height: height as u32,
                pixels: &pixels,
            };

            let mut out = Vec::new();
            encode_sixel(frame, &mut out).unwrap();
            let decoded = decode_sixel(&String::from_utf8(out).unwrap(), width, height);

            let expected = sixel_quantize(frame);
            for (pixel, (decoded, expected)) in decoded.iter().zip(&expected).enumerate() {
                assert_eq!(
                    *decoded,
                    Some(*expected as usize),
                    "{width}x{height} pixel {pixel}"
                );
            }
        }
    }

    #[test]
    fn sixel_quantization_is_close() {
        // Flat colors exactly on the palette survive dithering unchanged
        let pixels = [0, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 255];
        let frame = Frame {
            width: 3,
            height: 1,
            pixels: &pixels,
        };
        let colors: Vec<[u8; 3]> = sixel_quantize(frame)
            .iter()
            .map(|&i| sixel_palette_color(i as usize))
            .collect();
        assert_eq!(colors, vec![[0, 0, 0], [255, 255, 255], [255, 0, 0]]);
    }

    #[test]
    fn sixel_runs() {
        let mut out = Vec::new();
        write_sixel_runs(&[1, 1, 1, 1, 1, 2, 2, 0], &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "!5@AA?");
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore = "timing only; run with --release --ignored"]
    fn sixel_encode_time() {
        // A real frame if one is given (binary PPM), otherwise a synthetic gradient
        let (width, height, pixels) = match std::env::var("DCAT_BENCH_PPM") {
            Ok(path) => {
                let data = std::fs::read(path).unwrap();
                let header: Vec<&[u8]> = data.splitn(5, |b| b.is_ascii_whitespace()).collect();
                let parse = |b: &[u8]| std::str::from_utf8(b).unwrap().parse::<u32>().unwrap();
                let (w, h) = (parse(header[1]), parse(header[2]));
                let rgb = &data[data.len() - (w * h * 3) as usize..];
                let rgba: Vec<u8> = rgb
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .flat_map(|&[r, g, b]| [r, g, b, 255])
                    .collect();
                (w, h, rgba)
            }
            Err(_) => (
                1200,
                780,
                (0..1200 * 780 * 4).map(|i| (i % 200) as u8).collect(),
            ),
        };
        let frame = Frame {
            width,
            height,
            pixels: &pixels,
        };

        let start = std::time::Instant::now();
        for _ in 0..10 {
            std::hint::black_box(sixel_quantize(frame));
        }
        let quantize = start.elapsed() / 10;

        let mut out = Vec::new();
        let start = std::time::Instant::now();
        for _ in 0..10 {
            out.clear();
            encode_sixel(frame, &mut out).unwrap();
        }
        eprintln!(
            "sixel {width}x{height}: quantize {quantize:?}, total {:?} per frame, {} bytes",
            start.elapsed() / 10,
            out.len()
        );
    }
}
