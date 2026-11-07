use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{
        self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    },
};
use std::{
    io::{self, Write, stdout},
    panic,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{Receiver, SyncSender, channel, sync_channel},
    },
    thread::{self, JoinHandle},
};

use crate::output::{
    self, AllCells, ChangedCells, ChangedPixels, ColorDepth, Frame, KittyPlacement, KittyPosition,
    Layout, OutputMode, SymbolCell,
};

/// How the terminal is set up and frames are sent
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Pixels per character cell, for terminals that don't report it, like
    /// the ones Neovim runs programs in
    pub cell_size: Option<(u32, u32)>,
    /// The Kitty image id. A [`Terminal`] uses 1 if not given, replacing its
    /// image each frame. A printed frame goes without one unless given, so
    /// later ones don't replace it.
    pub kitty_image_id: Option<u32>,
    /// Send Kitty frames through shared memory without querying the terminal,
    /// removing frames it leaves unread
    pub kitty_shared_memory: bool,
    /// Capture the mouse, reporting positions in pixels where supported
    pub mouse_capture: bool,
    /// Ask for key release events where the terminal can report them; see
    /// [`Terminal::reports_key_releases`]
    pub key_releases: bool,
    /// Send only what changed since the last frame. Faster when much of each
    /// frame stays the same, slower when nearly all of it changes. Applies to
    /// the character modes and Kitty, where it needs a terminal that can edit
    /// images in place with animation frames, like Kitty itself.
    pub diff_frames: bool,
}

const CHARACTER_CELL_PIXELS: (u32, u32) = (output::CELL_COLUMNS as u32, output::CELL_ROWS as u32);
const FALLBACK_CELL_PIXELS: (u32, u32) = (10, 20);

// SGR-Pixels mouse reporting
const ENABLE_PIXEL_MOUSE: &str = "\x1b[?1016h";
const DISABLE_PIXEL_MOUSE: &str = "\x1b[?1016l";

// Terminal modes to undo on exit, readable from the panic hook
static MOUSE_CAPTURED: AtomicBool = AtomicBool::new(false);
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);
static FOCUS_REPORTED: AtomicBool = AtomicBool::new(false);
// 0 for none
static KITTY_IMAGE_SHOWN: AtomicU32 = AtomicU32::new(0);
static KITTY_IN_TMUX: AtomicBool = AtomicBool::new(false);
static SIXEL_SCROLLING_DISABLED: AtomicBool = AtomicBool::new(false);

fn cell_pixels(mode: OutputMode, cell_size: Option<(u32, u32)>) -> (u32, u32) {
    if mode.uses_characters() {
        return CHARACTER_CELL_PIXELS;
    }
    if let Some(size) = cell_size {
        return size;
    }
    match terminal::window_size() {
        Ok(size) if size.width > 0 && size.height > 0 && size.columns > 0 && size.rows > 0 => (
            (u32::from(size.width) / u32::from(size.columns)).max(1),
            (u32::from(size.height) / u32::from(size.rows)).max(1),
        ),
        _ => FALLBACK_CELL_PIXELS,
    }
}

fn kitty_image_id(options: &Options) -> u32 {
    options.kitty_image_id.unwrap_or(1)
}

fn terminal_reads_shared_memory() -> bool {
    #[cfg(unix)]
    let supported = crate::shm::terminal_supports();
    #[cfg(not(unix))]
    let supported = false;
    supported
}

// The status bar, and for Sixel a spare row so the image can't scroll the screen
fn reserved_rows(mode: OutputMode, status_bar: bool) -> u16 {
    u16::from(status_bar || mode == OutputMode::Sixel)
}

const REDIRECTED_SIZE: (u16, u16) = (80, 24);

/// Pixel size of frames that fill the terminal, less the status bar and, for
/// Sixel, a spare row
pub fn screen_size(
    mode: OutputMode,
    options: &Options,
    status_bar: bool,
) -> io::Result<(u32, u32)> {
    let (cols, rows) = terminal::size()?;
    let image_rows = rows.saturating_sub(reserved_rows(mode, status_bar));
    Ok(fit(mode, options, (cols, image_rows)))
}

/// Pixel size of a frame for [`print_frame`] to fill the terminal, or 80 by
/// 24 cells when output isn't going to one
pub fn print_size(mode: OutputMode, options: &Options) -> (u32, u32) {
    let cells = terminal::size().unwrap_or(REDIRECTED_SIZE);
    fit(mode, options, cells)
}

fn fit(mode: OutputMode, options: &Options, (cols, rows): (u16, u16)) -> (u32, u32) {
    let (cell_width, cell_height) = cell_pixels(mode, options.cell_size);
    (
        u32::from(cols.max(1)) * cell_width,
        u32::from(rows.max(1)) * cell_height,
    )
}

/// The terminal, taken over to draw frames: the alternate screen in raw mode,
/// restored when dropped, or on a panic before its message prints
pub struct Terminal {
    mode: OutputMode,
    cell_size: Option<(u32, u32)>,
    reports_key_releases: bool,
    // None once stopped
    writer: Option<FrameWriter>,
}

enum WriterJob {
    Frame {
        pixels: Vec<u8>,
        width: u32,
        height: u32,
        status: Option<String>,
        size: (u16, u16),
        cell_pixels: (u32, u32),
    },
    Clear,
}

// Encodes and writes frames on a background thread. The job channel is a
// rendezvous, so at most one frame is in progress behind the one being rendered.
struct FrameWriter {
    jobs: SyncSender<WriterJob>,
    recycled: Receiver<Vec<u8>>,
    thread: JoinHandle<io::Result<()>>,
}

#[derive(Clone, Copy, PartialEq)]
enum SharedMemory {
    Off,
    // Falls back to inline if the terminal stops reading
    Likely,
    // The caller may skip frames; unread ones are removed
    Assumed,
}

