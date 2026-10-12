//! A named pipe that reads and writes like a unix socket: a connection is a
//! `File`, and the listener hands out one per client.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;

pub type Pipe = std::fs::File;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(name: *const u16, open: u32, mode: u32, max: u32, out: u32, inp: u32, timeout: u32, sa: *const c_void) -> *mut c_void;
    fn ConnectNamedPipe(h: *mut c_void, overlapped: *mut c_void) -> i32;
    fn CloseHandle(h: *mut c_void) -> i32;
}

const PIPE_ACCESS_DUPLEX: u32 = 0x3;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x8;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const ERROR_PIPE_CONNECTED: i32 = 535;
const ERROR_PIPE_BUSY: i32 = 231;

pub struct Listener {
    name: Vec<u16>,
    /// The instance waiting for the next client. One always exists, so a
    /// client never finds the name missing while the daemon runs.
    next: std::cell::Cell<*mut c_void>,
}

impl Listener {
    pub fn bind(name: &Path) -> std::io::Result<Listener> {
        let mut w: Vec<u16> = name.as_os_str().encode_wide().collect();
        w.push(0);
        // The first instance fails if another process already owns the name.
        let first = create(&w, FILE_FLAG_FIRST_PIPE_INSTANCE)?;
        Ok(Listener { name: w, next: std::cell::Cell::new(first) })
    }

    pub fn incoming(&self) -> impl Iterator<Item = std::io::Result<Pipe>> + '_ {
        std::iter::from_fn(move || Some(self.accept()))
    }

    fn accept(&self) -> std::io::Result<Pipe> {
        let h = self.next.get();
        let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0
            || std::io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED);
        self.next.set(create(&self.name, 0)?);
        if !connected {
            unsafe { CloseHandle(h) };
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { Pipe::from_raw_handle(h) })
    }
}

fn create(name: &[u16], flags: u32) -> std::io::Result<*mut c_void> {
    let h = unsafe {
        CreateNamedPipeW(name.as_ptr(), PIPE_ACCESS_DUPLEX | flags, PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, 1 << 16, 1 << 16, 0, std::ptr::null())
    };
    if h as isize == -1 { Err(std::io::Error::last_os_error()) } else { Ok(h) }
}

/// Connect to a listening pipe, waiting out a moment when every instance is busy.
pub fn connect(name: &Path) -> std::io::Result<Pipe> {
    for _ in 0..500 {
        match std::fs::OpenOptions::new().read(true).write(true).open(name) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => std::thread::sleep(std::time::Duration::from_millis(2)),
            r => return r,
        }
    }
    std::fs::OpenOptions::new().read(true).write(true).open(name)
}
