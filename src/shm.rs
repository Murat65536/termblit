// Kitty's shared memory transfer (t=s): frames skip compression, base64 and
// the terminal stream. Only works when the terminal runs on the same machine.

use std::{
    collections::VecDeque,
    io::{self, Write},
    process,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use rustix::{
    event::{PollFd, PollFlags, Timespec},
    fd::OwnedFd,
    fs::Mode,
    shm,
};

const QUERY_TIMEOUT: Duration = Duration::from_secs(1);
// The terminal removes each object once it has read it
const MAX_UNREAD: usize = 2;
// If the terminal reads nothing for this long, shared memory isn't working
const READ_TIMEOUT: Duration = Duration::from_secs(1);
// With skip_unread, how long the oldest unread frame waits before removal
const SKIP_TIMEOUT: Duration = Duration::from_millis(100);

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

pub struct SharedFrames {
    unread: VecDeque<String>,
    skip_unread: bool,
    read_timeout: Duration,
    failed: bool,
}

impl SharedFrames {
    // `skip_unread` is for callers that know the terminal reads shared memory
    // but may drop some frames before it, like dcat.nvim
    pub fn new(skip_unread: bool) -> Self {
        Self {
            unread: VecDeque::new(),
            skip_unread,
            read_timeout: if skip_unread {
                SKIP_TIMEOUT
            } else {
                READ_TIMEOUT
            },
            failed: false,
        }
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<String> {
        if self.failed {
            return Err(io::Error::other(
                "the terminal stopped reading shared memory",
            ));
        }
        self.wait_for_reads()?;

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let name = format!("/dcat-{}-{id}", process::id());
        write_object(&name, data)?;
        self.unread.push_back(name.clone());
        Ok(name)
    }

    // Only a tiny command goes through the terminal stream, so nothing else
    // stops us running ahead of the terminal
    fn wait_for_reads(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + self.read_timeout;
        loop {
            self.unread.retain(|name| exists(name));
            if self.unread.len() < MAX_UNREAD {
                return Ok(());
            }
            if Instant::now() >= deadline && self.skip_unread {
                if let Some(oldest) = self.unread.pop_front() {
                    let _ = shm::unlink(&oldest);
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.failed = true;
                self.remove_unread();
                return Err(io::Error::other(
                    "the terminal stopped reading shared memory",
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn remove_unread(&mut self) {
        for name in self.unread.drain(..) {
            let _ = shm::unlink(&name);
        }
    }
}

impl Drop for SharedFrames {
    fn drop(&mut self) {
        self.remove_unread();
    }
}

fn exists(name: &str) -> bool {
    shm::open(name, shm::OFlags::RDONLY, Mode::empty()).is_ok()
}

fn write_object(name: &str, data: &[u8]) -> io::Result<()> {
    let fd = shm::open(
        name,
        shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
    )?;
    let result = fill(&fd, data);
    if result.is_err() {
        let _ = shm::unlink(name);
    }
    Ok(result?)
}

// Writing directly avoids a fault per page, about a third faster than mmap
#[cfg(target_os = "linux")]
fn fill(fd: &OwnedFd, data: &[u8]) -> rustix::io::Result<()> {
    let mut written = 0;
    while written < data.len() {
        written += rustix::io::write(fd, &data[written..])?;
    }
    Ok(())
}

// macOS can only write shared memory through a mapping
#[cfg(not(target_os = "linux"))]
fn fill(fd: &OwnedFd, data: &[u8]) -> rustix::io::Result<()> {
    use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
    use std::ptr;

    rustix::fs::ftruncate(fd, data.len() as u64)?;
    if data.is_empty() {
        return Ok(());
    }
    // SAFETY: a fresh shared mapping of exactly data.len() bytes, written
    // once and unmapped before anything else can use it
    unsafe {
        let map = mmap(
            ptr::null_mut(),
            data.len(),
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            fd,
            0,
        )?;
        ptr::copy_nonoverlapping(data.as_ptr(), map.cast::<u8>(), data.len());
        munmap(map, data.len())
    }
}

// Must run in raw mode, before anything else reads input
pub fn terminal_supports() -> bool {
    let name = format!("/dcat-{}-probe", process::id());
    if write_object(&name, &[0; 3]).is_err() {
        return false;
    }

    // Every terminal answers the trailing device attributes query, so there's
    // no waiting out the timeout when the first goes unanswered
    let query = format!(
        "\x1b_Gi=31,s=1,v=1,a=q,t=s,f=24;{}\x1b\\\x1b[c",
        BASE64.encode(&name)
    );
    let supported =
        send_and_read_reply(query.as_bytes()).is_ok_and(|reply| contains(&reply, b"\x1b_Gi=31;OK"));

    // The terminal removes objects it reads, but may not have read this one
    let _ = shm::unlink(&name);
    supported
}

fn send_and_read_reply(query: &[u8]) -> io::Result<Vec<u8>> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(query)?;
    stdout.flush()?;

    let stdin = rustix::stdio::stdin();
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut reply = Vec::new();
    while !device_attributes_received(&reply) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let timeout = Timespec::try_from(remaining).unwrap_or_default();
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        if rustix::event::poll(&mut fds, Some(&timeout))? == 0 {
            break;
        }
        let mut chunk = [0; 256];
        let read = rustix::io::read(stdin, &mut chunk)?;
        if read == 0 {
            break;
        }
        reply.extend_from_slice(&chunk[..read]);
    }
    Ok(reply)
}

// The answer to a device attributes query looks like ESC [ ? 62 ; 22 c
fn device_attributes_received(reply: &[u8]) -> bool {
    reply
        .windows(3)
        .position(|window| window == b"\x1b[?")
        .is_some_and(|start| reply[start..].contains(&b'c'))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
    use std::ptr;

    #[test]
    fn frames_are_written_and_cleaned_up() {
        let data: Vec<u8> = (0..=255).collect();
        let name = {
            let mut frames = SharedFrames::new(false);
            let name = frames.write(&data).unwrap();
            assert!(name.starts_with('/') && !name[1..].contains('/'));
            // macOS limits shared memory names to 31 characters
            assert!(name.len() <= 31, "{name}");

            let fd = shm::open(name.as_str(), shm::OFlags::RDONLY, Mode::empty()).unwrap();
            let read = unsafe {
                let map = mmap(
                    ptr::null_mut(),
                    data.len(),
                    ProtFlags::READ,
                    MapFlags::SHARED,
                    &fd,
                    0,
                )
                .unwrap();
                let copy = std::slice::from_raw_parts(map.cast::<u8>(), data.len()).to_vec();
                munmap(map, data.len()).unwrap();
                copy
            };
            assert_eq!(read, data);
            name
        };
        assert!(shm::open(name.as_str(), shm::OFlags::RDONLY, Mode::empty()).is_err());
    }

    // Stands in for the terminal
    fn read_all(names: &[String]) {
        for name in names {
            shm::unlink(name.as_str()).unwrap();
        }
    }

    #[test]
    fn waits_for_the_terminal_to_read() {
        let mut frames = SharedFrames::new(false);
        let first = frames.write(&[1, 2, 3]).unwrap();
        let second = frames.write(&[1, 2, 3]).unwrap();

        let reader = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            read_all(&[first, second]);
        });
        let start = Instant::now();
        let third = frames.write(&[1, 2, 3]).unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "{:?}",
            start.elapsed()
        );
        reader.join().unwrap();
        assert!(exists(&third));
    }

    #[test]
    fn gives_up_when_the_terminal_never_reads() {
        let mut frames = SharedFrames::new(false);
        frames.read_timeout = Duration::from_millis(20);
        let names: Vec<String> = (0..MAX_UNREAD)
            .map(|_| frames.write(&[1, 2, 3]).unwrap())
            .collect();

        assert!(frames.write(&[1, 2, 3]).is_err());
        assert!(names.iter().all(|name| !exists(name)));
        assert!(frames.write(&[1, 2, 3]).is_err());
    }

    #[test]
    fn skips_frames_that_go_unread() {
        let mut frames = SharedFrames::new(true);
        frames.read_timeout = Duration::from_millis(20);
        let mut names: Vec<String> = (0..MAX_UNREAD + 2)
            .map(|_| frames.write(&[1, 2, 3]).unwrap())
            .collect();

        let kept = names.split_off(names.len() - MAX_UNREAD);
        assert!(names.iter().all(|name| !exists(name)));
        assert!(kept.iter().all(|name| exists(name)));
        drop(frames);
        assert!(kept.iter().all(|name| !exists(name)));
    }

    #[test]
    #[ignore = "timing only; run with --release --ignored"]
    fn write_time() {
        let data = vec![30u8; 2560 * 1420 * 4];
        let mut frames = SharedFrames::new(false);
        let start = Instant::now();
        for _ in 0..20 {
            frames.write(&data).unwrap();
        }
        eprintln!(
            "shared memory write: {:?} per 2560x1420 frame",
            start.elapsed() / 20
        );
    }

    #[test]
    fn device_attributes_reply_detection() {
        assert!(device_attributes_received(b"\x1b[?62;22c"));
        assert!(device_attributes_received(b"\x1b_Gi=31;OK\x1b\\\x1b[?1;2c"));
        assert!(!device_attributes_received(b"\x1b_Gi=31;OK\x1b\\"));
        assert!(!device_attributes_received(b"\x1b[?62;22"));
    }
}
