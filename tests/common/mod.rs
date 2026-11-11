//! Differential test harness: run a C cronie program and its cronie-rs
//! port in the same sandbox and compare everything observable.
//!
//! # The sandbox
//!
//! Each run gets a fresh directory `T` under `target/difftest/<case>/<impl>`
//! and runs in new user and mount namespaces.  Ids are mapped with the
//! setuid `newuidmap`/`newgidmap` helpers and the invoking user's
//! subordinate ranges (`/etc/subuid`, `/etc/subgid`): namespace id 0 is the
//! invoking user, and ids 1-65536 are the first subordinate ids.  So the
//! sandbox has a real root and real other users, and the program can run as
//! any of them, or as a setgid binary.
//!
//! The sandbox's `/etc` is `T/etc`: a farm of symlinks to the real `/etc`
//! (bound aside at `T/.realetc`), except for what the harness or the case
//! puts there itself:
//!
//! | file | default |
//! |---|---|
//! | `passwd`, `group` | root, alice (1001), bob (1002), cron (16), nobody; groups crontab (460), users |
//! | `pam.d/crond` | `pam_permit` throughout (cases can make it `pam_deny`) |
//! | `crontab` | empty |
//! | `cron.d/` | empty |
//! | `cron.allow`, `cron.deny` | absent |
//!
//! Also bound: `T/spool` over `/var/spool/cron` (crontabs in `crontabs/`),
//! `T/tmp` over `/tmp`, and `T/devlog` (a socket the harness reads) over
//! `/dev/log`.  Home directories are `T/home/<user>`.  `@T@` in arguments,
//! environment and files stands for `T`.  The clock is the LD_PRELOAD shim
//! in tests/shim/.
//!
//! # What is compared
//!
//! The exit status, stdout, stderr, the syslog messages, and every file under
//! `T` (type, mode, owner, contents), with `T`, pids and random temporary
//! names normalized.  The snapshot and the cleanup run as root inside the
//! sandbox, so files the program created as other users are covered too.
//!
//! The C programs come from `$CRONIE_REFERENCE` (a build tree), by default
//! `../cronie` (tools/build-reference.sh builds it there).

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::{CString, c_char, c_int};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// 2025-11-11 14:00:00 UTC, a Tuesday.
pub const DEFAULT_TIME: (i64, i64) = (1_762_869_600, 0);

pub const PASSWD: &str = "\
root:x:0:0:root:@T@/home/root:/bin/sh
alice:x:1001:1001:Alice:@T@/home/alice:/bin/sh
bob:x:1002:1002:Bob:@T@/home/bob:/bin/sh
cron:x:16:16:cron daemon:/var/spool/cron:/sbin/nologin
nobody:x:65534:65534:nobody:/var/empty:/sbin/nologin
";
pub const GROUP: &str = "\
root:x:0:
cron:x:16:
users:x:100:alice,bob
crontab:x:460:
alice:x:1001:
bob:x:1002:
nobody:x:65534:
";
pub const PAM_PERMIT: &str =
    "auth required pam_permit.so\naccount required pam_permit.so\nsession required pam_permit.so\n";
pub const PAM_DENY: &str =
    "auth required pam_permit.so\naccount required pam_deny.so\nsession required pam_permit.so\n";

/// Who the program runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum As {
    Root,
    /// A test user: (uid, gid, supplementary groups).
    User(u32, u32, &'static [u32]),
}

pub const ALICE: As = As::User(1001, 1001, &[1001, 100]);
pub const BOB: As = As::User(1002, 1002, &[1002, 100]);

