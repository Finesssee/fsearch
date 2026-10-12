//! The daemon: one `Engine`, answering JSON lines over a unix socket (a
//! named pipe on Windows). `fsearch stdio` and the CLI are thin clients.

use fsearch::walk::{KIND_DIR, KIND_FILE, KIND_LINK};
use fsearch::{Engine, Found, GrepMode, Options, Query};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
#[cfg(windows)]
use crate::pipe::{Listener as UnixListener, Pipe as UnixStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(unix)]
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("fsearch.sock")
}

/// One pipe per user and index folder, named by the user's SID and a hash of
/// the folder's path.
#[cfg(windows)]
pub fn socket_path(dir: &Path) -> PathBuf {
    let h = dir.as_os_str().as_encoded_bytes().iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
    let sid = crate::pipe::user_sid().unwrap_or_default();
    PathBuf::from(format!(r"\\.\pipe\fsearch-{sid}-{h:016x}"))
}

pub fn serve(dir: PathBuf, home: String) {
    // One daemon per socket. (The engine's own lock decides who writes the
    // index: an app embedding fsearch may own it while the daemon follows.)
    std::fs::create_dir_all(&dir).ok();
    // Opened without truncating: a loser must not wipe the owner's pid.
    let Ok(mut lock) = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(dir.join("socket.lock")) else { return };
    if !try_lock(&lock) {
        eprintln!("{} another fsearch daemon is running", fsearch::query::now_secs());
        return;
    }
    // The pid lets `stop` find us.
    let _ = lock.set_len(0).and_then(|_| write!(lock, "{}", std::process::id()));
    // A Windows lock also blocks reads, so the pid gets a file of its own.
    #[cfg(windows)]
    let _ = std::fs::write(dir.join("daemon.pid"), std::process::id().to_string());
    let engine = match Engine::start(Options { dir: dir.clone(), home, skip: None }) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{} {e}", fsearch::query::now_secs());
            return;
        }
    };
    let sock = socket_path(&dir);
    #[cfg(unix)]
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind socket");
    for conn in listener.incoming().flatten() {
        let e = engine.clone();
        std::thread::spawn(move || handle(conn, &e));
    }
}

#[cfg(windows)]
fn try_lock(f: &std::fs::File) -> bool {
    f.try_lock().is_ok()
}

#[cfg(unix)]
fn try_lock(f: &std::fs::File) -> bool {
    unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(f), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Stop the daemon serving `dir`, if one is running, and wait until it has
/// let go of the socket lock. One already on its way out (say, after a
/// launchd bootout) is just waited for.
pub fn stop(dir: &Path) -> Result<(), String> {
    let path = dir.join("socket.lock");
    let Ok(lock) = std::fs::File::open(&path) else { return Ok(()) };
    let mut killed = None;
    for _ in 0..300 {
        if try_lock(&lock) {
            return Ok(());
        }
        if killed.is_none() {
            #[cfg(windows)]
            let path = dir.join("daemon.pid");
            killed = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i32>().ok()).filter(|&p| p > 1);
            if let Some(pid) = killed {
                kill(pid);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(match killed {
        Some(pid) => format!("daemon {pid} did not exit"),
        None => "a daemon is running but did not record its pid; stop it by hand".into(),
    })
}

#[cfg(unix)]
fn kill(pid: i32) {
    unsafe { libc::kill(pid, libc::SIGTERM) };
}

#[cfg(windows)]
fn kill(pid: i32) {
    let null = std::process::Stdio::null;
    let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).stdout(null()).stderr(null()).status();
}

/// How long the cores stay clocked up after an answer to a client that is
/// still connected (typing): most gaps between keystrokes are shorter.
const WARM: Duration = Duration::from_millis(250);

fn handle(mut conn: UnixStream, engine: &Engine) {
    // Content searches run on this thread: keep them on performance cores.
    fsearch::os::set_qos(fsearch::os::Qos::Interactive);
    let Ok(r) = conn.try_clone() else { return };
    let mut out = Vec::new();
    for line in BufReader::new(r).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        out.clear();
        respond(&line, engine, &mut out);
        if conn.write_all(&out).is_err() {
            return;
        }
        if waiting(&conn) {
            engine.keep_warm(WARM);
        }
    }
}

/// The client is still connected with nothing more to ask yet: a session
/// whose next request is a keystroke away. (One-shot clients like the CLI
/// close their side as soon as the request is out.)
#[cfg(windows)]
fn waiting(_: &UnixStream) -> bool {
    false
}

#[cfg(unix)]
fn waiting(conn: &UnixStream) -> bool {
    let mut b = 0u8;
    let n = unsafe { libc::recv(std::os::fd::AsRawFd::as_raw_fd(conn), (&raw mut b).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
}

/// The answer to one request line, as one line of JSON.
fn respond(line: &str, engine: &Engine, out: &mut Vec<u8>) {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return put(out, &json!({"ok": false, "error": format!("bad json: {e}")})),
    };
    let id = v.get("id").unwrap_or(&Value::Null);
    match run(&v, engine) {
        Ok(Reply::Hits(found, took_us)) => {
            let hits = found
                .iter()
                .map(|f| Hit { kind: kind_name(f.kind), mtime: f.mtime, path: f.path.to_string_lossy(), score: f.score, size: f.size })
                .collect();
            put(out, &Hits { hits, id, ok: true, took_us })
        }
        Ok(Reply::Value(mut r)) => {
            r["id"] = id.clone();
            put(out, &r)
        }
        Err(e) => put(out, &json!({"ok": false, "error": e, "id": id})),
    }
}

fn put(out: &mut Vec<u8>, v: &impl serde::Serialize) {
    serde_json::to_writer(&mut *out, v).expect("json");
    out.push(b'\n');
}

enum Reply {
    Value(Value),
    /// Name-search results and the search's own time (µs): the hot path,
    /// written straight from the results.
    Hits(Vec<Found>, u64),
}

/// The JSON of a name-search answer. Keys in sorted order, the order every
/// other answer (a `Value` object) prints in.
#[derive(serde::Serialize)]
struct Hits<'a> {
    hits: Vec<Hit<'a>>,
    id: &'a Value,
    ok: bool,
    took_us: u64,
}

#[derive(serde::Serialize)]
struct Hit<'a> {
    kind: &'static str,
    mtime: u32,
    path: std::borrow::Cow<'a, str>,
    score: i32,
    size: u64,
}

