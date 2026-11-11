//! Differential test harness: run a C cronie program and its cronie-rs
//! port on the same input and compare everything observable.  Adapted from
//! metalog-rs.
//!
//! Each run gets a fresh directory `T` under `target/difftest/<case>/<impl>`
//! and runs as "root" in a private user and mount namespace.  Test
//! directories are bound over the paths cronie reads:
//!
//! | test path     | bound over               |
//! |---------------|--------------------------|
//! | `T/spool`     | `/var/spool/cron` (the crontabs are in `crontabs/`) |
//! | `T/cron.d`    | `/etc/cron.d`            |
//! | `T/crontab`   | `/etc/crontab` (created empty unless given) |
//! | `T/devlog`    | `/dev/log`, a socket the harness reads |
//!
//! `@T@` in arguments and files stands for that directory.  The clock is the
//! LD_PRELOAD shim in tests/shim/, so "now" is fixed unless a case says
//! otherwise.
//!
//! The harness compares the exit status, stdout, stderr, the syslog
//! messages, and every file under `T`, with `T` and pids normalized.
//!
//! The C programs come from `$CRONIE_REFERENCE` (a build tree), by default
//! `../cronie` (tools/build-reference.sh builds it there).

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// 2025-11-11 14:00:00 UTC, a Tuesday.
pub const DEFAULT_TIME: (i64, i64) = (1_762_869_600, 0);

#[derive(Debug, Clone)]
pub struct Case {
    pub name: String,
    pub program: &'static str,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Files to create first: (path under T, contents, mode).
    pub files: Vec<(String, Vec<u8>, u32)>,
    pub stdin: Vec<u8>,
    pub time: (i64, i64),
}

impl Case {
    pub fn new(name: &str, program: &'static str, args: &[&str]) -> Case {
        Case {
            name: name.to_string(),
            program,
            args: args.iter().map(|s| s.to_string()).collect(),
            env: vec![("TZ".into(), "UTC".into()), ("PATH".into(), "/usr/bin:/bin".into())],
            files: Vec::new(),
            stdin: Vec::new(),
            time: DEFAULT_TIME,
        }
    }

    pub fn env(mut self, k: &str, v: &str) -> Case {
        self.env.retain(|(kk, _)| kk != k);
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn file(mut self, path: &str, contents: &[u8], mode: u32) -> Case {
        self.files.push((path.into(), contents.to_vec(), mode));
        self
    }

    /// A user crontab in the spool, as `crontab` installs them (mode 0600).
    pub fn spool(self, user: &str, contents: &str) -> Case {
        self.file(&format!("spool/crontabs/{user}"), contents.as_bytes(), 0o600)
    }

    pub fn time(mut self, sec: i64) -> Case {
        self.time = (sec, 0);
        self
    }

    pub fn stdin(mut self, data: &[u8]) -> Case {
        self.stdin = data.to_vec();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: String,
    pub stdout: String,
    pub stderr: String,
    pub syslog: String,
    pub files: BTreeMap<String, String>,
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn reference_binary(program: &str) -> PathBuf {
    let dir = std::env::var_os("CRONIE_REFERENCE").map_or_else(|| manifest_dir().join("../cronie"), PathBuf::from);
    match program {
        "anacron" => dir.join("anacron/anacron"),
        p => dir.join("src").join(p),
    }
}

pub fn rust_binary(program: &str) -> PathBuf {
    PathBuf::from(match program {
        "cronnext" => env!("CARGO_BIN_EXE_cronnext"),
        p => panic!("no Rust binary for {p} yet"),
    })
}

fn shim() -> &'static Path {
    static SHIM: OnceLock<PathBuf> = OnceLock::new();
    SHIM.get_or_init(|| {
        let dir = manifest_dir().join("target/difftest");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("faketime.so");
        let tmp = dir.join(format!("faketime.so.{}", std::process::id()));
        let st = Command::new("cc")
            .args(["-shared", "-fPIC", "-O2", "-o"])
            .arg(&tmp)
            .arg(manifest_dir().join("tests/shim/faketime.c"))
            .arg("-ldl")
            .status()
            .expect("run cc");
        assert!(st.success(), "building the faketime shim failed");
        std::fs::rename(&tmp, &out).unwrap();
        out
    })
}

fn subst(s: &str, t: &Path) -> String {
    s.replace("@T@", t.to_str().unwrap())
}

/// Enter new user and mount namespaces (our uid/gid mapped to root) and
/// bind the test paths in.  Runs between fork and exec, so only raw
/// syscalls on buffers prepared beforehand.
fn namespace_pre_exec(cmd: &mut Command, binds: Vec<(CString, CString)>) {
    // SAFETY: getuid/getgid are always safe.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let files: Vec<(CString, Vec<u8>)> = vec![
        (CString::new("/proc/self/setgroups").unwrap(), b"deny".to_vec()),
        (CString::new("/proc/self/uid_map").unwrap(), format!("0 {uid} 1").into_bytes()),
        (CString::new("/proc/self/gid_map").unwrap(), format!("0 {gid} 1").into_bytes()),
    ];
    let root = CString::new("/").unwrap();
    // SAFETY: the closure only makes async-signal-safe calls on data
    // prepared before the fork.
    unsafe {
        cmd.pre_exec(move || {
            let err = std::io::Error::last_os_error;
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return Err(err());
            }
            for (path, data) in &files {
                let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
                if fd < 0 {
                    return Err(err());
                }
                let n = libc::write(fd, data.as_ptr().cast(), data.len());
                libc::close(fd);
                if n < 0 {
                    return Err(err());
                }
            }
            if libc::mount(
                std::ptr::null(),
                root.as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ) != 0
            {
                return Err(err());
            }
            for (src, dst) in &binds {
                if libc::mount(src.as_ptr(), dst.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) != 0 {
                    return Err(err());
                }
            }
            Ok(())
        });
    }
}

