mod server;
#[cfg(windows)]
mod pipe;

use fsearch::{index, live, query};

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Instant;

/// Big buffers (index builds, content batches) come straight from mmap and
/// go straight back with munmap. macOS's malloc keeps freed large blocks
/// mapped and dirty, which left the daemon at ~1 GB footprint after a build
/// while only ~2 MB was live.
#[cfg(target_os = "macos")]
struct Alloc;
#[cfg(target_os = "macos")]
const BIG: usize = 1 << 20;

#[cfg(target_os = "macos")]
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() < BIG || l.align() > 16384 {
            return unsafe { System.alloc(l) };
        }
        let p = unsafe { libc::mmap(std::ptr::null_mut(), l.size(), libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
        if p == libc::MAP_FAILED { std::ptr::null_mut() } else { p as *mut u8 }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // Fresh anonymous pages are already zero.
        if l.size() < BIG || l.align() > 16384 { unsafe { System.alloc_zeroed(l) } } else { unsafe { self.alloc(l) } }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if l.size() < BIG || l.align() > 16384 {
            unsafe { System.dealloc(p, l) }
        } else {
            unsafe { libc::munmap(p as *mut libc::c_void, l.size()) };
        }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        if (l.size() < BIG && new_size < BIG) || l.align() > 16384 {
            return unsafe { System.realloc(p, l, new_size) };
        }
        let q = unsafe { self.alloc(Layout::from_size_align_unchecked(new_size, l.align())) };
        if !q.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(p, q, l.size().min(new_size));
                self.dealloc(p, l);
            }
        }
        q
    }
}

#[cfg(target_os = "macos")]
#[global_allocator]
static GLOBAL: Alloc = Alloc;

#[cfg(target_os = "macos")]
const USAGE: &str = "usage:
  fsearch <query...> [--json]   search (starts the daemon if needed)
  fsearch stdio                 JSON lines on stdin/stdout
  fsearch serve                 run the daemon in the foreground
  fsearch status
  fsearch install [--login]      copy to ~/.local/bin; --login also starts the daemon at login
                                (needs Full Disk Access granted to ~/.local/bin/fsearch)
  fsearch uninstall             remove the login agent (keeps the index)
  fsearch bench <query...>      time a query in-process against the saved index";

#[cfg(target_os = "macos")]
const LABEL: &str = "mt.nd.fsearch";

#[cfg(target_os = "macos")]
fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

/// The content-indexed folder, as an index path (`/D:/code`). `/` means
/// every indexed drive: the empty prefix, which every path starts with.
#[cfg(windows)]
fn home() -> String {
    let h = std::env::var_os("FSEARCH_HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
    if h == "/" {
        return String::new();
    }
    String::from_utf8_lossy(&fsearch::os::index_path(std::path::Path::new(&h))).into_owned()
}

#[cfg(windows)]
fn data_dir() -> PathBuf {
    let d = fsearch::default_dir(&home());
    std::fs::create_dir_all(&d).ok();
    d
}

#[cfg(windows)]
const USAGE: &str = "usage:
  fsearch <query...> [--json] [--all]
                                search (starts the daemon if needed); --all lists every
                                match instead of what 250 ms finds
  fsearch stdio                 JSON lines on stdin/stdout
  fsearch serve                 run the daemon in the foreground
  fsearch status
  fsearch stop                  stop the daemon (keeps the index)
  fsearch install [--login]     copy to ~/.local/bin; --login also starts the daemon at login
                                and saves FSEARCH_ROOTS/FSEARCH_HOME as user variables
  fsearch uninstall             remove the login entry (keeps the binary and the index)
  fsearch index                 build the name and content index in-process, then exit
  fsearch bench <query...>      time a query in-process against the saved index
env: FSEARCH_ROOTS=C:;D: drives for the name index (default: every fixed drive),
     FSEARCH_HOME=D:/code folder for content (/ for every drive; default: the user profile),
     FSEARCH_RUNS=20 bench repetitions";

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help") => eprintln!("{USAGE}"),
        Some("serve") => {
            if args.iter().any(|a| a == "--log") {
                server::log_to(&data_dir());
            }
            server::serve(data_dir(), home())
        }
        Some("stdio") => stdio(),
        Some("status") => print_one(&serde_json::json!({"op": "status"}), true),
        Some("stop") => server::stop(&data_dir()).unwrap_or_else(|e| die(&e)),
        Some("index") => build_index(),
        Some("bench") => bench(&args[1..].join(" ")),
        Some("install") => install(args.iter().any(|a| a == "--login")),
        Some("uninstall") => uninstall(),
        Some(_) => {
            let json = args.iter().any(|a| a == "--json");
            let q: Vec<&str> = args.iter().map(String::as_str).filter(|a| !matches!(*a, "--json" | "--all")).collect();
            let mut req = serde_json::json!({"q": q.join(" ")});
            if args.iter().any(|a| a == "--all") {
                req["budget_ms"] = 0.into();
                req["limit"] = u32::MAX.into();
            }
            print_one(&req, json);
        }
    }
}