fn run(v: &Value, engine: &Engine) -> Result<Reply, String> {
    let op = v.get("op").and_then(Value::as_str).unwrap_or("search");
    let is_grep = op == "grep"
        || (op == "search"
            && v.get("q").and_then(Value::as_str).is_some_and(|q| ["grep:", "regex:", "sym:", "content:", "symbol:"].iter().any(|k| q.contains(k))));
    match op {
        "ping" => Ok(Reply::Value(json!({"ok": true}))),
        "save" => {
            engine.save();
            Ok(Reply::Value(json!({"ok": true, "scheduled": true})))
        }
        _ if is_grep => grep(v, engine).map(Reply::Value),
        "status" => {
            let s = engine.status();
            if !s.ready {
                return Err(engine.indexing());
            }
            let mut v = serde_json::to_value(s).map_err(|e| e.to_string())?;
            v["ok"] = true.into();
            Ok(Reply::Value(v))
        }
        "search" => {
            let q = parse_request(v, engine.home())?;
            let t = Instant::now();
            let found = engine.search(&q)?;
            Ok(Reply::Hits(found, t.elapsed().as_micros() as u64))
        }
        _ => Err(format!("unknown op {op}")),
    }
}

/// Content search. The pattern comes from `pattern` (+ `mode`) or from a
/// `grep:`/`regex:`/`sym:` filter in `q`; the rest of the query narrows
/// which files are read.
fn grep(v: &Value, engine: &Engine) -> Result<Value, String> {
    let mut q = parse_request(v, engine.home())?;
    let mode = match v.get("mode").and_then(Value::as_str) {
        Some("regex") => GrepMode::Regex,
        Some("symbol") => GrepMode::Symbol,
        Some("literal") => GrepMode::Literal,
        Some(m) => return Err(format!("unknown mode {m}")),
        None => q.grep_mode,
    };
    let pattern = v.get("pattern").and_then(Value::as_str).map(str::to_string).or(q.grep.take()).ok_or("grep needs a pattern")?;
    let mut g = fsearch::Grep::new(&pattern, mode)?;
    if let Some(n) = v.get("per_file").and_then(Value::as_u64) {
        g.max_per_file = n as usize;
    }
    if let Some(ms) = v.get("budget_ms").and_then(Value::as_u64) {
        g.budget = (ms > 0).then(|| Duration::from_millis(ms));
    }
    let t = Instant::now();
    let (r, indexed) = engine.grep(&q, &g)?;
    let files: Vec<Value> = r
        .files
        .iter()
        .map(|f| {
            json!({
                "path": String::from_utf8_lossy(&f.path),
                "matches": f.lines.iter().map(|(n, t)| json!({"line": n, "text": t})).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "took_us": t.elapsed().as_micros() as u64,
        "source": if indexed { "index" } else { "scan" },
        "candidates": r.candidates,
        "read": r.read,
        "complete": r.complete,
        "indexing": engine.status().content_pending,
        "files": files,
    }))
}

/// `q` is the query language; any filter key may also be given as its own
/// JSON field (`{"q": "main", "ext": "rs", "in": "~/Developer"}`).
fn parse_request(v: &Value, home: &str) -> Result<Query, String> {
    let mut q = Query::parse(v.get("q").and_then(Value::as_str).unwrap_or(""), home)?;
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if k == "limit" {
                q.limit = val.as_u64().ok_or("limit must be a number")? as usize;
            } else {
                let s = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                q.filter(k, &s, home)?;
            }
        }
    }
    Ok(q)
}