impl FrameWriter {
    fn spawn(
        mode: OutputMode,
        kitty_image_id: u32,
        shared_memory: SharedMemory,
        tmux: bool,
        diff_frames: bool,
    ) -> io::Result<Self> {
        let (jobs, job_receiver) = sync_channel::<WriterJob>(0);
        let (recycle, recycled) = channel();
        let thread = thread::Builder::new()
            .name("frame writer".into())
            .spawn(move || {
                profiling::register_thread!();
                let mut buffer = Vec::new();
                #[cfg(unix)]
                let mut shared_frames = (shared_memory != SharedMemory::Off)
                    .then(|| crate::shm::SharedFrames::new(shared_memory == SharedMemory::Assumed));
                #[cfg(not(unix))]
                let _ = shared_memory;
                let mut placeholders_drawn = None;
                let mut shown = diff_frames.then(Shown::default);
                for job in job_receiver {
                    buffer.clear();
                    {
                        profiling::scope!("Encode");
                        match job {
                            WriterJob::Clear => {
                                queue!(buffer, Clear(ClearType::All))?;
                                placeholders_drawn = None;
                                if let Some(shown) = &mut shown {
                                    shown.forget();
                                }
                            }
                            WriterJob::Frame {
                                pixels,
                                width,
                                height,
                                status,
                                size,
                                cell_pixels,
                            } => {
                                let frame = Frame {
                                    width,
                                    height,
                                    pixels: &pixels,
                                };
                                #[cfg(unix)]
                                let mut write_shared = shared_frames
                                    .as_mut()
                                    .map(|frames| move |data: &[u8]| frames.write(data));
                                let kitty = KittyImage {
                                    id: kitty_image_id,
                                    #[cfg(unix)]
                                    write_shared: write_shared
                                        .as_mut()
                                        .map(|write| write as &mut WriteShared<'_>),
                                    #[cfg(not(unix))]
                                    write_shared: None,
                                    placeholders_drawn: tmux.then_some(&mut placeholders_drawn),
                                };
                                compose_frame(
                                    mode,
                                    cell_pixels,
                                    frame,
                                    status.as_deref(),
                                    size,
                                    kitty,
                                    shown.as_mut(),
                                    &mut buffer,
                                )?;
                                // Fails only if the terminal is being dropped
                                let _ = recycle.send(pixels);
                            }
                        }
                    }

                    profiling::scope!("Write");
                    let mut stdout = stdout().lock();
                    stdout.write_all(&buffer)?;
                    stdout.flush()?;
                }
                Ok(())
            })?;
        Ok(Self {
            jobs,
            recycled,
            thread,
        })
    }
}

impl Terminal {
    /// Take over the terminal to draw frames in `mode`
    pub fn new(mode: OutputMode, options: &Options) -> io::Result<Self> {
        // Restore before the panic message prints, or it's lost on the
        // alternate screen
        let default_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            restore();
            default_hook(info);
        }));

        execute!(stdout(), EnterAlternateScreen, Hide)?;
        enable_raw_mode()?;

        match mode {
            OutputMode::Kitty => KITTY_IMAGE_SHOWN.store(kitty_image_id(options), Ordering::SeqCst),
            OutputMode::Sixel => {
                execute!(stdout(), Print(output::SIXEL_DISABLE_SCROLLING))?;
                SIXEL_SCROLLING_DISABLED.store(true, Ordering::SeqCst);
            }
            _ => {}
        }

        if options.mouse_capture {
            execute!(stdout(), EnableMouseCapture)?;
            if cfg!(unix) {
                execute!(stdout(), Print(ENABLE_PIXEL_MOUSE))?;
            }
            MOUSE_CAPTURED.store(true, Ordering::SeqCst);
        }

        // The shared memory query reads stdin, so it must come before anything
        // else does
        let tmux = mode == OutputMode::Kitty && std::env::var_os("TMUX").is_some();
        KITTY_IN_TMUX.store(tmux, Ordering::SeqCst);
        let over_ssh = ["SSH_CONNECTION", "SSH_TTY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some());
        let shared_memory = if mode != OutputMode::Kitty {
            SharedMemory::Off
        } else if options.kitty_shared_memory {
            SharedMemory::Assumed
        } else if tmux {
            // tmux mistakes the query for a window title. Try anyway unless over
            // SSH, since big inline frames back up in tmux.
            if over_ssh {
                SharedMemory::Off
            } else {
                SharedMemory::Likely
            }
        } else if terminal_reads_shared_memory() {
            SharedMemory::Likely
        } else {
            SharedMemory::Off
        };

        // Neovim's terminal claims keyboard enhancement but never gets
        // releases itself, so keys would stay held
        let mut reports_key_releases = cfg!(windows);
        if options.key_releases
            && !reports_key_releases
            && std::env::var_os("NVIM").is_none()
            && terminal::supports_keyboard_enhancement().unwrap_or(false)
        {
            execute!(
                stdout(),
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                        | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                )
            )?;
            KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
            reports_key_releases = true;
        }

        Ok(Self {
            mode,
            cell_size: options.cell_size,
            reports_key_releases,
            writer: Some(FrameWriter::spawn(
                mode,
                kitty_image_id(options),
                shared_memory,
                tmux,
                options.diff_frames,
            )?),
        })
    }

    /// Whether key releases are reported; see [`Options::key_releases`]
    pub fn reports_key_releases(&self) -> bool {
        self.reports_key_releases
    }

    /// Ask the terminal to report gaining and losing focus
    pub fn report_focus(&mut self) -> io::Result<()> {
        execute!(stdout(), EnableFocusChange)?;
        FOCUS_REPORTED.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Clear leftovers from a larger previous frame
    pub fn clear(&mut self) -> io::Result<()> {
        self.send(WriterJob::Clear)
    }

    /// A buffer for the next frame's pixels, reusing one already written if available
    pub fn take_buffer(&mut self) -> Vec<u8> {
        self.writer
            .as_ref()
            .and_then(|writer| writer.recycled.try_recv().ok())
            .unwrap_or_default()
    }

    /// Queue a frame of tightly packed RGBA pixels for display
    pub fn draw(
        &mut self,
        pixels: Vec<u8>,
        width: u32,
        height: u32,
        status: Option<String>,
    ) -> io::Result<()> {
        let job = WriterJob::Frame {
            pixels,
            width,
            height,
            status,
            size: terminal::size()?,
            cell_pixels: cell_pixels(self.mode, self.cell_size),
        };
        self.send(job)
    }

    fn send(&mut self, job: WriterJob) -> io::Result<()> {
        let sent = match &self.writer {
            Some(writer) => writer.jobs.send(job).is_ok(),
            None => false,
        };
        if sent {
            return Ok(());
        }
        // The writer only stops early on a write error
        self.stop_writer()?;
        Err(io::Error::other("frame writer stopped"))
    }

    fn stop_writer(&mut self) -> io::Result<()> {
        let Some(writer) = self.writer.take() else {
            return Ok(());
        };
        drop(writer.jobs);
        writer
            .thread
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("frame writer panicked")))
    }
}