/// A pipe can't be half-closed, and a synchronous read would block a write
/// on the same handle, so requests go one at a time: each gets one line back.
#[cfg(windows)]
fn stdio() {
    let s = server::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    let mut up = s.try_clone().unwrap();
    let mut down = BufReader::new(s);
    let mut out = std::io::stdout().lock();
    let mut reply = String::new();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        reply.clear();
        if writeln!(up, "{line}").is_err() || down.read_line(&mut reply).unwrap_or(0) == 0 {
            break;
        }
        if out.write_all(reply.as_bytes()).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

#[cfg(windows)]
fn build_index() {
    let t = Instant::now();
    let e = fsearch::Engine::start(fsearch::Options { dir: data_dir(), home: home(), skip: Some(Vec::new()) }).unwrap_or_else(|e| die(&e));
    while !fsearch::CONTENT_SYNCED.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let s = e.status();
    eprintln!(
        "indexed {} entries, {} content docs ({:.0} MB content, {:.0} MB names) in {:.2?}",
        s.entries,
        s.content_docs,
        s.content_bytes as f64 / 1e6,
        s.index_bytes as f64 / 1e6,
        t.elapsed()
    );
}

#[cfg(target_os = "macos")]
fn data_dir() -> PathBuf {
    let d = PathBuf::from(home()).join("Library/Application Support/FSearch");
    std::fs::create_dir_all(&d).ok();
    d
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
}

#[cfg(target_os = "macos")]
fn main() {
    // Never let a search download iCloud placeholders: opening or listing a
    // dataless file/dir fails fast instead of materializing it.
    // (IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_PROCESS, OFF)
    unsafe { setiopolicy_np(3, 0, 1) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help") => eprintln!("{USAGE}"),
        Some("serve") => server::serve(data_dir(), home()),
        Some("stdio") => stdio(),
        Some("status") => print_one(&serde_json::json!({"op": "status"}), true),
        Some("bench") => bench(&args[1..].join(" ")),
        Some("install") => install(args.iter().any(|a| a == "--login")),
        Some("uninstall") => uninstall(),
        Some(_) => {
            let json = args.iter().any(|a| a == "--json");
            let q: Vec<&str> = args.iter().map(String::as_str).filter(|a| *a != "--json").collect();
            print_one(&serde_json::json!({"q": q.join(" ")}), json);
        }
    }
}

fn print_one(req: &serde_json::Value, raw: bool) {
    let mut s = server::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    s.write_all(format!("{req}\n").as_bytes()).unwrap();
    // One request only: the daemon needn't keep the cores warm for a next one.
    #[cfg(unix)]
    let _ = s.shutdown(std::net::Shutdown::Write);
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).unwrap();
    if raw {
        print!("{line}");
        return;
    }
    let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    if v["ok"] != true {
        die(v["error"].as_str().unwrap_or("error"));
    }
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for h in v["hits"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "{}", h["path"].as_str().unwrap_or(""));
    }
    for f in v["files"].as_array().into_iter().flatten() {
        for m in f["matches"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "{}:{}: {}", f["path"].as_str().unwrap_or(""), m["line"], m["text"].as_str().unwrap_or("").trim());
        }
    }
    let _ = out.flush();
}