fn kind_name(k: u8) -> &'static str {
    match k & 3 {
        KIND_FILE => "file",
        KIND_DIR => "dir",
        KIND_LINK => "link",
        _ => "other",
    }
}

/// Connect to the daemon, starting it if it isn't running.
pub fn connect(dir: &Path) -> std::io::Result<UnixStream> {
    let sock = socket_path(dir);
    if let Ok(s) = dial(&sock) {
        return Ok(s);
    }
    spawn_daemon(dir)?;
    for _ in 0..1500 {
        std::thread::sleep(Duration::from_millis(2));
        if let Ok(s) = dial(&sock) {
            return Ok(s);
        }
    }
    dial(&sock)
}

#[cfg(unix)]
fn dial(sock: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(sock)
}

#[cfg(windows)]
fn dial(sock: &Path) -> std::io::Result<UnixStream> {
    crate::pipe::connect(sock)
}

#[cfg(unix)]
fn spawn_daemon(dir: &Path) -> std::io::Result<()> {
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    // Own session: closing the terminal that started it doesn't kill it.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.arg("serve").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?;
    Ok(())
}

/// Started with no handles inherited at all: `Command` passes every
/// inheritable handle on, and a daemon holding a pipe of the shell that ran
/// the client keeps `fsearch x | findstr y` waiting for EOF. The daemon
/// opens its own log (`serve --log`).
#[cfg(windows)]
fn spawn_daemon(_dir: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[repr(C)]
    struct StartupInfo {
        cb: u32,
        rest: [u8; 100],
    }
    #[repr(C)]
    struct ProcessInfo {
        process: *mut std::ffi::c_void,
        thread: *mut std::ffi::c_void,
        pid: u32,
        tid: u32,
    }
    unsafe extern "system" {
        fn CreateProcessW(
            app: *const u16,
            cmd: *mut u16,
            pa: *const std::ffi::c_void,
            ta: *const std::ffi::c_void,
            inherit: i32,
            flags: u32,
            env: *const std::ffi::c_void,
            cwd: *const u16,
            si: *const StartupInfo,
            pi: *mut ProcessInfo,
        ) -> i32;
        fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
    }
    let exe = std::env::current_exe()?;
    let mut line: Vec<u16> = format!("\"{}\" serve --log", exe.display()).encode_utf16().chain([0]).collect();
    let app: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let si = StartupInfo { cb: std::mem::size_of::<StartupInfo>() as u32, rest: [0; 100] };
    let mut pi = ProcessInfo { process: std::ptr::null_mut(), thread: std::ptr::null_mut(), pid: 0, tid: 0 };
    // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP: no console, and closing the
    // terminal doesn't kill it.
    let ok = unsafe { CreateProcessW(app.as_ptr(), line.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, 0x0800_0000 | 0x0000_0200, std::ptr::null(), std::ptr::null(), &si, &mut pi) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        CloseHandle(pi.thread);
        CloseHandle(pi.process);
    }
    Ok(())
}

/// Point stdout and stderr at `daemon.log`, for a daemon started with no
/// handles.
#[cfg(windows)]
pub fn log_to(dir: &Path) {
    use std::os::windows::io::IntoRawHandle;
    unsafe extern "system" {
        fn SetStdHandle(n: u32, h: *mut std::ffi::c_void) -> i32;
    }
    std::fs::create_dir_all(dir).ok();
    let Ok(log) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("daemon.log")) else { return };
    let Ok(dup) = log.try_clone() else { return };
    unsafe {
        SetStdHandle(-11i32 as u32, log.into_raw_handle());
        SetStdHandle(-12i32 as u32, dup.into_raw_handle());
    }
}
