//! Thin wrappers over libc.
//!
//! This is the only module with `unsafe` code. Each wrapper mirrors one C
//! call in upstream cronie, with the same flags. Failures return errno, and
//! the errno of every failed call is also recorded as "C's errno"
//! ([`cerrno`]), which messages built from `errno` read.

use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::bytes::{c_str_prefix, to_cstring};

static C_ERRNO: AtomicI32 = AtomicI32::new(0);

fn last_os_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn fail() -> i32 {
    let e = last_os_errno();
    C_ERRNO.store(e, Ordering::Relaxed);
    e
}

/// The errno C cronie would see now.
pub fn cerrno() -> i32 {
    C_ERRNO.load(Ordering::Relaxed)
}

pub fn set_cerrno(e: i32) {
    C_ERRNO.store(e, Ordering::Relaxed);
}

/// `strerror(e)`.
pub fn strerror(e: i32) -> Vec<u8> {
    // SAFETY: strerror returns a valid C string; copied out at once in this
    // single-threaded program.
    unsafe { CStr::from_ptr(libc::strerror(e)) }.to_bytes().to_vec()
}

pub type Fd = c_int;

pub fn write(fd: Fd, data: &[u8]) -> isize {
    // SAFETY: data is valid for its length.
    let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
    if n < 0 {
        fail();
    }
    n
}

/// Write everything, retrying on EINTR, giving up on other errors.
pub fn write_all(fd: Fd, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        let n = write(fd, data);
        if n < 0 {
            if cerrno() == libc::EINTR {
                continue;
            }
            return false;
        }
        data = &data[n as usize..];
    }
    true
}

pub fn read(fd: Fd, buf: &mut [u8]) -> isize {
    // SAFETY: buf is valid for its length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        fail();
    }
    n
}

pub fn open(path: &[u8], flags: c_int, mode: libc::mode_t) -> Result<Fd, i32> {
    let p = to_cstring(path);
    // SAFETY: p is a valid C string.
    let fd = unsafe { libc::open(p.as_ptr(), flags, mode as libc::c_uint) };
    if fd < 0 { Err(fail()) } else { Ok(fd) }
}

pub fn close(fd: Fd) -> i32 {
    // SAFETY: no pointers involved.
    let r = unsafe { libc::close(fd) };
    if r < 0 {
        fail();
    }
    r
}

/// The parts of `struct stat` cronie looks at.
#[derive(Debug, Clone, Copy)]
pub struct Stat {
    pub mode: libc::mode_t,
    pub uid: libc::uid_t,
    pub nlink: u64,
    pub mtime: i64,
    pub size: i64,
    pub blksize: i64,
}

fn conv(st: &libc::stat) -> Stat {
    Stat {
        mode: st.st_mode,
        uid: st.st_uid,
        nlink: st.st_nlink,
        mtime: st.st_mtime,
        size: st.st_size,
        blksize: st.st_blksize,
    }
}

pub fn stat(path: &[u8]) -> Result<Stat, i32> {
    let p = to_cstring(path);
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: p is a valid C string; st has room for a struct stat.
    if unsafe { libc::stat(p.as_ptr(), st.as_mut_ptr()) } < 0 {
        return Err(fail());
    }
    // SAFETY: initialized by the successful call.
    Ok(conv(unsafe { st.assume_init_ref() }))
}

pub fn fstat(fd: Fd) -> Result<Stat, i32> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: st has room for a struct stat.
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } < 0 {
        return Err(fail());
    }
    // SAFETY: initialized by the successful call.
    Ok(conv(unsafe { st.assume_init_ref() }))
}

pub fn is_dir(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFDIR
}

pub fn is_reg(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFREG
}

pub fn is_chr(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFCHR
}

pub fn isatty(fd: Fd) -> bool {
    // SAFETY: no pointers involved.
    unsafe { libc::isatty(fd) == 1 }
}

/// Read all of an fd (stdio-style: a read error ends the data quietly).
pub fn read_to_end(fd: Fd) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = read(fd, &mut buf);
        if n < 0 && cerrno() == libc::EINTR {
            continue;
        }
        if n <= 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
}

/// `opendir()` + `readdir()` until NULL: the names, in readdir order
/// (`.` and `..` included, as readdir returns them).
pub fn read_dir(path: &[u8]) -> Result<Vec<Vec<u8>>, i32> {
    let p = to_cstring(path);
    // SAFETY: p is a valid C string.
    let dir = unsafe { libc::opendir(p.as_ptr()) };
    if dir.is_null() {
        return Err(fail());
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: dir is a valid DIR*; the entry is copied out before the
        // next call.
        let ent = unsafe { libc::readdir(dir) };
        if ent.is_null() {
            break;
        }
        // SAFETY: d_name is a NUL-terminated array inside the entry.
        let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        names.push(name.to_bytes().to_vec());
    }
    // SAFETY: dir came from opendir and is closed once.
    unsafe { libc::closedir(dir) };
    Ok(names)
}