#[cfg(target_os = "macos")]
fn stdio() {
    let s = server::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    let mut up = s.try_clone().unwrap();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if writeln!(up, "{line}").is_err() {
                break;
            }
        }
        let _ = up.shutdown(std::net::Shutdown::Write);
    });
    let mut out = std::io::stdout().lock();
    for line in BufReader::new(s).lines() {
        let Ok(line) = line else { break };
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

fn bench(qs: &str) {
    let dir = data_dir();
    let idx = index::Index::load(&dir.join("index.bin")).unwrap_or_else(|| die("no index yet; run fsearch serve"));
    let live = live::Live::new(idx);
    let home = home();
    let mut q = query::Query::parse(qs, &home).unwrap_or_else(|e| die(&e));
    // Like the daemon: a content search when the query has a pattern, on
    // user-interactive threads, paths included.
    // The daemon's 250 ms budget returns partial results; a benchmark wants all of them.
    let grep = q.grep.take().map(|p| {
        let mut g = fsearch::Grep::new(&p, q.grep_mode).unwrap_or_else(|e| die(&e));
        g.budget = None;
        g
    });
    let stats = std::cell::Cell::new((0, 0, true));
    let content = grep.as_ref().map(|_| fsearch::content::Content::open_shared(dir.join("content")));
    let pool = rayon::ThreadPoolBuilder::new()
        .start_handler(|_| fsearch::os::set_qos(fsearch::os::Qos::Interactive))
        .build()
        .unwrap();
    let run = || -> Vec<String> {
        let (Some(g), Some(c)) = (&grep, &content) else {
            let mut p = Vec::new();
            let hits = pool.install(|| query::Searcher { live: &live }.search(&q));
            return hits
                .iter()
                .map(|h| {
                    live.base.path(h.idx as usize, &mut p);
                    format!("{:5} {}", h.score, String::from_utf8_lossy(&p))
                })
                .collect();
        };
        let r = if q.scope.as_ref().is_none_or(|s| fsearch::content::in_scope(s, home.as_bytes())) {
            pool.install(|| c.search(g, &q))
        } else {
            let paths = pool.install(|| fsearch::content::scan_paths(&live, q.clone_for_scan()));
            fsearch::content::verify(g, &paths, q.limit)
        };
        stats.set((r.candidates, r.read, r.complete));
        let first = |f: &fsearch::FileMatches| f.lines.first().map_or(String::new(), |(n, t)| format!("{n}: {}", t.trim()));
        r.files.iter().map(|f| format!("{}:{}", String::from_utf8_lossy(&f.path), first(f))).collect()
    };
    // Between runs, another name search: a repeat would be answered from
    // the names cache, which only a repeat of the last query hits.
    let other = query::Query::parse("zzzzzz", &home).unwrap();
    let mut times = Vec::new();
    let mut out = Vec::new();
    let runs = std::env::var("FSEARCH_RUNS").ok().and_then(|r| r.parse().ok()).unwrap_or(20);
    for _ in 0..runs {
        pool.install(|| query::Searcher { live: &live }.search(&other));
        let t = Instant::now();
        out = run();
        times.push(t.elapsed());
    }
    let shown = if runs == 1 { out.len() } else { 10 };
    for l in out.iter().take(shown) {
        println!("{l}");
    }
    let (candidates, read, complete) = stats.get();
    eprintln!("{} results; content: {candidates} candidates, {read} read, complete {complete}", out.len());
    let first = times[0];
    times.sort();
    eprintln!("first {first:.2?}  median {:.2?}  min {:.2?}", times[times.len() / 2], times[0]);
}

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    PathBuf::from(home()).join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> bool {
    std::process::Command::new("launchctl").args(args).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

#[cfg(target_os = "macos")]
fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(target_os = "macos")]
fn install(login: bool) {
    // A plain reinstall keeps an existing login agent.
    let login = login || plist_path().exists();
    let bin = PathBuf::from(home()).join(".local/bin/fsearch");
    // Stop the old daemon so the next one runs the new binary. Unload the
    // login agent first, or KeepAlive would restart it straight away.
    let target = format!("{}/{LABEL}", domain());
    launchctl(&["bootout", &target]);
    server::stop(&data_dir()).unwrap_or_else(|e| die(&e));
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    // Replace, never overwrite in place: a rewritten signed binary at the same
    // path can be SIGKILLed by the code-signing cache. Copy then rename, so
    // reinstalling from the installed copy works too.
    let tmp = bin.with_extension("new");
    std::fs::copy(std::env::current_exe().unwrap(), &tmp).and_then(|_| std::fs::rename(&tmp, &bin)).unwrap_or_else(|e| die(&format!("copy: {e}")));
    if !login {
        println!("installed {}; the daemon starts on first use", bin.display());
        return;
    }
    let log = data_dir().join("daemon.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        bin.display(),
        log.display(),
        log.display()
    );
    std::fs::write(plist_path(), plist).unwrap();
    // bootout returns before the old job is fully gone; bootstrap fails
    // until it is.
    let bootstrap = || launchctl(&["bootstrap", &domain(), plist_path().to_str().unwrap()]);
    let retry = || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        false
    };
    if !(0..50).any(|_| bootstrap() || retry()) {
        die("launchctl bootstrap failed");
    }
    println!("installed {} (LaunchAgent {LABEL})", bin.display());
}

