//! The few OS differences the engine sees. Index paths are bytes with `/`
//! separators rooted at `/`; on Windows a drive is a top-level folder, so
//! `D:\code` is `/D:/code` inside the index.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub enum Qos {
    Interactive,
    Initiated,
    Utility,
}

#[cfg(target_os = "macos")]
pub fn set_qos(q: Qos) {
    let c = match q {
        Qos::Interactive => libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE,
        Qos::Initiated => libc::qos_class_t::QOS_CLASS_USER_INITIATED,
        Qos::Utility => libc::qos_class_t::QOS_CLASS_UTILITY,
    };
    unsafe { libc::pthread_set_qos_class_self_np(c, 0) };
}

#[cfg(not(target_os = "macos"))]
pub fn set_qos(_: Qos) {}

#[cfg(unix)]
pub fn os_path(p: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(p))
}

#[cfg(unix)]
pub fn index_path(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
pub fn os_path(p: &[u8]) -> PathBuf {
    let p = p.strip_prefix(b"/").unwrap_or(p);
    let mut p = p.to_vec();
    // `D:` alone is the drive's current directory, not its root.
    if p.len() == 2 && p[1] == b':' {
        p.push(b'/');
    }
    // Index bytes come from `as_encoded_bytes` (WTF-8) or ASCII edits of it.
    PathBuf::from(unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(p) })
}

#[cfg(windows)]
pub fn index_path(p: &Path) -> Vec<u8> {
    let b = p.as_os_str().as_encoded_bytes();
    let b = b.strip_prefix(br"\\?\").unwrap_or(b);
    let mut out = Vec::with_capacity(b.len() + 1);
    if !b.starts_with(b"/") {
        out.push(b'/');
    }
    out.extend(b.iter().map(|&c| if c == b'\\' { b'/' } else { c }));
    while out.len() > 1 && out.last() == Some(&b'/') {
        out.pop();
    }
    out
}

/// The disk mtime as whole seconds since the epoch, clamped to u32.
pub fn mtime_secs(m: &std::fs::Metadata) -> u32 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}