type WriteShared<'a> = dyn FnMut(&[u8]) -> io::Result<String> + 'a;

struct KittyImage<'a> {
    id: u32,
    // None to send the pixels inline
    write_shared: Option<&'a mut WriteShared<'a>>,
    // Inside tmux, the placeholder size last drawn, which tmux keeps
    placeholders_drawn: Option<&'a mut Option<(u16, u16)>>,
}

// What the screen shows, so only what changes is sent
#[derive(Default)]
struct Shown {
    // The terminal size and cell size it was drawn at
    screen: Option<((u16, u16), (u32, u32))>,
    symbols: ChangedCells<SymbolCell>,
    glyphs: ChangedCells<char>,
    pixels: ChangedPixels,
}

impl Shown {
    fn forget(&mut self) {
        self.symbols.forget();
        self.glyphs.forget();
        self.pixels.forget();
    }

    // Resizing may move or clear what the terminal shows
    fn check_screen(&mut self, size: (u16, u16), cell_pixels: (u32, u32)) {
        if self.screen != Some((size, cell_pixels)) {
            self.forget();
            self.screen = Some((size, cell_pixels));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn compose_frame(
    mode: OutputMode,
    (cell_width, cell_height): (u32, u32),
    frame: Frame,
    status: Option<&str>,
    (cols, rows): (u16, u16),
    kitty: KittyImage<'_>,
    mut shown: Option<&mut Shown>,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    if let Some(shown) = shown.as_deref_mut() {
        shown.check_screen((cols, rows), (cell_width, cell_height));
    }
    let image_rows = rows.saturating_sub(reserved_rows(mode, status.is_some()));
    let (cols_used, rows_used) = (usize::from(cols), usize::from(image_rows));

    // tmux redraws a whole pane after a synchronized update, which would send
    // the placeholder text again every frame. Kitty swaps images at once anyway.
    let synchronized = kitty.placeholders_drawn.is_none();
    if synchronized {
        queue!(out, BeginSynchronizedUpdate)?;
    }

    match mode {
        OutputMode::TruecolorCharacters | OutputMode::PaletteCharacters => {
            let cells = (cols_used, rows_used);
            let depth = if mode == OutputMode::TruecolorCharacters {
                ColorDepth::Truecolor
            } else {
                ColorDepth::Palette
            };
            match shown {
                Some(shown) => {
                    let filter = &mut shown.symbols;
                    output::encode_symbols(frame, cells, depth, Layout::Screen, filter, out)?;
                }
                None => {
                    output::encode_symbols(
                        frame,
                        cells,
                        depth,
                        Layout::Screen,
                        &mut AllCells,
                        out,
                    )?;
                }
            }
        }
        OutputMode::BlockCharacters => {
            let cells = (cols_used, rows_used);
            match shown {
                Some(shown) => {
                    output::encode_mono_blocks(
                        frame,
                        cells,
                        Layout::Screen,
                        &mut shown.glyphs,
                        out,
                    )?;
                }
                None => {
                    output::encode_mono_blocks(frame, cells, Layout::Screen, &mut AllCells, out)?
                }
            }
        }
        OutputMode::Kitty | OutputMode::Sixel => {
            let max_width = u32::from(cols) * cell_width;
            let max_height = u32::from(image_rows) * cell_height;
            let cropped;
            let frame = if frame.width > max_width || frame.height > max_height {
                cropped = frame.cropped(max_width, max_height);
                Frame {
                    width: frame.width.min(max_width),
                    height: frame.height.min(max_height),
                    pixels: &cropped,
                }
            } else {
                frame
            };

            queue!(out, MoveTo(0, 0))?;
            if frame.width > 0 && frame.height > 0 {
                if mode == OutputMode::Kitty {
                    let cells = (
                        frame.width.div_ceil(cell_width) as u16,
                        frame.height.div_ceil(cell_height) as u16,
                    );
                    let position = match kitty.placeholders_drawn {
                        Some(_) => KittyPosition::Virtual(cells.0, cells.1),
                        None => KittyPosition::Cursor,
                    };
                    let placement = KittyPlacement {
                        image_id: Some(kitty.id),
                        position,
                    };
                    let start = out.len();
                    let changed = shown.map(|shown| &mut shown.pixels);
                    encode_kitty_frame(
                        frame,
                        kitty.id,
                        placement,
                        kitty.write_shared,
                        changed,
                        out,
                    )?;
                    if let Some(drawn) = kitty.placeholders_drawn {
                        let commands = out.split_off(start);
                        output::wrap_for_tmux(&commands, out);
                        if *drawn != Some(cells) {
                            output::encode_kitty_placeholders(
                                kitty.id,
                                cells,
                                Layout::Screen,
                                out,
                            )?;
                            *drawn = Some(cells);
                        }
                    }
                } else {
                    output::encode_sixel(frame, out)?;
                }
            }
        }
    }

    if let Some(status) = status
        && rows > 0
    {
        let status: String = status.chars().take(usize::from(cols)).collect();
        queue!(
            out,
            MoveTo(0, rows - 1),
            Clear(ClearType::CurrentLine),
            SetAttribute(Attribute::Reverse),
            Print(status),
            SetAttribute(Attribute::Reset)
        )?;
    }

    if synchronized {
        queue!(out, EndSynchronizedUpdate)?;
    }
    Ok(())
}

// The image whole, or with `shown` holding the one on screen, only the parts
// that changed
fn encode_kitty_frame(
    frame: Frame,
    id: u32,
    placement: KittyPlacement,
    mut write_shared: Option<&mut WriteShared<'_>>,
    shown: Option<&mut ChangedPixels>,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let changes = shown.and_then(|shown| shown.changes(frame).map(|rects| (shown, rects)));
    let Some((shown, rects)) = changes else {
        return match write_shared.map(|write| write(frame.pixels)) {
            Some(Ok(name)) => output::encode_kitty_shared(frame, placement, &name, out),
            _ => output::encode_kitty(frame, placement, out),
        };
    };

    // The terminal removes each shared memory object it reads, and at most two
    // may wait unread, so all the changes go in one
    if let Some(write) = write_shared.as_mut()
        && let Some(rect) = output::bounding_rect(&rects)
    {
        let pixels = shown.pixels_in(frame, rect);
        if let Ok(name) = write(pixels) {
            return output::encode_kitty_edit_shared(id, rect, pixels.len(), &name, out);
        }
    }
    for rect in rects {
        output::encode_kitty_edit(id, rect, shown.pixels_in(frame, rect), out)?;
    }
    Ok(())
}

/// Print one frame at the cursor like text, leaving the cursor below it
pub fn print_frame(mode: OutputMode, options: &Options, frame: Frame) -> io::Result<()> {
    let mut out = Vec::new();
    let tmux = std::env::var_os("TMUX").is_some();
    compose_inline(
        mode,
        cell_pixels(mode, options.cell_size),
        frame,
        (options.kitty_image_id, tmux),
        &mut out,
    )?;
    let mut stdout = stdout().lock();
    stdout.write_all(&out)?;
    stdout.flush()
}

fn compose_inline(
    mode: OutputMode,
    (cell_width, cell_height): (u32, u32),
    frame: Frame,
    (kitty_image_id, tmux): (Option<u32>, bool),
    out: &mut Vec<u8>,
) -> io::Result<()> {
    // Printed frames are sized to fit, so show every cell
    let all = (usize::MAX, usize::MAX);
    match mode {
        OutputMode::TruecolorCharacters => {
            output::encode_symbols(
                frame,
                all,
                ColorDepth::Truecolor,
                Layout::Inline,
                &mut AllCells,
                out,
            )?;
        }
        OutputMode::PaletteCharacters => {
            output::encode_symbols(
                frame,
                all,
                ColorDepth::Palette,
                Layout::Inline,
                &mut AllCells,
                out,
            )?;
        }
        OutputMode::BlockCharacters => {
            output::encode_mono_blocks(frame, all, Layout::Inline, &mut AllCells, out)?;
        }
        OutputMode::Kitty if tmux => {
            // Printed images stay in the scrollback, so each needs its own id
            let id = kitty_image_id.unwrap_or(std::process::id() % 255 + 1);
            let cells = (
                frame.width.div_ceil(cell_width) as u16,
                frame.height.div_ceil(cell_height) as u16,
            );
            let placement = KittyPlacement {
                image_id: Some(id),
                position: KittyPosition::Virtual(cells.0, cells.1),
            };
            let mut commands = Vec::new();
            output::encode_kitty(frame, placement, &mut commands)?;
            output::wrap_for_tmux(&commands, out);
            output::encode_kitty_placeholders(id, cells, Layout::Inline, out)?;
        }
        OutputMode::Kitty => {
            let placement = KittyPlacement {
                image_id: kitty_image_id,
                position: KittyPosition::Inline,
            };
            output::encode_kitty(frame, placement, out)?;
        }
        OutputMode::Sixel => {
            output::encode_sixel(frame, out)?;
        }
    }
    // Text and images alike leave the cursor at the end of their last row
    out.extend_from_slice(b"\r\n");
    Ok(())
}

fn restore() {
    let kitty_image = KITTY_IMAGE_SHOWN.swap(0, Ordering::SeqCst);
    if kitty_image != 0 {
        let delete = output::kitty_delete(kitty_image);
        let mut command = Vec::new();
        if KITTY_IN_TMUX.load(Ordering::SeqCst) {
            output::wrap_for_tmux(delete.as_bytes(), &mut command);
        } else {
            command.extend_from_slice(delete.as_bytes());
        }
        let _ = stdout().write_all(&command).and_then(|()| stdout().flush());
    }
    if SIXEL_SCROLLING_DISABLED.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout(), Print(output::SIXEL_ENABLE_SCROLLING));
    }
    if FOCUS_REPORTED.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout(), DisableFocusChange);
    }
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    if MOUSE_CAPTURED.swap(false, Ordering::SeqCst) {
        if cfg!(unix) {
            let _ = execute!(stdout(), Print(DISABLE_PIXEL_MOUSE));
        }
        let _ = execute!(stdout(), DisableMouseCapture);
    }
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), Show, LeaveAlternateScreen);
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Finish writing before restoring, so the two never interleave
        let _ = self.stop_writer();
        restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use output::{CELL_COLUMNS, CELL_ROWS};
    use std::num::NonZeroU16;
    use vt100::Color as Vt;

    const PIXEL_CELL_SIZE: (u32, u32) = (10, 20);
    const SHAPES: &str = "▘▝▖▗▀▄▌▐▚▞▛▜▙▟▂▆";

    fn screen(
        mode: OutputMode,
        frame: Frame,
        status: Option<&str>,
        (cols, rows): (u16, u16),
    ) -> vt100::Parser {
        let mut out = Vec::new();
        compose_frame(
            mode,
            PIXEL_CELL_SIZE,
            frame,
            status,
            (cols, rows),
            KittyImage {
                id: 1,
                write_shared: None,
                placeholders_drawn: None,
            },
            None,
            &mut out,
        )
        .unwrap();
        let mut parser = vt100::Parser::new(
            NonZeroU16::new(rows).unwrap(),
            NonZeroU16::new(cols).unwrap(),
            0,
        );
        parser.process(&out);
        parser
    }

    #[test]
    fn given_cell_size_wins_in_pixel_modes() {
        for mode in [OutputMode::Kitty, OutputMode::Sixel] {
            assert_eq!(cell_pixels(mode, Some((9, 18))), (9, 18), "{mode:?}");
        }
        assert_eq!(
            cell_pixels(OutputMode::TruecolorCharacters, Some((9, 18))),
            CHARACTER_CELL_PIXELS
        );
    }

    // Each cell gets a two-color block shape, so it can be shown exactly
    fn shaped_frame(cols: usize, rows: usize) -> (u32, u32, Vec<u8>) {
        let (width, height) = (cols * CELL_COLUMNS, rows * CELL_ROWS);
        let shapes: Vec<char> = SHAPES.chars().collect();
        let mut pixels = vec![255; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let (col, row) = (x / CELL_COLUMNS, y / CELL_ROWS);
                let shape = shapes[(row * cols + col) % shapes.len()];
                let mask = output::symbol_mask(shape).unwrap();
                let bit = (y % CELL_ROWS) * CELL_COLUMNS + x % CELL_COLUMNS;
                let color = if mask & (1 << bit) != 0 {
                    [(col * 30) as u8, (row * 40) as u8, 200]
                } else {
                    [255 - (col * 30) as u8, 100, (row * 20) as u8]
                };
                let start = (y * width + x) * 4;
                pixels[start..start + 3].copy_from_slice(&color);
            }
        }
        (width as u32, height as u32, pixels)
    }

    fn source_cell(pixels: &[u8], width: u32, col: u16, row: u16) -> Vec<[u8; 3]> {
        (0..CELL_ROWS)
            .flat_map(|dy| (0..CELL_COLUMNS).map(move |dx| (dx, dy)))
            .map(|(dx, dy)| {
                let x = usize::from(col) * CELL_COLUMNS + dx;
                let y = usize::from(row) * CELL_ROWS + dy;
                let start = (y * width as usize + x) * 4;
                [pixels[start], pixels[start + 1], pixels[start + 2]]
            })
            .collect()
    }

    fn shown_cell(parser: &vt100::Parser, col: u16, row: u16) -> Vec<Vt> {
        let cell = parser.screen().cell(row, col).unwrap();
        let symbol = cell.contents().chars().next().unwrap_or(' ');
        let mask = output::symbol_mask(symbol).unwrap_or_else(|| panic!("{symbol:?}"));
        (0..CELL_ROWS * CELL_COLUMNS)
            .map(|bit| {
                if mask & (1 << bit) != 0 {
                    cell.fgcolor()
                } else {
                    cell.bgcolor()
                }
            })
            .collect()
    }

    fn rgb([r, g, b]: [u8; 3]) -> Vt {
        Vt::Rgb(r, g, b)
    }

    fn row_text(parser: &vt100::Parser, row: u16, cols: u16) -> String {
        (0..cols)
            .map(|col| parser.screen().cell(row, col).unwrap().contents())
            .collect()
    }

    #[test]
    fn truecolor_cells_reproduce_every_pixel() {
        let (width, height, pixels) = shaped_frame(4, 4);
        let frame = Frame {
            width,
            height,
            pixels: &pixels,
        };
        let parser = screen(OutputMode::TruecolorCharacters, frame, None, (4, 4));

        for row in 0..4 {
            for col in 0..4 {
                let expected: Vec<Vt> = source_cell(&pixels, width, col, row)
                    .into_iter()
                    .map(rgb)
                    .collect();
                assert_eq!(shown_cell(&parser, col, row), expected, "({col}, {row})");
            }
        }
    }

    #[test]
    fn palette_cells_use_palette_colors() {
        let (width, height, pixels) = shaped_frame(3, 2);
        let frame = Frame {
            width,
            height,
            pixels: &pixels,
        };
        let parser = screen(OutputMode::PaletteCharacters, frame, None, (3, 2));

        for row in 0..2 {
            for col in 0..3 {
                let expected: Vec<Vt> = source_cell(&pixels, width, col, row)
                    .into_iter()
                    .map(|color| Vt::Idx(ansi_colours::ansi256_from_rgb(color)))
                    .collect();
                assert_eq!(shown_cell(&parser, col, row), expected, "({col}, {row})");
            }
        }
    }

    #[test]
    fn mono_blocks_use_terminal_colors() {
        // Left cell lit on top, right cell lit on the bottom
        let (white, black) = ([255, 255, 255, 255], [0, 0, 0, 255]);
        let top = [white, white, black, black];
        let bottom = [black, black, white, white];
        let pixels: Vec<u8> = [top, top, bottom, bottom].concat().concat();
        let frame = Frame {
            width: 4,
            height: 4,
            pixels: &pixels,
        };
        let parser = screen(OutputMode::BlockCharacters, frame, None, (2, 1));

        assert_eq!(row_text(&parser, 0, 2), "▀▄");
        for col in 0..2 {
            let cell = parser.screen().cell(0, col).unwrap();
            assert_eq!((cell.fgcolor(), cell.bgcolor()), (Vt::Default, Vt::Default));
        }
    }

    #[test]
    fn status_bar_takes_the_last_row() {
        let (width, height, pixels) = shaped_frame(4, 3);
        let frame = Frame {
            width,
            height,
            pixels: &pixels,
        };
        let parser = screen(
            OutputMode::TruecolorCharacters,
            frame,
            Some("FPS: 60.0"),
            (4, 3),
        );

        let image_row = row_text(&parser, 1, 4);
        assert_eq!(image_row.chars().count(), 4);
        assert!(
            image_row.chars().all(|c| SHAPES.contains(c)),
            "{image_row:?}"
        );
        assert_eq!(row_text(&parser, 2, 4), "FPS:");
        assert!(parser.screen().cell(2, 0).unwrap().inverse());
        assert!(!parser.screen().cell(1, 0).unwrap().inverse());
    }

    #[test]
    fn oversized_frames_are_clipped_without_scrolling() {
        let (width, height, pixels) = shaped_frame(10, 6);
        let frame = Frame {
            width,
            height,
            pixels: &pixels,
        };
        let parser = screen(OutputMode::TruecolorCharacters, frame, None, (4, 3));

        // Had anything wrapped or scrolled, the corners would hold other cells
        for (col, row) in [(0, 0), (3, 2)] {
            let expected: Vec<Vt> = source_cell(&pixels, width, col, row)
                .into_iter()
                .map(rgb)
                .collect();
            assert_eq!(shown_cell(&parser, col, row), expected, "({col}, {row})");
        }
        assert_eq!(parser.screen().cursor_position().0, 2);
    }

    #[test]
    fn partial_cells_show_the_pixels_that_exist() {
        // Six pixel rows: one full cell row and half of another
        let (width, _, pixels) = shaped_frame(2, 2);
        let pixels = &pixels[..width as usize * 6 * 4];
        let frame = Frame {
            width,
            height: 6,
            pixels,
        };
        let parser = screen(OutputMode::TruecolorCharacters, frame, None, (2, 2));

        for col in 0..2 {
            let present_pixels = CELL_COLUMNS * 2;
            let expected: Vec<Vt> = source_cell(&pixels.repeat(2), width, col, 1)[..present_pixels]
                .iter()
                .map(|&color| rgb(color))
                .collect();
            assert_eq!(
                shown_cell(&parser, col, 1)[..present_pixels],
                expected,
                "col {col}"
            );
        }
    }

    #[test]
    fn image_protocols_leave_no_text_behind() {
        let pixels = vec![120; 50 * 70 * 4];
        let frame = Frame {
            width: 50,
            height: 70,
            pixels: &pixels,
        };
        for mode in [OutputMode::Kitty, OutputMode::Sixel] {
            let parser = screen(mode, frame, Some("status"), (4, 3));
            for row in 0..2 {
                assert_eq!(row_text(&parser, row, 4).trim(), "", "{mode:?} row {row}");
            }
            assert_eq!(row_text(&parser, 2, 4), "stat", "{mode:?}");
        }
    }

    fn compose_kitty(write_shared: &mut WriteShared<'_>) -> String {
        let pixels = vec![120; 20 * 40 * 4];
        let frame = Frame {
            width: 20,
            height: 40,
            pixels: &pixels,
        };
        let mut out = Vec::new();
        compose_frame(
            OutputMode::Kitty,
            PIXEL_CELL_SIZE,
            frame,
            None,
            (4, 3),
            KittyImage {
                id: 1,
                write_shared: Some(write_shared),
                placeholders_drawn: None,
            },
            None,
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn kitty_sends_shared_memory_names() {
        let mut written = Vec::new();
        let text = compose_kitty(&mut |data: &[u8]| {
            written.push(data.len());
            Ok("/dcat-test".into())
        });
        assert_eq!(written, [20 * 40 * 4]);
        assert!(text.contains("t=s,"), "{text}");
        assert!(!text.contains("o=z"), "{text}");
    }

    #[test]
    fn kitty_in_tmux_uses_placeholders() {
        let pixels = vec![120; 20 * 40 * 4];
        let frame = Frame {
            width: 20,
            height: 40,
            pixels: &pixels,
        };
        let mut drawn = None;
        let compose = |drawn: &mut Option<(u16, u16)>| {
            let mut out = Vec::new();
            let kitty = KittyImage {
                id: 1,
                write_shared: None,
                placeholders_drawn: Some(drawn),
            };
            compose_frame(
                OutputMode::Kitty,
                PIXEL_CELL_SIZE,
                frame,
                None,
                (4, 3),
                kitty,
                None,
                &mut out,
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        };

        let first = compose(&mut drawn);
        assert!(
            !first.replace("\x1b\x1b", "").contains("\x1b_G"),
            "{first:?}"
        );
        assert!(first.contains("\x1bPtmux;\x1b\x1b_Ga=T,"), "{first:?}");
        assert!(first.contains(",U=1,c=2,r=2,"), "{first:?}");
        assert_eq!(first.matches('\u{10EEEE}').count(), 4);
        assert_eq!(drawn, Some((2, 2)));

        assert!(!first.contains("\x1b[?2026h"), "{first:?}");

        let second = compose(&mut drawn);
        assert!(second.contains("\x1bPtmux;"));
        assert_eq!(second.matches('\u{10EEEE}').count(), 0);
    }

    #[test]
    fn kitty_falls_back_when_shared_memory_fails() {
        let text = compose_kitty(&mut |_: &[u8]| Err(io::Error::other("no space")));
        assert!(text.contains("o=z"), "{text}");
        assert!(!text.contains("t=s"), "{text}");
    }

    fn print_inline(mode: OutputMode, frame: Frame, tmux: bool) -> Vec<u8> {
        let mut out = Vec::new();
        compose_inline(mode, PIXEL_CELL_SIZE, frame, (None, tmux), &mut out).unwrap();
        out
    }

    #[test]
    fn printed_frames_start_at_the_cursor() {
        let (width, height, pixels) = shaped_frame(3, 2);
        let frame = Frame {
            width,
            height,
            pixels: &pixels,
        };
        let out = print_inline(OutputMode::TruecolorCharacters, frame, false);
        assert!(!String::from_utf8_lossy(&out).contains('H'), "{out:?}");

        let mut parser =
            vt100::Parser::new(NonZeroU16::new(6).unwrap(), NonZeroU16::new(10).unwrap(), 0);
        parser.process(b"$ dcat\r\n");
        parser.process(&out);
        for row in 0..2 {
            for col in 0..3 {
                let expected: Vec<Vt> = source_cell(&pixels, width, col, row)
                    .into_iter()
                    .map(rgb)
                    .collect();
                assert_eq!(
                    shown_cell(&parser, col, row + 1),
                    expected,
                    "({col}, {row})"
                );
            }
        }
        assert_eq!(parser.screen().cursor_position(), (3, 0));
    }

    #[test]
    fn printed_kitty_images_move_the_cursor() {
        let pixels = vec![120; 20 * 40 * 4];
        let frame = Frame {
            width: 20,
            height: 40,
            pixels: &pixels,
        };
        let text = String::from_utf8(print_inline(OutputMode::Kitty, frame, false)).unwrap();
        assert!(
            text.starts_with("\x1b_Ga=T,f=32,o=z,s=20,v=40,q=2,m=0;"),
            "{text:?}"
        );
        assert!(text.ends_with("\x1b\\\r\n"), "{text:?}");

        let text = String::from_utf8(print_inline(OutputMode::Kitty, frame, true)).unwrap();
        assert!(text.starts_with("\x1bPtmux;"), "{text:?}");
        assert!(text.contains(",U=1,c=2,r=2,"), "{text:?}");
        assert_eq!(text.matches('\u{10EEEE}').count(), 4);
        assert!(!text.contains("\x1b[1;1H"), "{text:?}");
        assert!(text.ends_with("\r\n"), "{text:?}");
    }

    fn recolor_cell(pixels: &mut [u8], width: u32, (col, row): (usize, usize), color: [u8; 3]) {
        for dy in 0..CELL_ROWS {
            for dx in 0..CELL_COLUMNS {
                let (x, y) = (col * CELL_COLUMNS + dx, row * CELL_ROWS + dy);
                let start = (y * width as usize + x) * 4;
                pixels[start..start + 3].copy_from_slice(&color);
            }
        }
    }

    // Each frame drawn over the last on one screen, with the bytes each sent
    fn draw_in_turn(
        mode: OutputMode,
        frames: &[Frame],
        mut shown: Option<&mut Shown>,
        (cols, rows): (u16, u16),
    ) -> (vt100::Parser, Vec<Vec<u8>>) {
        let mut parser = vt100::Parser::new(
            NonZeroU16::new(rows).unwrap(),
            NonZeroU16::new(cols).unwrap(),
            0,
        );
        let mut sent = Vec::new();
        for &frame in frames {
            let mut out = Vec::new();
            let kitty = KittyImage {
                id: 1,
                write_shared: None,
                placeholders_drawn: None,
            };
            compose_frame(
                mode,
                PIXEL_CELL_SIZE,
                frame,
                None,
                (cols, rows),
                kitty,
                shown.as_deref_mut(),
                &mut out,
            )
            .unwrap();
            parser.process(&out);
            sent.push(out);
        }
        (parser, sent)
    }

    fn assert_same_screen(a: &vt100::Parser, b: &vt100::Parser, (cols, rows): (u16, u16)) {
        for row in 0..rows {
            for col in 0..cols {
                let (a, b) = (
                    a.screen().cell(row, col).unwrap(),
                    b.screen().cell(row, col).unwrap(),
                );
                // A space shows only its background
                let shown = |cell: &vt100::Cell| {
                    let blank = cell.contents().trim().is_empty();
                    (
                        cell.contents().to_string(),
                        (!blank).then(|| cell.fgcolor()),
                        cell.bgcolor(),
                    )
                };
                assert_eq!(shown(a), shown(b), "({col}, {row})");
            }
        }
    }

    #[test]
    fn diffed_characters_match_full_redraws() {
        let size = (8, 5);
        let (width, height, before) = shaped_frame(8, 5);
        let mut after = before.clone();
        // The last column, then the first of the next row, and a run of two
        for cell in [(7, 0), (0, 1), (3, 3), (4, 3)] {
            recolor_cell(&mut after, width, cell, [9, 200, 30]);
        }
        let frame = |pixels| Frame {
            width,
            height,
            pixels,
        };
        let frames = [frame(&before), frame(&after)];

        for mode in [
            OutputMode::TruecolorCharacters,
            OutputMode::PaletteCharacters,
            OutputMode::BlockCharacters,
        ] {
            let (full, full_sent) = draw_in_turn(mode, &frames, None, size);
            let mut shown = Shown::default();
            let (diffed, diffed_sent) = draw_in_turn(mode, &frames, Some(&mut shown), size);
            assert_same_screen(&diffed, &full, size);
            assert_eq!(diffed_sent[0], full_sent[0], "{mode:?}");
            assert!(
                diffed_sent[1].len() * 3 < full_sent[1].len(),
                "{mode:?}: {} of {} bytes",
                diffed_sent[1].len(),
                full_sent[1].len()
            );

            // Nothing changed, so no cells are sent
            let (_, sent) = draw_in_turn(mode, &frames[1..], Some(&mut shown), size);
            let text = String::from_utf8(sent[0].clone()).unwrap();
            assert_eq!(
                text.replace("\x1b[?2026h", "")
                    .replace("\x1b[?2026l", "")
                    .replace("\x1b[0m", ""),
                "",
                "{mode:?}"
            );

            // Once forgotten, everything is drawn again
            shown.forget();
            let (_, sent) = draw_in_turn(mode, &frames[1..], Some(&mut shown), size);
            assert_eq!(sent[0], full_sent[1], "{mode:?}");
        }
    }

    #[test]
    fn diffed_characters_redraw_after_resizing() {
        let (width, height, pixels) = shaped_frame(8, 5);
        let frame = Frame {
            width,
            height,
            pixels: &pixels,
        };
        let mut shown = Shown::default();
        draw_in_turn(
            OutputMode::TruecolorCharacters,
            &[frame],
            Some(&mut shown),
            (8, 5),
        );
        let (resized, _) = draw_in_turn(
            OutputMode::TruecolorCharacters,
            &[frame],
            Some(&mut shown),
            (9, 5),
        );
        let (full, _) = draw_in_turn(OutputMode::TruecolorCharacters, &[frame], None, (9, 5));
        assert_same_screen(&resized, &full, (9, 5));
    }

    // Kitty's commands for image 1 applied to its pixels, as the terminal would
    fn apply_kitty(image: &mut (u32, u32, Vec<u8>), sent: &[u8]) {
        use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
        use std::{collections::HashMap, io::Read};

        let text = String::from_utf8(sent.to_vec()).unwrap();
        let mut keys = HashMap::new();
        let mut payload = String::new();
        for command in text.split("\x1b\\") {
            let Some((_, command)) = command.split_once("\x1b_G") else {
                continue;
            };
            let (command_keys, data) = command.split_once(';').unwrap();
            for pair in command_keys.split(',') {
                let (key, value) = pair.split_once('=').unwrap();
                keys.entry(key.to_string()).or_insert(value.to_string());
            }
            payload.push_str(data);
            if keys["m"] == "1" {
                keys.remove("m");
                continue;
            }

            let mut pixels = Vec::new();
            let compressed = BASE64.decode(&payload).unwrap();
            flate2::read::ZlibDecoder::new(&compressed[..])
                .read_to_end(&mut pixels)
                .unwrap();
            let number = |key: &str| keys.get(key).map_or(0, |v| v.parse::<u32>().unwrap());
            let (width, height) = (number("s"), number("v"));
            assert_eq!(pixels.len(), (width * height * 4) as usize);
            match keys["a"].as_str() {
                "T" => *image = (width, height, pixels),
                "f" => {
                    assert_eq!((number("i"), number("r"), number("X")), (1, 1, 1));
                    let (x, y) = (number("x"), number("y"));
                    assert!(x + width <= image.0 && y + height <= image.1);
                    for row in 0..height {
                        let to = (((y + row) * image.0 + x) * 4) as usize;
                        let from = (row * width * 4) as usize;
                        let span = (width * 4) as usize;
                        image.2[to..to + span].copy_from_slice(&pixels[from..from + span]);
                    }
                }
                other => panic!("unexpected action {other}"),
            }
            keys.clear();
            payload.clear();
        }
    }

    fn kitty_frames() -> (u32, u32, Vec<u8>, Vec<u8>) {
        let (width, height) = (40u32, 60u32);
        let before: Vec<u8> = (0..width * height)
            .flat_map(|i| [(i % width * 6) as u8, (i / width * 4) as u8, 90, 255])
            .collect();
        let mut after = before.clone();
        // Two changes far enough apart to go in separate rectangles
        for (x, y) in [(3, 5), (30, 7), (12, 44), (13, 45)] {
            let start = ((y * width + x) * 4) as usize;
            after[start..start + 4].copy_from_slice(&[255, 0, 255, 255]);
        }
        (width, height, before, after)
    }

    #[test]
    fn diffed_kitty_frames_edit_the_image() {
        let (width, height, before, after) = kitty_frames();
        let frame = |pixels| Frame {
            width,
            height,
            pixels,
        };
        let frames = [frame(&before), frame(&after), frame(&after)];
        let mut shown = Shown::default();
        let (_, sent) = draw_in_turn(OutputMode::Kitty, &frames, Some(&mut shown), (4, 3));

        let mut image = (0, 0, Vec::new());
        apply_kitty(&mut image, &sent[0]);
        assert_eq!(image, (width, height, before.clone()));

        let edits = String::from_utf8(sent[1].clone()).unwrap();
        assert!(!edits.contains("a=T"), "{edits:?}");
        assert_eq!(edits.matches("\x1b_Ga=f,i=1,r=1,").count(), 2, "{edits:?}");
        assert!(edits.contains("x=3,y=5,s=28,v=3,"), "{edits:?}");
        assert!(edits.contains("x=12,y=44,s=2,v=2,"), "{edits:?}");
        apply_kitty(&mut image, &sent[1]);
        assert_eq!(image, (width, height, after.clone()));

        // An unchanged frame sends no image at all
        assert!(!String::from_utf8_lossy(&sent[2]).contains("\x1b_G"));

        // Clearing the screen deletes the image, so it's sent whole again
        shown.forget();
        let (_, sent) = draw_in_turn(OutputMode::Kitty, &frames[2..], Some(&mut shown), (4, 3));
        assert!(String::from_utf8_lossy(&sent[0]).contains("a=T,"));
    }

    #[test]
    fn diffed_kitty_frames_send_one_shared_memory_object() {
        let (width, height, before, after) = kitty_frames();
        let mut written = Vec::new();
        let mut write = |data: &[u8]| {
            written.push(data.to_vec());
            Ok(format!("/dcat-test-{}", written.len()))
        };
        let mut shown = Shown::default();
        let mut sent = Vec::new();
        for pixels in [&before, &after] {
            let mut out = Vec::new();
            let frame = Frame {
                width,
                height,
                pixels,
            };
            let kitty = KittyImage {
                id: 1,
                write_shared: Some(&mut write),
                placeholders_drawn: None,
            };
            compose_frame(
                OutputMode::Kitty,
                PIXEL_CELL_SIZE,
                frame,
                None,
                (4, 3),
                kitty,
                Some(&mut shown),
                &mut out,
            )
            .unwrap();
            sent.push(String::from_utf8(out).unwrap());
        }

        assert_eq!(written.len(), 2);
        assert_eq!(written[0], before);
        // Covering both changes: columns 3 to 30, rows 5 to 45
        let (x, y, w, h) = (3, 5, 28, 41);
        let expected: Vec<u8> = (y..y + h)
            .flat_map(|row| {
                let start = ((row * width + x) * 4) as usize;
                after[start..start + (w * 4) as usize].to_vec()
            })
            .collect();
        assert_eq!(written[1], expected);
        assert_eq!(sent[1].matches("\x1b_G").count(), 1, "{:?}", sent[1]);
        assert!(
            sent[1].contains(&format!(
                "a=f,i=1,r=1,x={x},y={y},s={w},v={h},X=1,f=32,q=2,t=s,S={};",
                expected.len()
            )),
            "{:?}",
            sent[1]
        );
    }

    #[test]
    fn sixel_keeps_a_spare_row() {
        assert_eq!(reserved_rows(OutputMode::Sixel, false), 1);
        assert_eq!(reserved_rows(OutputMode::Kitty, false), 0);
        assert_eq!(reserved_rows(OutputMode::TruecolorCharacters, true), 1);
    }
}