#[cfg(target_os = "macos")]
fn uninstall() {
    launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    let _ = std::fs::remove_file(plist_path());
    println!("removed LaunchAgent {LABEL}; index kept in {}", data_dir().display());
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn reg(args: &[&str]) -> bool {
    let null = std::process::Stdio::null;
    std::process::Command::new("reg").args(args).stdout(null()).stderr(null()).status().is_ok_and(|s| s.success())
}

#[cfg(windows)]
fn install(login: bool) {
    // A plain reinstall keeps an existing login entry.
    let login = login || reg(&["query", RUN_KEY, "/v", "fsearch"]);
    let profile = PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default());
    let bin = profile.join(r".local\bin\fsearch.exe");
    // The next daemon must run the new binary.
    server::stop(&data_dir()).unwrap_or_else(|e| die(&e));
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    // A running exe can't be overwritten but can be renamed: move the old one
    // aside (reinstalling from the installed copy), then put the new one in.
    let tmp = bin.with_extension("new");
    std::fs::copy(std::env::current_exe().unwrap(), &tmp).unwrap_or_else(|e| die(&format!("copy: {e}")));
    if bin.exists() {
        std::fs::rename(&bin, bin.with_extension("old")).unwrap_or_else(|e| die(&format!("move old binary aside: {e}")));
    }
    std::fs::rename(&tmp, &bin).unwrap_or_else(|e| die(&format!("install: {e}")));
    if !login {
        println!("installed {}; the daemon starts on first use", bin.display());
        return;
    }
    // A login entry can't carry environment variables: the daemon it starts
    // reads the user's.
    for k in ["FSEARCH_ROOTS", "FSEARCH_HOME"] {
        if let Ok(v) = std::env::var(k) {
            if !reg(&["add", r"HKCU\Environment", "/v", k, "/t", "REG_SZ", "/d", &v, "/f"]) {
                die(&format!("could not save {k}"));
            }
            println!("saved {k}={v} as a user variable");
        }
    }
    let cmd = format!("\"{}\" serve --log", bin.display());
    if !reg(&["add", RUN_KEY, "/v", "fsearch", "/t", "REG_SZ", "/d", &cmd, "/f"]) {
        die("could not add the login entry");
    }
    println!("installed {} (login entry {RUN_KEY}\\fsearch)", bin.display());
}

#[cfg(windows)]
fn uninstall() {
    reg(&["delete", RUN_KEY, "/v", "fsearch", "/f"]);
    println!("removed the login entry; binary and index kept in {}", data_dir().display());
}

fn die(msg: &str) -> ! {
    eprintln!("fsearch: {msg}");
    std::process::exit(1)
}