pub fn getpid() -> i32 {
    // SAFETY: always safe.
    unsafe { libc::getpid() }
}

pub fn gethostname() -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    // SAFETY: buf is valid for its length.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        fail();
        return None;
    }
    Some(c_str_prefix(&buf).to_vec())
}

/// `getenv(name)`.
pub fn getenv(name: &[u8]) -> Option<Vec<u8>> {
    let n = to_cstring(name);
    // SAFETY: n is a valid C string; the result is copied out at once.
    let p = unsafe { libc::getenv(n.as_ptr()) };
    if p.is_null() {
        None
    } else {
        // SAFETY: getenv returns a valid C string.
        Some(unsafe { CStr::from_ptr(p) }.to_bytes().to_vec())
    }
}

unsafe extern "C" {
    #[link_name = "environ"]
    static C_ENVIRON: *const *const c_char;
    fn asctime(tm: *const libc::tm) -> *mut c_char;
}

/// The process environment, in `environ` order.
pub fn environ() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    // SAFETY: environ is the process's NULL-terminated environment array;
    // nothing modifies it concurrently.
    unsafe {
        let mut p = C_ENVIRON;
        while !p.is_null() && !(*p).is_null() {
            out.push(CStr::from_ptr(*p).to_bytes().to_vec());
            p = p.add(1);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Users

/// A copied `struct passwd` (`pw_dup()`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Passwd {
    pub name: Vec<u8>,
    pub passwd: Vec<u8>,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    pub gecos: Vec<u8>,
    pub dir: Vec<u8>,
    pub shell: Vec<u8>,
}

/// `getpwnam(name)`.
pub fn getpwnam(name: &[u8]) -> Option<Passwd> {
    let n = to_cstring(name);
    // SAFETY: n is a valid C string; the static result is copied out at
    // once in this single-threaded program.
    let pw = unsafe { libc::getpwnam(n.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    let s = |p: *const c_char| {
        if p.is_null() {
            Vec::new()
        } else {
            // SAFETY: passwd fields are valid C strings when non-NULL.
            unsafe { CStr::from_ptr(p) }.to_bytes().to_vec()
        }
    };
    // SAFETY: pw is a valid struct passwd.
    let pw = unsafe { &*pw };
    Some(Passwd {
        name: s(pw.pw_name),
        passwd: s(pw.pw_passwd),
        uid: pw.pw_uid,
        gid: pw.pw_gid,
        gecos: s(pw.pw_gecos),
        dir: s(pw.pw_dir),
        shell: s(pw.pw_shell),
    })
}

/// `endpwent()`.
pub fn endpwent() {
    // SAFETY: always safe.
    unsafe { libc::endpwent() };
}

// ---------------------------------------------------------------------------
// Time

pub type Tm = libc::tm;

pub fn time_now() -> i64 {
    // SAFETY: NULL is a valid argument.
    unsafe { libc::time(std::ptr::null_mut()) }
}

/// `localtime_r()`.
pub fn localtime_r(t: i64) -> Tm {
    // SAFETY: struct tm is plain old data.
    let mut tm: Tm = unsafe { std::mem::zeroed() };
    // SAFETY: t and tm are valid.
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}

/// `localtime()` (re-reads TZ, unlike `localtime_r`).
pub fn localtime(t: i64) -> Option<Tm> {
    // SAFETY: the static result is copied out at once.
    let p = unsafe { libc::localtime(&t) };
    // SAFETY: non-NULL results point to a valid struct tm.
    if p.is_null() { None } else { Some(unsafe { *p }) }
}

/// `mktime()` (normalizes `tm` in place).
pub fn mktime(tm: &mut Tm) -> i64 {
    // SAFETY: tm is a valid struct tm.
    unsafe { libc::mktime(tm) }
}

/// `asctime(tm)`: "Wed Nov 11 17:00:00 2026\n".
pub fn asctime_text(tm: &Tm) -> Vec<u8> {
    // SAFETY: tm is valid; the static result is copied out at once.
    let p = unsafe { asctime(tm) };
    if p.is_null() {
        return Vec::new();
    }
    // SAFETY: asctime returns a valid C string.
    unsafe { CStr::from_ptr(p) }.to_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// syslog

/// `openlog(NULL, LOG_PID, facility)` (NULL: glibc uses the program's
/// short name, as cronnext's unset ProgramName does).
pub fn openlog(ident: Option<&CStr>, option: c_int, facility: c_int) {
    let p = ident.map_or(std::ptr::null(), CStr::as_ptr);
    // SAFETY: p is NULL or a C string the caller keeps alive (leaked).
    unsafe { libc::openlog(p, option, facility) };
}

/// `syslog(priority, "%s", msg)`.
pub fn syslog(priority: c_int, msg: &[u8]) {
    let m = to_cstring(msg);
    // SAFETY: the format and argument are valid C strings.
    unsafe { libc::syslog(priority, c"%s".as_ptr(), m.as_ptr()) };
}

/// Leak a C string for the life of the process (openlog keeps the pointer).
pub fn leak_cstring(s: &[u8]) -> &'static CStr {
    Box::leak(to_cstring(s).into_boxed_c_str())
}

/// Keep `CString` in scope for callers that build their own.
pub type OwnedCString = CString;

// ---------------------------------------------------------------------------
// Credentials

pub fn getuid() -> libc::uid_t {
    // SAFETY: always safe.
    unsafe { libc::getuid() }
}
pub fn geteuid() -> libc::uid_t {
    // SAFETY: always safe.
    unsafe { libc::geteuid() }
}
pub fn getgid() -> libc::gid_t {
    // SAFETY: always safe.
    unsafe { libc::getgid() }
}
pub fn getegid() -> libc::gid_t {
    // SAFETY: always safe.
    unsafe { libc::getegid() }
}

fn ok_or_errno(r: c_int) -> Result<(), i32> {
    if r < 0 { Err(fail()) } else { Ok(()) }
}

pub fn setregid(r: libc::gid_t, e: libc::gid_t) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::setregid(r, e) })
}
pub fn setreuid(r: libc::uid_t, e: libc::uid_t) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::setreuid(r, e) })
}
pub fn setgid(g: libc::gid_t) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::setgid(g) })
}
pub fn setuid(u: libc::uid_t) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::setuid(u) })
}

