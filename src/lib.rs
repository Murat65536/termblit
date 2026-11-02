//! Stream RGBA frames to the terminal, as fast as it can show them.
//!
//! [`detect`] picks the best [`OutputMode`] the terminal supports. A
//! [`Terminal`] takes over the screen and draws frames sized with
//! [`screen_size`], encoding and writing each on a background thread while the
//! next one is made. [`print_frame`] prints a single frame at the cursor
//! instead, sized with [`print_size`].

mod output;
#[cfg(unix)]
mod shm;
mod terminal;

pub use output::{Frame, OutputMode, detect};
pub use terminal::{Options, Terminal, print_frame, print_size, screen_size};