#[derive(Debug, Clone)]
pub struct Case {
    pub name: String,
    pub program: &'static str,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Files to create first: (path under T, contents, mode, owner uid).
    /// Modes above 0777 (setuid, sticky) and non-root owners are applied
    /// inside the sandbox.
    pub files: Vec<(String, Vec<u8>, u32, u32)>,
    pub stdin: Vec<u8>,
    pub time: (i64, i64),
    pub user: As,
    /// Run a copy of the program that is setgid to this group (crontab is
    /// installed setgid crontab, mode 2751).
    pub setgid: Option<u32>,
    /// Daemon mode (crond): run as PID 1 of a new PID namespace with
    /// virtual time (the shim's sleep() advances the clock), until this
    /// time, when the shim sends SIGTERM.
    pub stop: Option<i64>,
    /// PID 1 of a new PID namespace with virtual time (set by `run_for`
    /// and `virtual_pid1`).
    pub virtual_time: bool,
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
            user: As::Root,
            setgid: None,
            stop: None,
            virtual_time: false,
        }
    }

    /// Virtual time as PID 1, without a stop time (anacron exits by itself).
    pub fn virtual_pid1(mut self) -> Case {
        self.virtual_time = true;
        self
    }

    /// Run as a daemon in virtual time for `minutes` minutes (see `stop`).
    pub fn run_for(mut self, minutes: i64) -> Case {
        self.stop = Some(self.time.0 + minutes * 60);
        self.virtual_time = true;
        self
    }

    pub fn env(mut self, k: &str, v: &str) -> Case {
        self.env.retain(|(kk, _)| kk != k);
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn file(mut self, path: &str, contents: &[u8], mode: u32) -> Case {
        self.files.push((path.into(), contents.to_vec(), mode, 0));
        self
    }

    /// A file owned by `uid` (and the group of the same number).
    pub fn file_owned(mut self, path: &str, contents: &[u8], mode: u32, uid: u32) -> Case {
        self.files.push((path.into(), contents.to_vec(), mode, uid));
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

    pub fn run_as(mut self, user: As) -> Case {
        self.user = user;
        self
    }

    /// Run as `user`, through a copy of the program that is setgid
    /// crontab, the way Gentoo installs crontab.
    pub fn setgid_crontab(mut self, user: As) -> Case {
        self.user = user;
        self.setgid = Some(460);
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
        "crond" => env!("CARGO_BIN_EXE_crond"),
        "anacron" => env!("CARGO_BIN_EXE_anacron"),
        "crontab" => env!("CARGO_BIN_EXE_crontab"),
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

fn cstr(s: impl AsRef<[u8]>) -> CString {
    CString::new(s.as_ref().to_vec()).unwrap()
}

fn path_c(p: &Path) -> CString {
    cstr(p.to_str().unwrap())
}

/// The invoking user's first subordinate id, from /etc/subuid or /etc/subgid.
fn subordinate(file: &str, name: &str, id: u32) -> u32 {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    for line in text.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() == 3 && (f[0] == name || f[0] == id.to_string()) && f[2].parse::<u32>().unwrap_or(0) >= 65536 {
            return f[1].parse().unwrap();
        }
    }
    panic!("no subordinate range of 65536 ids for {name} in {file}: the sandbox needs one");
}

/// What the child does between fork and exec, prepared beforehand.
struct Spawn {
    /// Also a new PID namespace: the program runs as its PID 1 (with a
    /// fresh /proc), under a waiter that exits with its status.
    pid_ns: bool,
    binds: Vec<(CString, CString)>,
    /// (path, owner, group, mode) fixups made as root in the sandbox.
    chowns: Vec<(CString, u32, u32, u32)>,
    user: As,
    dir: CString,
    stdio: [c_int; 3],
    exe: CString,
    argv: Vec<CString>,
    envp: Vec<CString>,
}

fn pipe() -> (c_int, c_int) {
    let mut fds = [0 as c_int; 2];
    // SAFETY: fds has room for two ints.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    (fds[0], fds[1])
}

/// fork(); the child enters the sandbox and execs.  Returns the pid once
/// the child has reached exec.
fn spawn_in_sandbox(s: Spawn) -> u32 {
    // SAFETY: getuid/getgid have no preconditions.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let name = std::env::var("USER").unwrap_or_default();
    let subuid = subordinate("/etc/subuid", &name, uid);
    let subgid = subordinate("/etc/subgid", &name, gid);

    let mut argv: Vec<*const c_char> = s.argv.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<*const c_char> = s.envp.iter().map(|a| a.as_ptr()).collect();
    envp.push(std::ptr::null());
    let gs: Vec<libc::gid_t> = match s.user {
        As::User(_, _, groups) => groups.to_vec(),
        As::Root => Vec::new(),
    };
    let (up_r, up_w) = pipe();
    let (go_r, go_w) = pipe();
    let root = cstr("/");
    let proc_ = cstr("proc");
    let proc_dir = cstr("/proc");

    // SAFETY: between fork and exec the child only makes async-signal-safe
    // system calls on memory prepared above, then execs or _exits.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        // SAFETY: as above.
        unsafe {
            libc::close(go_w);
            libc::close(up_r);
            let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | if s.pid_ns { libc::CLONE_NEWPID } else { 0 };
            if libc::unshare(flags) != 0 {
                libc::_exit(120);
            }
            libc::write(up_w, b"u".as_ptr().cast(), 1);
            let mut b = [0u8; 1];
            if libc::read(go_r, b.as_mut_ptr().cast(), 1) != 1 {
                libc::_exit(121);
            }
            let none = std::ptr::null::<c_char>();
            if libc::mount(none, root.as_ptr(), none, libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) != 0 {
                libc::_exit(122);
            }
            for (src, dst) in &s.binds {
                if libc::mount(src.as_ptr(), dst.as_ptr(), none, libc::MS_BIND | libc::MS_REC, std::ptr::null()) != 0 {
                    libc::_exit(123);
                }
            }
            if s.pid_ns {
                // The next child is PID 1; this process waits for it.
                let inner = libc::fork();
                if inner < 0 {
                    libc::_exit(119);
                }
                if inner > 0 {
                    libc::close(up_w);
                    let mut st: c_int = 0;
                    while libc::waitpid(inner, &mut st, 0) < 0 {}
                    if libc::WIFEXITED(st) {
                        libc::_exit(libc::WEXITSTATUS(st));
                    }
                    libc::_exit(128 + libc::WTERMSIG(st));
                }
                // Die with the waiter (the harness may kill it on a timeout).
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
                let none = std::ptr::null::<c_char>();
                let fl = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
                if libc::mount(proc_.as_ptr(), proc_dir.as_ptr(), proc_.as_ptr(), fl, std::ptr::null()) != 0 {
                    let _ = none;
                    libc::_exit(118);
                }
            }
            for (path, owner, group, mode) in &s.chowns {
                if libc::chown(path.as_ptr(), *owner, *group) != 0 || libc::chmod(path.as_ptr(), *mode) != 0 {
                    libc::_exit(124);
                }
            }
            if let As::User(u, g, _) = s.user
                && (libc::setgroups(gs.len(), gs.as_ptr()) != 0 || libc::setgid(g) != 0 || libc::setuid(u) != 0)
            {
                libc::_exit(125);
            }
            for (i, fd) in s.stdio.iter().enumerate() {
                libc::dup2(*fd, i as c_int);
            }
            if libc::chdir(s.dir.as_ptr()) != 0 {
                libc::_exit(126);
            }
            // up_w is close-on-exec: EOF tells the parent we reached exec.
            // ETXTBSY: another test thread's fork may briefly hold a write
            // fd to a freshly copied binary; wait it out.
            for _ in 0..200 {
                libc::execve(s.exe.as_ptr(), argv.as_ptr(), envp.as_ptr());
                if *libc::__errno_location() != libc::ETXTBSY {
                    break;
                }
                let ts = libc::timespec { tv_sec: 0, tv_nsec: 10_000_000 };
                libc::nanosleep(&ts, std::ptr::null_mut());
            }
            libc::_exit(127);
        }
    }
    // SAFETY: closing our copies of the child's ends.
    unsafe {
        libc::close(go_r);
        libc::close(up_w);
    }
    let mut b = [0u8; 1];
    // SAFETY: reading one byte into b.
    let n = unsafe { libc::read(up_r, b.as_mut_ptr().cast(), 1) };
    if n != 1 {
        panic!("sandbox child failed before unshare ({})", wait(pid as u32, Duration::from_secs(5)));
    }
    for (tool, host, sub) in [("newuidmap", uid, subuid), ("newgidmap", gid, subgid)] {
        let st = Command::new(tool)
            .arg(pid.to_string())
            .args(["0", &host.to_string(), "1", "1", &sub.to_string(), "65536"])
            .status()
            .unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert!(st.success(), "{tool} failed");
    }
    // SAFETY: writing one byte from a static buffer, then closing.
    unsafe {
        libc::write(go_w, b"g".as_ptr().cast(), 1);
        libc::close(go_w);
    }
    // The child execs (close-on-exec makes this EOF) or exits.
    // SAFETY: reading into b; then closing.
    unsafe {
        libc::read(up_r, b.as_mut_ptr().cast(), 1);
        libc::close(up_r);
    }
    pid as u32
}

/// waitpid with a timeout; the status as text.
fn wait(pid: u32, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let mut st: c_int = 0;
        // SAFETY: st is a valid out-pointer.
        let r = unsafe { libc::waitpid(pid as i32, &mut st, libc::WNOHANG) };
        if r == pid as i32 {
            return if libc::WIFEXITED(st) {
                format!("exit {}", libc::WEXITSTATUS(st))
            } else if libc::WIFSIGNALED(st) {
                format!("signal {}", libc::WTERMSIG(st))
            } else {
                "unknown".into()
            };
        }
        if Instant::now() > deadline {
            // SAFETY: killing and reaping our own child.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
                libc::waitpid(pid as i32, &mut st, 0);
            }
            return "timeout".into();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The binds every sandbox gets.
fn binds(t: &Path) -> Vec<(CString, CString)> {
    let mut b = vec![
        (cstr("/etc"), path_c(&t.join(".realetc"))),
        (path_c(&t.join("etc")), cstr("/etc")),
        (path_c(&t.join("spool")), cstr("/var/spool/cron")),
        (path_c(&t.join("tmp")), cstr("/tmp")),
        (path_c(&t.join("run")), cstr("/run")),
    ];
    if Path::new("/var/spool/anacron").exists() {
        b.push((path_c(&t.join("anacron")), cstr("/var/spool/anacron")));
    }
    if Path::new("/usr/sbin/sendmail").exists() {
        b.push((path_c(&t.join("sendmail")), cstr("/usr/sbin/sendmail")));
    }
    if Path::new("/dev/log").exists() {
        b.push((path_c(&t.join("devlog")), cstr("/dev/log")));
    }
    b
}

/// Run a shell script as root in a sandbox over `t`, for snapshots and
/// cleanup, which must see files owned by the sandbox's other users.
fn root_shell(t: &Path, script: &str) -> Vec<u8> {
    use std::os::fd::AsRawFd;
    let out = t.with_extension("rootsh.out");
    let outf = std::fs::File::create(&out).unwrap();
    let null = std::fs::File::open("/dev/null").unwrap();
    let pid = spawn_in_sandbox(Spawn {
        pid_ns: false,
        binds: binds(t),
        chowns: Vec::new(),
        user: As::Root,
        dir: cstr("/"),
        stdio: [null.as_raw_fd(), outf.as_raw_fd(), outf.as_raw_fd()],
        exe: cstr("/bin/sh"),
        argv: vec![cstr("sh"), cstr("-c"), cstr(script)],
        envp: vec![cstr("PATH=/usr/bin:/bin"), cstr("LC_ALL=C")],
    });
    let st = wait(pid, Duration::from_secs(30));
    let data = std::fs::read(&out).unwrap();
    let _ = std::fs::remove_file(&out);
    assert_eq!(st, "exit 0", "root shell failed: {script}\n{}", String::from_utf8_lossy(&data));
    data
}

/// Remove a previous run's directory, including files sandbox users made.
fn clean(t: &Path) {
    if !t.exists() || std::fs::remove_dir_all(t).is_ok() {
        return;
    }
    for d in ["etc", ".realetc", "spool", "tmp", "run", "anacron"] {
        std::fs::create_dir_all(t.join(d)).ok();
    }
    root_shell(t, &format!("chown -R 0:0 '{0}' 2>/dev/null; chmod -R u+rwX '{0}' 2>/dev/null; exit 0", t.display()));
    std::fs::remove_dir_all(t).unwrap_or_else(|e| panic!("can't clean {}: {e}", t.display()));
}

/// Replace `T` with `@T@`, the program's pid with `@PID@`, and random
/// mkstemp suffixes (`crontab.XXXXXX`) with `XXXXXX`.
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
    let mut res = String::new();
    let mut rest = out.as_str();
    while let Some(k) = rest.find("/crontab.") {
        res.push_str(&rest[..k + 9]);
        rest = &rest[k + 9..];
        let n = rest.bytes().take_while(u8::is_ascii_alphanumeric).count();
        if n == 6 {
            res.push_str("XXXXXX");
            rest = &rest[6..];
        }
    }
    res.push_str(rest);
    // "#tmp.HOST.XXXXXXXXXX": the last 10 characters are random.
    let mut out = String::new();
    let mut rest = res.as_str();
    while let Some(k) = rest.find("#tmp.") {
        let after = &rest[k + 5..];
        match after.find('.') {
            Some(dot)
                if after.len() >= dot + 11 && after[dot + 1..dot + 11].bytes().all(|b| b.is_ascii_alphanumeric()) =>
            {
                out.push_str(&rest[..k + 5 + dot + 1]);
                out.push_str("XXXXXXXXXX");
                rest = &after[dot + 11..];
            }
            _ => {
                out.push_str(&rest[..k + 5]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// In daemon runs the daemon is PID 1; its children's pids depend on
/// scheduling.  `CROND[17]` -> `CROND[N]` (the capitalized name is the
/// job processes').
fn normalize_job_pids(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(k) = rest.find("CROND[") {
        out.push_str(&rest[..k + 6]);
        rest = &rest[k + 6..];
        let n = rest.bytes().take_while(u8::is_ascii_digit).count();
        out.push('N');
        rest = &rest[n..];
    }
    out.push_str(rest);
    out
}

/// The tree under `t`, as root in the sandbox: type, mode, owner, contents.
fn snapshot(t: &Path, pid: u32) -> BTreeMap<String, String> {
    let script = format!(
        "cd '{}' || exit 1
         find . -mindepth 1 \\( -path ./.realetc -o -path ./etc -o -path ./bin -o -name devlog -o -name .faketime \
            -o -name stdin -o -name stdout -o -name stderr \\) -prune -o -print | LC_ALL=C sort | while read -r f; do
           printf '\\001%s\\002%s\\002' \"$f\" \"$(stat -c '%F %a %u:%g' \"$f\")\"
           if [ -f \"$f\" ] && [ ! -L \"$f\" ]; then cat \"$f\"; fi
         done
         for f in etc/cron.allow etc/cron.deny etc/crontab; do
           if [ -e \"$f\" ] && [ ! -L \"$f\" ]; then printf '\\001./%s\\002%s\\002' \"$f\" \"$(stat -c '%F %a %u:%g' \"$f\")\"; cat \"$f\"; fi
         done; exit 0",
        t.display()
    );
    let raw = root_shell(t, &script);
    let text = String::from_utf8_lossy(&raw).to_string();
    let mut out = BTreeMap::new();
    for rec in text.split('\u{1}').skip(1) {
        let mut parts = rec.splitn(3, '\u{2}');
        let (name, meta, data) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        out.insert(normalize(name, t, pid), normalize(&format!("{meta}\n{data}"), t, pid));
    }
    out
}

/// Build `T/etc`: symlinks to the real /etc (seen at `T/.realetc` inside
/// the sandbox), plus the harness's own files.
fn build_etc(t: &Path) {
    let etc = t.join("etc");
    std::fs::create_dir_all(&etc).unwrap();
    let own =
        ["passwd", "group", "pam.d", "crontab", "cron.d", "cron.allow", "cron.deny", "shadow", "gshadow", "anacrontab"];
    for ent in std::fs::read_dir("/etc").unwrap().flatten() {
        let name = ent.file_name();
        if own.contains(&name.to_str().unwrap_or("")) {
            continue;
        }
        std::os::unix::fs::symlink(t.join(".realetc").join(&name), etc.join(&name)).unwrap();
    }
    std::fs::write(etc.join("passwd"), subst(PASSWD, t)).unwrap();
    std::fs::write(etc.join("group"), GROUP).unwrap();
    std::fs::write(etc.join("crontab"), b"").unwrap();
    std::fs::create_dir_all(etc.join("cron.d")).unwrap();
    let pam = etc.join("pam.d");
    std::fs::create_dir_all(&pam).unwrap();
    for ent in std::fs::read_dir("/etc/pam.d").into_iter().flatten().flatten() {
        let name = ent.file_name();
        if name != "crond" {
            std::os::unix::fs::symlink(t.join(".realetc/pam.d").join(&name), pam.join(&name)).unwrap();
        }
    }
    std::fs::write(pam.join("crond"), PAM_PERMIT).unwrap();
}

/// The fake /usr/sbin/sendmail: each message (arguments, then the mail)
/// becomes a file in T/mail.
const SENDMAIL: &str = "#!/bin/sh\n{ printf 'sendmail'; printf ' %s' \"$@\"; printf '\\n'; cat; } > @T@/mail/mail.$$\n";

pub fn run(case: &Case, bin: &Path, label: &str) -> Outcome {
    use std::os::fd::AsRawFd;
    let t = manifest_dir().join("target/difftest").join(&case.name).join(label);
    clean(&t);
    for d in [
        "spool/crontabs",
        ".realetc",
        "tmp",
        "home/root",
        "home/alice",
        "home/bob",
        "bin",
        "run",
        "mail",
        "out",
        "anacron",
    ] {
        std::fs::create_dir_all(t.join(d)).unwrap();
    }
    build_etc(&t);
    let mut chowns = vec![
        (path_c(&t.join("home/alice")), 1001, 1001, 0o755),
        (path_c(&t.join("home/bob")), 1002, 1002, 0o755),
        (path_c(&t.join("tmp")), 0, 0, 0o1777),
        // Gentoo: docrondir -m 1730 -o root -g crontab
        (path_c(&t.join("spool/crontabs")), 0, 460, 0o1730),
        (path_c(&t.join("mail")), 0, 0, 0o1777),
        (path_c(&t.join("out")), 0, 0, 0o1777),
    ];
    std::fs::write(t.join("sendmail"), subst(SENDMAIL, &t)).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(t.join("sendmail"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for (path, data, mode, uid) in &case.files {
        let p = t.join(subst(path, &t));
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        if p.is_symlink() {
            std::fs::remove_file(&p).unwrap();
        }
        std::fs::write(&p, subst(&String::from_utf8_lossy(data), &t)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(*mode & 0o777)).unwrap();
        if *uid != 0 || *mode > 0o777 {
            chowns.push((path_c(&p), *uid, *uid, *mode));
        }
    }
    std::fs::write(t.join(".faketime"), format!("{} {}\n", case.time.0, case.time.1)).unwrap();
    std::fs::write(t.join("stdin"), &case.stdin).unwrap();

    // The program: run in place, or as a setgid copy under T/bin.
    let exe = match case.setgid {
        None => bin.to_path_buf(),
        Some(g) => {
            let copy = t.join("bin").join(case.program);
            std::fs::copy(bin, &copy).unwrap();
            chowns.push((path_c(&copy), 0, g, 0o2751));
            copy
        }
    };

    let syslog = UnixDatagram::bind(t.join("devlog")).unwrap();
    // A datagram socket queues only a few messages (net.unix.max_dgram_qlen)
    // before senders block, so read them as they come.
    syslog.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let stop_reader = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let sock = syslog.try_clone().unwrap();
        let stop = stop_reader.clone();
        std::thread::spawn(move || {
            let mut msgs: Vec<Vec<u8>> = Vec::new();
            let mut buf = [0u8; 65536];
            loop {
                match sock.recv(&mut buf) {
                    Ok(n) => msgs.push(buf[..n].to_vec()),
                    Err(_) if stop.load(std::sync::atomic::Ordering::SeqCst) => break,
                    Err(_) => {}
                }
            }
            msgs
        })
    };
    {
        // Like the real /dev/log: every user may log.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(t.join("devlog"), std::fs::Permissions::from_mode(0o666)).unwrap();
    }
    let stdin = std::fs::File::open(t.join("stdin")).unwrap();
    let stdout = std::fs::File::create(t.join("stdout")).unwrap();
    let stderr = std::fs::File::create(t.join("stderr")).unwrap();

    let mut envp = vec![
        cstr(format!("LD_PRELOAD={}", shim().display())),
        cstr(format!("TEST_FAKETIME={}", t.join(".faketime").display())),
    ];
    if case.virtual_time {
        envp.push(cstr("TEST_VIRTUAL_SLEEP=1"));
    }
    if let Some(stop) = case.stop {
        envp.push(cstr(format!("TEST_STOP={stop}")));
    }
    for (k, v) in &case.env {
        envp.push(cstr(format!("{k}={}", subst(v, &t))));
    }
    let mut argv = vec![cstr(case.program)];
    argv.extend(case.args.iter().map(|a| cstr(subst(a, &t))));

    let pid = spawn_in_sandbox(Spawn {
        pid_ns: case.virtual_time,
        binds: binds(&t),
        chowns,
        user: case.user,
        dir: path_c(&t),
        stdio: [stdin.as_raw_fd(), stdout.as_raw_fd(), stderr.as_raw_fd()],
        exe: path_c(&exe),
        argv,
        envp,
    });
    let status = wait(pid, Duration::from_secs(20));
    assert_ne!(status, "timeout", "{}: {label}: did not exit", case.name);
    // 120-127: the sandbox child failed before or at exec (see spawn_in_sandbox).
    if let Some(code) = status.strip_prefix("exit ").and_then(|c| c.parse::<i32>().ok()) {
        assert!(!(120..=127).contains(&code), "{}: {label}: sandbox setup failed ({status})", case.name);
    }

    stop_reader.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut log = String::new();
    for raw in reader.join().unwrap() {
        // "<pri>Mmm dd hh:mm:ss ident[pid]: msg": glibc's syslog() reads the
        // clock internally (not through the shim), so drop the timestamp.
        let msg = String::from_utf8_lossy(&raw).to_string();
        let msg = match (msg.find('>'), msg.len() > 20) {
            (Some(gt), true) if msg.is_char_boundary(gt + 16) => format!("{}{}", &msg[..=gt], &msg[gt + 16..]),
            _ => msg,
        };
        log += &normalize(&msg, &t, pid);
        log.push('\n');
    }
    let rd = |n: &str| normalize(&String::from_utf8_lossy(&std::fs::read(t.join(n)).unwrap()), &t, pid);
    let mut files = snapshot(&t, pid);
    if case.virtual_time {
        // Jobs of the same minute run concurrently: compare their mails
        // and log lines as sets, and job pids as placeholders.
        let mails: Vec<String> = files.keys().filter(|k| k.starts_with("./mail/")).cloned().collect();
        let mut bodies: Vec<String> = mails.iter().map(|k| files.remove(k).unwrap()).collect();
        bodies.sort();
        files.insert("<mail>".into(), bodies.join("\n=====\n"));
        let mut lines: Vec<String> = log.lines().map(normalize_job_pids).collect();
        lines.sort();
        log = lines.join("\n");
    }
    Outcome { status, stdout: rd("stdout"), stderr: rd("stderr"), syslog: log, files }
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