/// `getpwuid(uid)`.
pub fn getpwuid(uid: libc::uid_t) -> Option<Passwd> {
    // SAFETY: the static result is converted at once (see getpwnam).
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return None;
    }
    // SAFETY: pw is a valid struct passwd.
    Some(passwd_from(unsafe { &*pw }))
}

fn passwd_from(pw: &libc::passwd) -> Passwd {
    let s = |p: *const c_char| {
        if p.is_null() {
            Vec::new()
        } else {
            // SAFETY: passwd fields are valid C strings when non-NULL.
            unsafe { CStr::from_ptr(p) }.to_bytes().to_vec()
        }
    };
    Passwd {
        name: s(pw.pw_name),
        passwd: s(pw.pw_passwd),
        uid: pw.pw_uid,
        gid: pw.pw_gid,
        gecos: s(pw.pw_gecos),
        dir: s(pw.pw_dir),
        shell: s(pw.pw_shell),
    }
}

// ---------------------------------------------------------------------------
// More file operations

pub fn lstat(path: &[u8]) -> Result<Stat, i32> {
    let p = to_cstring(path);
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: p is a valid C string; st has room for a struct stat.
    if unsafe { libc::lstat(p.as_ptr(), st.as_mut_ptr()) } < 0 {
        return Err(fail());
    }
    // SAFETY: initialized by the successful call.
    Ok(conv(unsafe { st.assume_init_ref() }))
}

fn path_call(path: &[u8], f: impl FnOnce(*const c_char) -> c_int) -> Result<(), i32> {
    let p = to_cstring(path);
    ok_or_errno(f(p.as_ptr()))
}

pub fn mkdir(path: &[u8], mode: libc::mode_t) -> Result<(), i32> {
    // SAFETY: valid C string.
    path_call(path, |p| unsafe { libc::mkdir(p, mode) })
}
pub fn unlink(path: &[u8]) -> Result<(), i32> {
    // SAFETY: valid C string.
    path_call(path, |p| unsafe { libc::unlink(p) })
}
pub fn rename(from: &[u8], to: &[u8]) -> Result<(), i32> {
    let a = to_cstring(from);
    let b = to_cstring(to);
    // SAFETY: valid C strings.
    ok_or_errno(unsafe { libc::rename(a.as_ptr(), b.as_ptr()) })
}