/// Replace `T` with `@T@` and the program's pid with `@PID@`; syslog
/// timestamps are fixed by the fake clock.
pub fn normalize(s: &str, t: &Path, pid: u32) -> String {
    let s = s.replace(t.to_str().unwrap(), "@T@");
    let pid_s = pid.to_string();
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            let j = i + b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            let after_ok = j == b.len() || !b[j].is_ascii_alphanumeric();
            out.push_str(if after_ok && s[i..j] == pid_s { "@PID@" } else { &s[i..j] });
            i = j;
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn snapshot(t: &Path, pid: u32) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let skip = [".faketime", "stdout", "stderr", "stdin", "devlog"];
    fn walk(base: &Path, dir: &Path, skip: &[&str], pid: u32, out: &mut BTreeMap<String, String>) {
        let mut ents: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        ents.sort_by_key(|e| e.file_name());
        for e in ents {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
            if dir == base && skip.contains(&rel.as_str()) {
                continue;
            }
            use std::os::unix::fs::{FileTypeExt, PermissionsExt};
            let md = std::fs::symlink_metadata(&p).unwrap();
            let mode = md.permissions().mode() & 0o7777;
            let ft = md.file_type();
            let desc = if ft.is_dir() {
                walk(base, &p, skip, pid, out);
                format!("dir {mode:o}")
            } else if ft.is_socket() {
                format!("socket {mode:o}")
            } else if ft.is_symlink() {
                format!("symlink -> {}", std::fs::read_link(&p).unwrap().display())
            } else {
                match std::fs::read(&p) {
                    Ok(data) => format!("file {mode:o}\n{}", normalize(&String::from_utf8_lossy(&data), base, pid)),
                    Err(_) => format!("file {mode:o} (unreadable)"),
                }
            };
            out.insert(normalize(&rel, base, pid), desc);
        }
    }
    walk(t, t, &skip, pid, &mut out);
    out
}

