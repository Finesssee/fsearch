//! History from the NTFS/ReFS change journal (opening a volume needs admin).
//! Every 30 s the daemon notes each drive's journal position next to an event
//! id; a stream starting from an older id reads the journal from the position
//! noted before it and reports the folders that changed, so a restart catches
//! up exactly instead of relisting by mtime.

use super::{current_id, CloseHandle, CreateFileW, Handle, FILE_FLAG_BACKUP_SEMANTICS, INVALID, OPEN_EXISTING, SHARE_ALL};
use std::ffi::c_void;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::OnceLock;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(h: Handle, code: u32, inp: *const c_void, inlen: u32, out: *mut c_void, outlen: u32, ret: *mut u32, ov: *mut c_void) -> i32;
    fn OpenFileById(hint: Handle, id: *const FileId, access: u32, share: u32, sa: *const c_void, flags: u32) -> Handle;
    fn GetFinalPathNameByHandleW(h: Handle, buf: *mut u16, len: u32, flags: u32) -> u32;
}

const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;
const GENERIC_READ: u32 = 0x8000_0000;
/// Positions noted less than this long before the index's id are skipped: a
/// change's watcher event can trail its journal record.
const MARGIN_US: u64 = 10_000_000;
const KEEP: usize = 64;

static DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn set_dir(dir: &std::path::Path) {
    let _ = DIR.set(dir.to_path_buf());
}

#[repr(C)]
struct FileId {
    size: u32,
    kind: u32,
    id: [u8; 16],
}

struct Volume(Handle);

impl Drop for Volume {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open(root: &[u8]) -> Option<Volume> {
    let w: Vec<u16> = format!(r"\\.\{}", String::from_utf8_lossy(root)).encode_utf16().chain([0]).collect();
    let h = unsafe { CreateFileW(w.as_ptr(), GENERIC_READ, SHARE_ALL, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    (h != INVALID).then_some(Volume(h))
}

/// (journal id, first usn, next usn), from USN_JOURNAL_DATA.
fn query(v: &Volume) -> Option<(u64, i64, i64)> {
    let mut out = [0u64; 10];
    let mut n = 0u32;
    let ok = unsafe { DeviceIoControl(v.0, FSCTL_QUERY_USN_JOURNAL, std::ptr::null(), 0, out.as_mut_ptr().cast(), 80, &mut n, std::ptr::null_mut()) };
    (ok != 0).then(|| (out[0], out[1] as i64, out[2] as i64))
}

fn file() -> Option<PathBuf> {
    DIR.get().map(|d| d.join("usn.txt"))
}

/// Note every drive's position now. Lines: `root journal usn id`.
pub fn note(roots: &[Vec<u8>]) {
    let Some(path) = file() else { return };
    let mut lines: Vec<String> = std::fs::read_to_string(&path).unwrap_or_default().lines().map(str::to_string).collect();
    for root in roots {
        let Some((journal, _, next)) = open(root).as_ref().and_then(query) else { continue };
        // Position first, id after: everything past the id is past the position.
        lines.push(format!("{} {journal} {next} {}", String::from_utf8_lossy(root), current_id()));
    }
    let skip = lines.len().saturating_sub(KEEP * roots.len().max(1));
    let _ = std::fs::write(&path, lines[skip..].join("\n") + "\n");
}

/// The folders on `root` that changed since `since`, or None if the journal
/// can't say (no admin, journal off or recreated, wrapped, or never noted).
pub fn changed_since(root: &[u8], since: u64) -> Option<Vec<Vec<u8>>> {
    let v = open(root)?;
    let (journal, first, end) = query(&v)?;
    let name = String::from_utf8_lossy(root);
    let text = std::fs::read_to_string(file()?).ok()?;
    let (_, start) = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            if f.len() != 4 || f[0] != name || f[1].parse::<u64>().ok()? != journal {
                return None;
            }
            Some((f[3].parse::<u64>().ok()?, f[2].parse::<i64>().ok()?))
        })
        .filter(|&(id, _)| id + MARGIN_US <= since)
        .max()?;
    if start < first {
        return None;
    }
    let mut parents = std::collections::HashSet::new();
    let mut buf = vec![0u64; 1 << 17];
    let mut at = start;
    while at < end {
        // READ_USN_JOURNAL_DATA_V1, asking for v2 or v3 records (ReFS ids are 128-bit).
        let mut req = [0u8; 48];
        req[0..8].copy_from_slice(&at.to_le_bytes());
        req[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        req[32..40].copy_from_slice(&journal.to_le_bytes());
        req[40..42].copy_from_slice(&2u16.to_le_bytes());
        req[42..44].copy_from_slice(&3u16.to_le_bytes());
        let mut n = 0u32;
        let ok = unsafe {
            DeviceIoControl(v.0, FSCTL_READ_USN_JOURNAL, req.as_ptr().cast(), 48, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut n, std::ptr::null_mut())
        };
        if ok == 0 || n < 8 {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
        let next = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let mut off = 8;
        while off + 8 <= bytes.len() {
            let rec = &bytes[off..];
            let len = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
            if len == 0 {
                break;
            }
            let major = u16::from_le_bytes(rec[4..6].try_into().unwrap());
            let mut id = [0u8; 16];
            match major {
                2 => id[..8].copy_from_slice(&rec[16..24]),
                3 => id.copy_from_slice(&rec[24..40]),
                _ => {}
            }
            parents.insert((major, id));
            off += len;
        }
        if next <= at {
            break;
        }
        at = next;
    }
    // A parent that's gone was deleted itself: its own record names the
    // folder it was in, so skipping it loses nothing.
    Some(parents.into_iter().filter_map(|(major, id)| resolve(&v, major, id)).collect())
}

fn resolve(v: &Volume, major: u16, id: [u8; 16]) -> Option<Vec<u8>> {
    // FileIdType (64-bit) or ExtendedFileIdType (128-bit).
    let d = FileId { size: 24, kind: if major == 3 { 2 } else { 0 }, id };
    let h = unsafe { OpenFileById(v.0, &d, 0, SHARE_ALL, std::ptr::null(), FILE_FLAG_BACKUP_SEMANTICS) };
    if h == INVALID {
        return None;
    }
    let mut buf = vec![0u16; 32768];
    let n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, 0) } as usize;
    unsafe { CloseHandle(h) };
    if n == 0 || n >= buf.len() {
        return None;
    }
    let s = std::ffi::OsString::from_wide(&buf[..n]);
    let s = s.to_string_lossy();
    Some(crate::os::index_path(std::path::Path::new(s.strip_prefix(r"\\?\").unwrap_or(&s))))
}