/// `utime(path, NULL)` (now) or `utime(path, {t, t})`.
pub fn utime(path: &[u8], t: Option<i64>) -> Result<(), i32> {
    let buf = t.map(|t| libc::utimbuf { actime: t, modtime: t });
    // SAFETY: valid C string; the utimbuf is NULL or valid.
    path_call(path, |p| unsafe { libc::utime(p, buf.as_ref().map_or(std::ptr::null(), |b| b as *const _)) })
}

pub fn fchown(fd: Fd, uid: libc::uid_t, gid: libc::gid_t) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::fchown(fd, uid, gid) })
}
pub fn fchmod(fd: Fd, mode: libc::mode_t) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::fchmod(fd, mode) })
}
pub fn ftruncate(fd: Fd, len: i64) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::ftruncate(fd, len) })
}
pub fn fsync(fd: Fd) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::fsync(fd) })
}
pub fn lseek_set(fd: Fd, off: i64) -> Result<i64, i32> {
    // SAFETY: no pointers involved.
    let r = unsafe { libc::lseek(fd, off, libc::SEEK_SET) };
    if r < 0 { Err(fail()) } else { Ok(r) }
}

pub fn umask(m: libc::mode_t) -> libc::mode_t {
    // SAFETY: no pointers involved.
    unsafe { libc::umask(m) }
}

/// `mkstemp(template)`: replaces the trailing XXXXXX in place.
pub fn mkstemp(template: &mut Vec<u8>) -> Result<Fd, i32> {
    let mut buf = template.clone();
    buf.push(0);
    // SAFETY: buf is a writable NUL-terminated template.
    let fd = unsafe { libc::mkstemp(buf.as_mut_ptr().cast()) };
    buf.pop();
    *template = buf;
    if fd < 0 { Err(fail()) } else { Ok(fd) }
}

// ---------------------------------------------------------------------------
// Processes and signals

pub fn fork() -> Result<i32, i32> {
    // SAFETY: the program is single-threaded.
    let pid = unsafe { libc::fork() };
    if pid < 0 { Err(fail()) } else { Ok(pid) }
}

/// `execl(path, arg0, args..., NULL)`; returns only on failure.
pub fn execl(path: &[u8], args: &[&[u8]]) -> i32 {
    let p = to_cstring(path);
    let cs: Vec<CString> = args.iter().map(|a| to_cstring(a)).collect();
    let mut argv: Vec<*const c_char> = cs.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());
    // SAFETY: p and argv are valid NULL-terminated arrays of C strings.
    unsafe { libc::execv(p.as_ptr(), argv.as_ptr()) };
    fail()
}

/// `waitpid(pid, &status, 0)`: (returned pid, status).
pub fn waitpid(pid: i32) -> (i32, c_int) {
    let mut st: c_int = 0;
    // SAFETY: st is a valid out-pointer.
    let r = unsafe { libc::waitpid(pid, &mut st, 0) };
    if r < 0 {
        fail();
    }
    (r, st)
}

pub type Handler = extern "C" fn(c_int);

pub fn signal_handler(sig: c_int, handler: Handler) {
    // SAFETY: handler is a valid extern "C" fn(c_int).
    unsafe { libc::signal(sig, handler as *const () as libc::sighandler_t) };
}
pub fn signal_ignore(sig: c_int) {
    // SAFETY: SIG_IGN is a valid disposition.
    unsafe { libc::signal(sig, libc::SIG_IGN) };
}
pub fn signal_default(sig: c_int) {
    // SAFETY: SIG_DFL is a valid disposition.
    unsafe { libc::signal(sig, libc::SIG_DFL) };
}

/// `_exit(code)`.
pub fn exit_now(code: i32) -> ! {
    // SAFETY: _exit never returns.
    unsafe { libc::_exit(code) }
}

/// `unlink()` of a prepared path, for signal handlers.
pub fn unlink_c(path: &CStr) {
    // SAFETY: path is a valid C string.
    unsafe { libc::unlink(path.as_ptr()) };
}

/// `setlocale(LC_ALL, "")`.
pub fn setlocale_all() {
    // SAFETY: "" is a valid C string; single-threaded.
    unsafe { libc::setlocale(libc::LC_ALL, c"".as_ptr()) };
}

// ---------------------------------------------------------------------------
// PAM (libpam), as security.c uses it

