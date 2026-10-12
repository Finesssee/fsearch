//! Change feed on Windows: one recursive ReadDirectoryChangesW per indexed
//! drive, reported like FSEvents (the folder that changed). It has no
//! history, so a stream started from an older id asks for a relist of the
//! folders changed since the last sync, and a buffer overflow does the same.
//! (The NTFS USN journal has history, but reading it needs admin rights.)

use std::ffi::c_void;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};

pub const MUST_SCAN_SUBDIRS: u32 = 0x1;
pub const USER_DROPPED: u32 = 0x2;
pub const KERNEL_DROPPED: u32 = 0x4;
pub const HISTORY_DONE: u32 = 0x10;

pub struct Event {
    pub path: Vec<u8>,
    pub flags: u32,
    pub id: u64,
    pub stream: u32,
}

type Handle = *mut c_void;

#[path = "usn_win.rs"]
mod usn;
pub use usn::set_dir;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *const c_void, disposition: u32, flags: u32, template: Handle) -> Handle;
    fn ReadDirectoryChangesW(
        dir: Handle,
        buf: *mut c_void,
        len: u32,
        subtree: i32,
        filter: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
        completion: *const c_void,
    ) -> i32;
    fn CancelIoEx(h: Handle, overlapped: *mut c_void) -> i32;
    fn CloseHandle(h: Handle) -> i32;
}

const FILE_LIST_DIRECTORY: u32 = 0x1;
const SHARE_ALL: u32 = 0x7;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const INVALID: Handle = -1isize as Handle;
// File and folder names, attributes, size, last write.
const FILTER: u32 = 0x1F;
// Local volumes take more than 64 KB; a bigger buffer overflows less often.
const BUF: usize = 1 << 20;

/// Ids are microseconds since the epoch, so a saved index's id is older
/// than anything a later process hands out.
pub fn current_id() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64);
    LAST.fetch_max(now, Ordering::Relaxed).max(now)
}

fn started() -> u64 {
    static AT: OnceLock<u64> = OnceLock::new();
    *AT.get_or_init(current_id)
}

/// A running stream; dropping it stops it.
pub struct Stream {
    stop: Arc<AtomicBool>,
    dirs: Vec<usize>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for &h in &self.dirs {
            unsafe { CancelIoEx(h as Handle, std::ptr::null_mut()) };
        }
    }
}

/// Watch every indexed drive. Starting from an id this process didn't hand
/// out means changes were missed while nothing watched: `/` must be rescanned.
pub fn watch(since: u64, _latency: f64, tx: Sender<Vec<Event>>, stream: u32) -> Stream {
    let first = started();
    let roots = crate::walk::roots();
    let stop = Arc::new(AtomicBool::new(false));
    let mut dirs = Vec::new();
    for root in roots.clone() {
        let mut wide: Vec<u16> = std::ffi::OsStr::new(&format!(r"\\?\{}\", String::from_utf8_lossy(&root))).encode_wide().collect();
        wide.push(0);
        let h = unsafe { CreateFileW(wide.as_ptr(), FILE_LIST_DIRECTORY, SHARE_ALL, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut()) };
        if h == INVALID {
            crate::engine::log(format!("cannot watch {}: {}", String::from_utf8_lossy(&root), std::io::Error::last_os_error()));
            continue;
        }
        dirs.push(h as usize);
        let (tx, stop, h) = (tx.clone(), stop.clone(), h as usize);
        std::thread::Builder::new().name("fsearch-watch".into()).spawn(move || read_loop(h, &root, &tx, &stop, stream)).unwrap();
    }
    // History after the watchers are up, so nothing falls between the two.
    let mut start = Vec::new();
    if since < first {
        let t = std::time::Instant::now();
        let mut changed = Vec::new();
        if roots.iter().all(|r| usn::changed_since(r, since).map(|d| changed.extend(d)).is_some()) {
            crate::engine::log(format!("change journal: {} folders changed since {since} ({:.2?})", changed.len(), t.elapsed()));
            let id = current_id();
            start.extend(changed.into_iter().map(|path| Event { path, flags: 0, id, stream }));
        } else {
            start.push(Event { path: b"/".to_vec(), flags: MUST_SCAN_SUBDIRS, id: current_id(), stream });
        }
    }
    start.push(Event { path: b"/".to_vec(), flags: HISTORY_DONE, id: current_id(), stream });
    let _ = tx.send(start);
    static NOTING: std::sync::Once = std::sync::Once::new();
    NOTING.call_once(|| {
        std::thread::Builder::new()
            .name("fsearch-usn".into())
            .spawn(move || loop {
                usn::note(&roots);
                std::thread::sleep(std::time::Duration::from_secs(30));
            })
            .unwrap();
    });
    Stream { stop, dirs }
}

fn read_loop(h: usize, root: &[u8], tx: &Sender<Vec<Event>>, stop: &AtomicBool, stream: u32) {
    // u32-aligned, as FILE_NOTIFY_INFORMATION needs.
    let mut buf = vec![0u32; BUF / 4];
    let root_os = std::path::PathBuf::from(format!("{}\\", String::from_utf8_lossy(root)));
    while !stop.load(Ordering::SeqCst) {
        let mut n = 0u32;
        let ok = unsafe { ReadDirectoryChangesW(h as Handle, buf.as_mut_ptr().cast(), BUF as u32, 1, FILTER, &mut n, std::ptr::null_mut(), std::ptr::null()) };
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if ok == 0 || n == 0 {
            // Overflow (n == 0) or a failed read: what changed is unknown.
            let _ = tx.send(vec![Event { path: b"/".to_vec(), flags: MUST_SCAN_SUBDIRS | KERNEL_DROPPED, id: current_id(), stream }]);
            if ok == 0 {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            continue;
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
        let mut seen = std::collections::HashSet::new();
        let mut off = 0usize;
        loop {
            let rec = &bytes[off..];
            let next = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(rec[8..12].try_into().unwrap()) as usize;
            let name: Vec<u16> = rec[12..12 + len].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let full = root_os.join(std::ffi::OsString::from_wide(&name));
            let mut p = crate::os::index_path(&full);
            if let Some(slash) = p.iter().rposition(|&b| b == b'/') {
                p.truncate(slash.max(1));
            }
            seen.insert(p);
            if next == 0 {
                break;
            }
            off += next;
        }
        let id = current_id();
        let _ = tx.send(seen.into_iter().map(|path| Event { path, flags: 0, id, stream }).collect());
    }
    unsafe { CloseHandle(h as Handle) };
}
