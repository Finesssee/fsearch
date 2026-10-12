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
    fn GetCurrentProcess() -> *mut c_void;
    fn LocalFree(p: *mut c_void) -> *mut c_void;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(token: *mut c_void, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, out: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl: *const u16, rev: u32, sd: *mut *mut c_void, len: *mut u32) -> i32;
    fn GetSecurityInfo(h: *mut c_void, kind: u32, info: u32, owner: *mut *mut c_void, group: *mut *mut c_void, dacl: *mut *mut c_void, sacl: *mut *mut c_void, sd: *mut *mut c_void) -> u32;
}

#[repr(C)]
struct SecurityAttributes {
    len: u32,
    sd: *mut c_void,
    inherit: i32,
}

const TOKEN_QUERY: u32 = 0x8;
const TOKEN_USER: u32 = 1;
const SE_KERNEL_OBJECT: u32 = 6;
const OWNER_SECURITY_INFORMATION: u32 = 1;

const PIPE_ACCESS_DUPLEX: u32 = 0x3;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x8;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const ERROR_PIPE_CONNECTED: i32 = 535;
const ERROR_PIPE_BUSY: i32 = 231;

pub struct Listener {
    name: Vec<u16>,
    /// Owned by this user, and only this user may open it. Lives as long as
    /// the listener; it is never freed.
    sd: *mut c_void,
    /// The instance waiting for the next client. One always exists, so a
    /// client never finds the name missing while the daemon runs.
    next: std::cell::Cell<*mut c_void>,
}

impl Listener {
    pub fn bind(name: &Path) -> std::io::Result<Listener> {
        let mut w: Vec<u16> = name.as_os_str().encode_wide().collect();
        w.push(0);
        let sid = user_sid()?;
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})").encode_utf16().chain([0]).collect();
        let mut sd = std::ptr::null_mut();
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut sd, std::ptr::null_mut()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The first instance fails if another process already owns the name.
        let first = create(&w, FILE_FLAG_FIRST_PIPE_INSTANCE, sd)?;
        Ok(Listener { name: w, sd, next: std::cell::Cell::new(first) })
    }

    pub fn incoming(&self) -> impl Iterator<Item = std::io::Result<Pipe>> + '_ {
        std::iter::from_fn(move || Some(self.accept()))
    }

    fn accept(&self) -> std::io::Result<Pipe> {
        let h = self.next.get();
        let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0
            || std::io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED);
        self.next.set(create(&self.name, 0, self.sd)?);
        if !connected {
            unsafe { CloseHandle(h) };
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { Pipe::from_raw_handle(h) })
    }
}

fn create(name: &[u16], flags: u32, sd: *mut c_void) -> std::io::Result<*mut c_void> {
    let sa = SecurityAttributes { len: std::mem::size_of::<SecurityAttributes>() as u32, sd, inherit: 0 };
    let h = unsafe {
        CreateNamedPipeW(name.as_ptr(), PIPE_ACCESS_DUPLEX | flags, PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, 1 << 16, 1 << 16, 0, &sa as *const _ as *const c_void)
    };
    if h as isize == -1 { Err(std::io::Error::last_os_error()) } else { Ok(h) }
}

/// The current user's SID, as `S-1-5-21-...`.
pub fn user_sid() -> std::io::Result<String> {
    static SID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(s) = SID.get() {
        return Ok(s.clone());
    }
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // TOKEN_USER is a SID pointer followed by the SID it points into.
    let mut buf = [0u64; 64];
    let mut len = 0;
    let ok = unsafe { GetTokenInformation(token, TOKEN_USER, buf.as_mut_ptr().cast(), std::mem::size_of_val(&buf) as u32, &mut len) };
    let err = std::io::Error::last_os_error();
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(err);
    }
    let sid = sid_string(buf[0] as *mut c_void)?;
    Ok(SID.get_or_init(|| sid).clone())
}

fn sid_string(sid: *mut c_void) -> std::io::Result<String> {
    let mut p = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut p) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let n = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) });
    unsafe { LocalFree(p.cast()) };
    Ok(s)
}

/// Connect to a listening pipe, waiting out a moment when every instance is
/// busy. The pipe must be owned by this user: anyone can create a pipe under
/// the expected name first, and queries must not go to them.
pub fn connect(name: &Path) -> std::io::Result<Pipe> {
    let open = || std::fs::OpenOptions::new().read(true).write(true).open(name);
    let mut r = open();
    for _ in 0..500 {
        match r {
            Err(ref e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => std::thread::sleep(std::time::Duration::from_millis(2)),
            _ => break,
        }
        r = open();
    }
    let pipe = r?;
    if owner(&pipe)? != user_sid()? {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "fsearch pipe is owned by another user"));
    }
    Ok(pipe)
}

fn owner(pipe: &Pipe) -> std::io::Result<String> {
    use std::os::windows::io::AsRawHandle;
    let (mut sid, mut sd) = (std::ptr::null_mut(), std::ptr::null_mut());
    let null = std::ptr::null_mut();
    let rc = unsafe { GetSecurityInfo(pipe.as_raw_handle().cast(), SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION, &mut sid, null, null, null, &mut sd) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    let s = sid_string(sid);
    unsafe { LocalFree(sd) };
    s
}