pub fn run(case: &Case, bin: &Path, label: &str) -> Outcome {
    let t = manifest_dir().join("target/difftest").join(&case.name).join(label);
    let _ = std::fs::remove_dir_all(&t);
    for d in ["spool/crontabs", "cron.d"] {
        std::fs::create_dir_all(t.join(d)).unwrap();
    }
    std::fs::write(t.join("crontab"), b"").unwrap();
    for (path, data, mode) in &case.files {
        let p = t.join(subst(path, &t));
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, subst(&String::from_utf8_lossy(data), &t)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(*mode)).unwrap();
    }
    std::fs::write(t.join(".faketime"), format!("{} {}\n", case.time.0, case.time.1)).unwrap();
    std::fs::write(t.join("stdin"), &case.stdin).unwrap();

    let syslog = UnixDatagram::bind(t.join("devlog")).unwrap();
    syslog.set_nonblocking(true).unwrap();
    let c = |p: &Path| CString::new(p.to_str().unwrap()).unwrap();
    let mut binds = vec![
        (c(&t.join("spool")), CString::new("/var/spool/cron").unwrap()),
        (c(&t.join("cron.d")), CString::new("/etc/cron.d").unwrap()),
        (c(&t.join("crontab")), CString::new("/etc/crontab").unwrap()),
    ];
    if Path::new("/dev/log").exists() {
        binds.push((c(&t.join("devlog")), CString::new("/dev/log").unwrap()));
    }

    let mut cmd = Command::new(bin);
    cmd.arg0(case.program)
        .args(case.args.iter().map(|a| subst(a, &t)))
        .current_dir(&t)
        .env_clear()
        .env("LD_PRELOAD", shim())
        .env("TEST_FAKETIME", t.join(".faketime"))
        .stdin(std::fs::File::open(t.join("stdin")).unwrap())
        .stdout(std::fs::File::create(t.join("stdout")).unwrap())
        .stderr(std::fs::File::create(t.join("stderr")).unwrap());
    for (k, v) in &case.env {
        cmd.env(k, subst(v, &t));
    }
    namespace_pre_exec(&mut cmd, binds);
    let mut child = cmd.spawn().expect("spawn");
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{}: {label}: did not exit", case.name);
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let status = match (status.code(), status.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (None, Some(s)) => format!("signal {s}"),
        _ => "unknown".into(),
    };
    let mut log = String::new();
    let mut buf = [0u8; 65536];
    while let Ok(n) = syslog.recv(&mut buf) {
        // "<pri>Mmm dd hh:mm:ss ident[pid]: msg": glibc's syslog() reads the
        // clock internally (not through the shim), so drop the timestamp.
        let msg = String::from_utf8_lossy(&buf[..n]).to_string();
        let msg = match (msg.find('>'), msg.len() > 20) {
            (Some(gt), true) if msg.is_char_boundary(gt + 16) => format!("{}{}", &msg[..=gt], &msg[gt + 16..]),
            _ => msg,
        };
        log += &normalize(&msg, &t, pid);
        log.push('\n');
    }
    let rd = |n: &str| normalize(&String::from_utf8_lossy(&std::fs::read(t.join(n)).unwrap()), &t, pid);
    Outcome { status, stdout: rd("stdout"), stderr: rd("stderr"), syslog: log, files: snapshot(&t, pid) }
}

/// Run a case with the C program and its port; assert identical outcomes.
pub fn check(case: Case) -> Outcome {
    let reference = reference_binary(case.program);
    assert!(
        reference.exists(),
        "reference {} not found at {} (run tools/build-reference.sh or set CRONIE_REFERENCE)",
        case.program,
        reference.display()
    );
    let c = run(&case, &reference, "c");
    let r = run(&case, &rust_binary(case.program), "rust");
    if c != r {
        let mut msg = format!("case {}: outcomes differ\n", case.name);
        for (what, a, b) in [
            ("status", &c.status, &r.status),
            ("stdout", &c.stdout, &r.stdout),
            ("stderr", &c.stderr, &r.stderr),
            ("syslog", &c.syslog, &r.syslog),
        ] {
            if a != b {
                msg += &format!("{what}:\n--- C\n{a}\n--- Rust\n{b}\n");
            }
        }
        for k in c.files.keys().chain(r.files.keys()).collect::<std::collections::BTreeSet<_>>() {
            let (a, b) = (c.files.get(k), r.files.get(k));
            if a != b {
                msg += &format!(
                    "file {k}:\n--- C\n{}\n--- Rust\n{}\n",
                    a.map_or("(missing)", |s| s),
                    b.map_or("(missing)", |s| s)
                );
            }
        }
        panic!("{msg}");
    }
    if std::env::var_os("CRONIE_DIFF_SHOW").is_some() {
        eprintln!("== {} ==\n{c:#?}", case.name);
    }
    c
}