#[repr(C)]
struct PamHandle {
    _p: [u8; 0],
}

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamConv {
    conv: extern "C" fn(c_int, *mut *const PamMessage, *mut *mut libc::c_void, *mut libc::c_void) -> c_int,
    appdata: *mut libc::c_void,
}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(service: *const c_char, user: *const c_char, conv: *const PamConv, pamh: *mut *mut PamHandle)
    -> c_int;
    fn pam_end(pamh: *mut PamHandle, status: c_int) -> c_int;
    fn pam_set_item(pamh: *mut PamHandle, item_type: c_int, item: *const libc::c_void) -> c_int;
    fn pam_acct_mgmt(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_setcred(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_open_session(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_close_session(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_strerror(pamh: *mut PamHandle, errnum: c_int) -> *const c_char;
}

pub const PAM_SUCCESS: c_int = 0;
pub const PAM_SILENT: c_int = 0x8000;
pub const PAM_ESTABLISH_CRED: c_int = 0x0002;
pub const PAM_DELETE_CRED: c_int = 0x0004;
const PAM_TTY: c_int = 3;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

/// Where the conversation function sends PAM's messages (`log_it("CRON",
/// pid, "pam_message", msg)` in C).
static PAM_LOG: std::sync::OnceLock<fn(&[u8])> = std::sync::OnceLock::new();

pub fn set_pam_log(f: fn(&[u8])) {
    let _ = PAM_LOG.set(f);
}

/// `cron_conv()`: log error and info messages; answer nothing.
extern "C" fn cron_conv(
    num_msg: c_int,
    msgm: *mut *const PamMessage,
    _resp: *mut *mut libc::c_void,
    _appdata: *mut libc::c_void,
) -> c_int {
    for i in 0..num_msg.max(0) as usize {
        // SAFETY: PAM passes num_msg valid message pointers.
        let m = unsafe { &**msgm.add(i) };
        if (m.msg_style == PAM_ERROR_MSG || m.msg_style == PAM_TEXT_INFO) && !m.msg.is_null() {
            // SAFETY: msg is a valid C string.
            let text = unsafe { CStr::from_ptr(m.msg) }.to_bytes();
            if let Some(f) = PAM_LOG.get() {
                f(text);
            }
        }
    }
    0
}

static CONV: PamConv = PamConv { conv: cron_conv, appdata: std::ptr::null_mut() };
// SAFETY: CONV is immutable and only read by libpam.
unsafe impl Sync for PamConv {}

/// An open PAM transaction (`pamh`).
#[derive(Debug)]
pub struct Pam {
    h: *mut PamHandle,
}

impl Pam {
    /// `pam_start(service, user, &conv, &pamh)`.
    pub fn start(service: &[u8], user: &[u8]) -> Result<Pam, (Option<Pam>, c_int)> {
        let s = to_cstring(service);
        let u = to_cstring(user);
        let mut h: *mut PamHandle = std::ptr::null_mut();
        // SAFETY: valid strings and conversation; h receives the handle.
        let r = unsafe { pam_start(s.as_ptr(), u.as_ptr(), &CONV, &mut h) };
        let pam = if h.is_null() { None } else { Some(Pam { h }) };
        if r != PAM_SUCCESS {
            return Err((pam, r));
        }
        Ok(pam.expect("pam_start succeeded"))
    }

    pub fn set_tty(&self, tty: &CStr) -> c_int {
        // SAFETY: valid handle; PAM copies the string.
        unsafe { pam_set_item(self.h, PAM_TTY, tty.as_ptr().cast()) }
    }

    pub fn acct_mgmt(&self, flags: c_int) -> c_int {
        // SAFETY: valid handle.
        unsafe { pam_acct_mgmt(self.h, flags) }
    }

    pub fn setcred(&self, flags: c_int) -> c_int {
        // SAFETY: valid handle.
        unsafe { pam_setcred(self.h, flags) }
    }

    pub fn open_session(&self, flags: c_int) -> c_int {
        // SAFETY: valid handle.
        unsafe { pam_open_session(self.h, flags) }
    }

    pub fn close_session(&self, flags: c_int) -> c_int {
        // SAFETY: valid handle.
        unsafe { pam_close_session(self.h, flags) }
    }

    /// `pam_strerror(pamh, err)`.
    pub fn strerror(&self, err: c_int) -> Vec<u8> {
        // SAFETY: valid handle; the result is a static C string.
        let p = unsafe { pam_strerror(self.h, err) };
        if p.is_null() {
            return Vec::new();
        }
        // SAFETY: as above.
        unsafe { CStr::from_ptr(p) }.to_bytes().to_vec()
    }

    /// `pam_end(pamh, status)`.
    pub fn end(self, status: c_int) {
        // SAFETY: valid handle, ended once (self is consumed).
        unsafe { pam_end(self.h, status) };
    }
}
