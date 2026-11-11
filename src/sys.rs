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
    // glibc sets errno itself (0 for "no such user"); callers log it.
    set_cerrno(last_os_errno());
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

/// `closelog()`.
pub fn closelog() {
    // SAFETY: always safe.
    unsafe { libc::closelog() };
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
    set_cerrno(last_os_errno());
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
    fn pam_getenvlist(pamh: *mut PamHandle) -> *mut *mut c_char;
}

pub const PAM_SUCCESS: c_int = 0;
pub const PAM_SILENT: c_int = 0x8000;
pub const PAM_ESTABLISH_CRED: c_int = 0x0002;
pub const PAM_DELETE_CRED: c_int = 0x0004;
pub const PAM_REINITIALIZE_CRED: c_int = 0x0008;
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

    /// `pam_strerror(NULL, err)` (libpam doesn't use the handle).
    pub fn strerror_static(err: c_int) -> Vec<u8> {
        // SAFETY: pam_strerror accepts a NULL handle; static result.
        let p = unsafe { pam_strerror(std::ptr::null_mut(), err) };
        if p.is_null() {
            return Vec::new();
        }
        // SAFETY: as above.
        unsafe { CStr::from_ptr(p) }.to_bytes().to_vec()
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

    /// `pam_getenvlist(pamh)`: the PAM environment (`None` if NULL).  The
    /// list and its strings are freed, as the caller would.
    pub fn getenvlist(&self) -> Option<Vec<Vec<u8>>> {
        // SAFETY: valid handle; the result is a malloc'ed NULL-terminated
        // array of malloc'ed strings, owned by the caller.
        unsafe {
            let list = pam_getenvlist(self.h);
            if list.is_null() {
                return None;
            }
            let mut out = Vec::new();
            let mut i = 0;
            while !(*list.add(i)).is_null() {
                let s = *list.add(i);
                out.push(CStr::from_ptr(s).to_bytes().to_vec());
                libc::free(s.cast());
                i += 1;
            }
            libc::free(list.cast());
            Some(out)
        }
    }

    /// `pam_end(pamh, status)`.
    pub fn end(self, status: c_int) {
        // SAFETY: valid handle, ended once (self is consumed).
        unsafe { pam_end(self.h, status) };
    }
}

// ---------------------------------------------------------------------------
// crond: time, environment, files

/// `sleep(secs)`, through libc (the test clock shim intercepts it).
pub fn sleep(secs: u32) -> u32 {
    // SAFETY: no pointers involved.
    unsafe { libc::sleep(secs) }
}

/// `gettimeofday()`'s microseconds (0 if it fails, as cron.c sets).
pub fn gettimeofday_usec() -> i64 {
    let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
    // SAFETY: tv is a valid out-pointer; tz may be NULL.
    if unsafe { libc::gettimeofday(&mut tv, std::ptr::null_mut()) } != 0 {
        return 0;
    }
    tv.tv_usec
}

/// `gmtime()`.
pub fn gmtime(t: i64) -> Option<Tm> {
    // SAFETY: the static result is copied out at once.
    let p = unsafe { libc::gmtime(&t) };
    // SAFETY: non-NULL results point to a valid struct tm.
    if p.is_null() { None } else { Some(unsafe { *p }) }
}

/// `setenv(name, value, 1)`.
pub fn setenv(name: &[u8], value: &[u8]) -> Result<(), i32> {
    let n = to_cstring(name);
    let v = to_cstring(value);
    // SAFETY: valid C strings; single-threaded.
    ok_or_errno(unsafe { libc::setenv(n.as_ptr(), v.as_ptr(), 1) })
}

/// `unsetenv(name)`.
pub fn unsetenv(name: &[u8]) {
    let n = to_cstring(name);
    // SAFETY: valid C string; single-threaded.
    unsafe { libc::unsetenv(n.as_ptr()) };
}

/// `nl_langinfo(CODESET)`.
pub fn codeset() -> Option<Vec<u8>> {
    // SAFETY: nl_langinfo returns a static string (or NULL).
    let p = unsafe { libc::nl_langinfo(libc::CODESET) };
    if p.is_null() {
        return None;
    }
    // SAFETY: a valid C string.
    Some(unsafe { CStr::from_ptr(p) }.to_bytes().to_vec())
}

/// `access(path, mode) == 0`.
pub fn access(path: &[u8], mode: c_int) -> bool {
    let p = to_cstring(path);
    // SAFETY: valid C string.
    let r = unsafe { libc::access(p.as_ptr(), mode) };
    if r != 0 {
        fail();
    }
    r == 0
}

/// `creat(path, mode)`.
pub fn creat(path: &[u8], mode: libc::mode_t) -> Result<Fd, i32> {
    let p = to_cstring(path);
    // SAFETY: valid C string.
    let fd = unsafe { libc::creat(p.as_ptr(), mode) };
    if fd < 0 { Err(fail()) } else { Ok(fd) }
}

/// `flock(fd, LOCK_EX|LOCK_NB)`.
pub fn flock_try(fd: Fd) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) })
}

/// `fcntl(fd, F_SETFD, 1)`.
pub fn set_cloexec(fd: Fd) {
    // SAFETY: no pointers involved.
    unsafe { libc::fcntl(fd, libc::F_SETFD, 1) };
}

pub fn chdir(path: Option<&[u8]>) -> Result<(), i32> {
    match path {
        // chdir(NULL) fails with EFAULT, as in C.
        None => {
            set_cerrno(libc::EFAULT);
            Err(libc::EFAULT)
        }
        Some(p) => {
            let c = to_cstring(p);
            // SAFETY: valid C string.
            ok_or_errno(unsafe { libc::chdir(c.as_ptr()) })
        }
    }
}

pub fn initgroups(name: &[u8], gid: libc::gid_t) -> Result<(), i32> {
    let n = to_cstring(name);
    // SAFETY: valid C string.
    ok_or_errno(unsafe { libc::initgroups(n.as_ptr(), gid) })
}

pub fn pipe() -> Result<[Fd; 2], i32> {
    let mut fds = [0 as c_int; 2];
    // SAFETY: fds has room for two ints.
    ok_or_errno(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    Ok(fds)
}

pub fn dup2(from: Fd, to: Fd) {
    // SAFETY: no pointers involved.
    unsafe { libc::dup2(from, to) };
}

pub fn setsid() {
    // SAFETY: no pointers involved.
    unsafe { libc::setsid() };
}

pub fn getdtablesize() -> c_int {
    // SAFETY: no pointers involved.
    unsafe { libc::getdtablesize() }
}

pub fn kill(pid: i32, sig: c_int) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::kill(pid, sig) })
}

/// `wait(&status)`: (pid, status); pid -1 on error (errno recorded).
pub fn wait() -> (i32, c_int) {
    let mut st: c_int = 0;
    // SAFETY: st is a valid out-pointer.
    let r = unsafe { libc::wait(&mut st) };
    if r < 0 {
        fail();
    }
    (r, st)
}

/// `waitpid(-1, &status, WNOHANG)`.
pub fn waitpid_nohang() -> (i32, c_int) {
    let mut st: c_int = 0;
    // SAFETY: st is a valid out-pointer.
    let r = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG) };
    if r < 0 {
        fail();
    }
    (r, st)
}

/// `sigaction(sig, {handler, mask = {}, flags})`.
pub fn sigaction(sig: c_int, handler: Option<Handler>, flags: c_int) {
    // SAFETY: sigaction is plain old data; all-zero is a valid start.
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = match handler {
        Some(h) => h as *const () as libc::sighandler_t,
        None => libc::SIG_DFL,
    };
    sa.sa_flags = flags;
    // SAFETY: sa is valid; the old action isn't wanted.
    unsafe {
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

/// `sigaction(sig, {SIG_IGN})`.
pub fn sigaction_ignore(sig: c_int) {
    // SAFETY: as above, with SIG_IGN.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_IGN;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

/// Block SIGINT, SIGQUIT and SIGHUP; returns the old mask for `sigrestore`.
pub fn block_int_quit_hup() -> libc::sigset_t {
    // SAFETY: sigset_t is plain old data; the calls fill it in.
    unsafe {
        let mut nset: libc::sigset_t = std::mem::zeroed();
        let mut oset: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut nset);
        libc::sigaddset(&mut nset, libc::SIGINT);
        libc::sigaddset(&mut nset, libc::SIGQUIT);
        libc::sigaddset(&mut nset, libc::SIGHUP);
        libc::sigprocmask(libc::SIG_BLOCK, &nset, &mut oset);
        oset
    }
}

pub fn sigrestore(oset: &libc::sigset_t) {
    // SAFETY: oset came from block_int_quit_hup.
    unsafe { libc::sigprocmask(libc::SIG_SETMASK, oset, std::ptr::null_mut()) };
}

/// `execle(path, arg0, args..., NULL, envp)`; returns only on failure.
pub fn execle(path: &[u8], args: &[&[u8]], envp: &[Vec<u8>]) -> i32 {
    let p = to_cstring(path);
    let a: Vec<CString> = args.iter().map(|x| to_cstring(x)).collect();
    let e: Vec<CString> = envp.iter().map(|x| to_cstring(x)).collect();
    let mut av: Vec<*const c_char> = a.iter().map(|c| c.as_ptr()).collect();
    av.push(std::ptr::null());
    let mut ev: Vec<*const c_char> = e.iter().map(|c| c.as_ptr()).collect();
    ev.push(std::ptr::null());
    // SAFETY: valid NULL-terminated arrays of C strings.
    unsafe { libc::execve(p.as_ptr(), av.as_ptr(), ev.as_ptr()) };
    fail()
}

/// `execvpe(file, argv, envp)` (searches PATH); returns only on failure.
pub fn execvpe(file: &[u8], args: &[Vec<u8>], envp: &[Vec<u8>]) -> i32 {
    let f = to_cstring(file);
    let a: Vec<CString> = args.iter().map(|x| to_cstring(x)).collect();
    let e: Vec<CString> = envp.iter().map(|x| to_cstring(x)).collect();
    let mut av: Vec<*const c_char> = a.iter().map(|c| c.as_ptr()).collect();
    av.push(std::ptr::null());
    let mut ev: Vec<*const c_char> = e.iter().map(|c| c.as_ptr()).collect();
    ev.push(std::ptr::null());
    // SAFETY: valid NULL-terminated arrays of C strings.
    unsafe { libc::execvpe(f.as_ptr(), av.as_ptr(), ev.as_ptr()) };
    fail()
}

/// The fds open in this process (from /proc/self/fd), ascending.
pub fn open_fds() -> Option<Vec<c_int>> {
    let names = read_dir(b"/proc/self/fd").ok()?;
    let mut fds: Vec<c_int> =
        names.iter().filter(|n| n.as_slice() != b"." && n.as_slice() != b"..").map(|n| crate::num::atoi(n)).collect();
    fds.sort_unstable();
    Some(fds)
}

// ---------------------------------------------------------------------------
// inotify

pub fn inotify_init() -> Result<Fd, i32> {
    // SAFETY: no pointers involved.
    let fd = unsafe { libc::inotify_init() };
    if fd < 0 { Err(fail()) } else { Ok(fd) }
}

pub fn inotify_add_watch(fd: Fd, path: &[u8], mask: u32) -> Result<c_int, i32> {
    let p = to_cstring(path);
    // SAFETY: valid C string.
    let w = unsafe { libc::inotify_add_watch(fd, p.as_ptr(), mask) };
    if w < 0 { Err(fail()) } else { Ok(w) }
}

pub fn inotify_rm_watch(fd: Fd, wd: c_int) {
    // SAFETY: no pointers involved.
    unsafe { libc::inotify_rm_watch(fd, wd) };
}

/// `select()` with a zero timeout: is fd readable?  Err on failure.
pub fn readable_now(fd: Fd) -> Result<bool, i32> {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: p is a valid pollfd.
    let r = unsafe { libc::poll(&mut p, 1, 0) };
    if r < 0 {
        return Err(fail());
    }
    Ok(r > 0 && p.revents & libc::POLLIN != 0)
}

// ---------------------------------------------------------------------------
// argv (crond's children upshift their program name for ps)

static ARGV0: std::sync::atomic::AtomicPtr<c_char> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

extern "C" fn capture_argv(_argc: c_int, argv: *mut *mut c_char, _envp: *mut *mut c_char) {
    if !argv.is_null() {
        // SAFETY: glibc passes a valid argv to .init_array functions.
        ARGV0.store(unsafe { *argv }, std::sync::atomic::Ordering::Relaxed);
    }
}

#[used]
#[unsafe(link_section = ".init_array")]
static CAPTURE_ARGV: extern "C" fn(c_int, *mut *mut c_char, *mut *mut c_char) = capture_argv;

/// Upshift argv[0] after its last '/' in place, as cron.c does to
/// `ProgramName` with CAPITALIZE_FOR_PS.  Returns the new name.
pub fn capitalize_program_name() -> Vec<u8> {
    let p = ARGV0.load(std::sync::atomic::Ordering::Relaxed);
    if p.is_null() {
        return Vec::new();
    }
    // SAFETY: p is argv[0], a NUL-terminated string in this process's argv
    // area; it is only modified in place, without changing its length.
    unsafe {
        let len = libc::strlen(p);
        let bytes = std::slice::from_raw_parts_mut(p.cast::<u8>(), len);
        let start = bytes.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
        for b in &mut bytes[start..] {
            *b = b.to_ascii_uppercase();
        }
        bytes[start..].to_vec()
    }
}

// ---------------------------------------------------------------------------
// anacron: POSIX regex, fnmatch, alarm/sigsuspend, fcntl locks

/// `match_rx()` failed (C's -1): a bad regex or an unmatched group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegexError;

/// `match_rx(rx, string, n_sub, ...)` from matchrx.c: extended regex match;
/// `Err` on a regex error or an unmatched subexpression, else the
/// substrings (C NUL-terminates them in place; they never touch).
pub fn match_rx(rx: &CStr, string: &[u8], n_sub: usize) -> Result<Option<Vec<Vec<u8>>>, RegexError> {
    let s = to_cstring(string);
    // SAFETY: regex_t is plain old data until regcomp initializes it.
    let mut crx: libc::regex_t = unsafe { std::mem::zeroed() };
    // SAFETY: valid pattern; crx is writable.
    if unsafe { libc::regcomp(&mut crx, rx.as_ptr(), libc::REG_EXTENDED) } != 0 {
        return Err(RegexError);
    }
    let mut subs = vec![libc::regmatch_t { rm_so: 0, rm_eo: 0 }; n_sub + 1];
    // SAFETY: crx is compiled; subs has n_sub + 1 entries.
    let r = unsafe { libc::regexec(&crx, s.as_ptr(), n_sub + 1, subs.as_mut_ptr(), 0) };
    if r != 0 && r != libc::REG_NOMATCH {
        // (C leaks crx here.)
        return Err(RegexError);
    }
    // SAFETY: crx was compiled above and is freed once.
    unsafe { libc::regfree(&mut crx) };
    if r == libc::REG_NOMATCH {
        return Ok(None);
    }
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    for m in &subs[1..] {
        if m.rm_so == -1 {
            return Err(RegexError);
        }
        out.push(bytes[m.rm_so as usize..m.rm_eo as usize].to_vec());
    }
    Ok(Some(out))
}

/// `fnmatch(pattern, string, 0)`: 0 on a match, FNM_NOMATCH, or an error.
pub fn fnmatch(pattern: &[u8], string: &[u8]) -> c_int {
    let p = to_cstring(pattern);
    let s = to_cstring(string);
    // SAFETY: valid C strings.
    unsafe { libc::fnmatch(p.as_ptr(), s.as_ptr(), 0) }
}

pub const FNM_NOMATCH: c_int = libc::FNM_NOMATCH;

/// `alarm(n)`, through libc (the test clock shim intercepts it).
pub fn alarm(n: u32) {
    // SAFETY: no pointers involved.
    unsafe { libc::alarm(n) };
}

/// The current signal mask (`sigprocmask(0, NULL, &ss)`).
pub fn sigmask_current() -> Result<libc::sigset_t, i32> {
    // SAFETY: sigset_t is plain old data; the call fills it.
    unsafe {
        let mut ss: libc::sigset_t = std::mem::zeroed();
        ok_or_errno(libc::sigprocmask(0, std::ptr::null(), &mut ss))?;
        Ok(ss)
    }
}

/// `sigprocmask(SIG_SETMASK, ss, NULL)`.
pub fn sigmask_set(ss: &libc::sigset_t) -> Result<(), i32> {
    // SAFETY: ss is a valid sigset_t.
    ok_or_errno(unsafe { libc::sigprocmask(libc::SIG_SETMASK, ss, std::ptr::null_mut()) })
}

/// A set of these signals.
pub fn sigset_of(sigs: &[c_int]) -> libc::sigset_t {
    // SAFETY: sigset_t is plain old data; sigemptyset/sigaddset fill it.
    unsafe {
        let mut ss: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut ss);
        for &s in sigs {
            libc::sigaddset(&mut ss, s);
        }
        ss
    }
}

/// `sigprocmask(SIG_BLOCK, ss, NULL)`.
pub fn sigmask_block(ss: &libc::sigset_t) -> Result<(), i32> {
    // SAFETY: ss is a valid sigset_t.
    ok_or_errno(unsafe { libc::sigprocmask(libc::SIG_BLOCK, ss, std::ptr::null_mut()) })
}

/// `sigsuspend(mask without sigs)`: wait for one of `sigs`.
pub fn sigsuspend_unblocking(sigs: &[c_int]) -> Result<(), i32> {
    let mut ss = sigmask_current()?;
    // SAFETY: ss is a valid sigset_t.
    unsafe {
        for &s in sigs {
            libc::sigdelset(&mut ss, s);
        }
        libc::sigsuspend(&ss);
    }
    Ok(())
}

/// `sigaction(sig, {handler, mask, flags})`.
pub fn sigaction_masked(sig: c_int, handler: Handler, mask: &libc::sigset_t, flags: c_int) -> Result<(), i32> {
    // SAFETY: sigaction is plain old data; handler is a valid fn.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as libc::sighandler_t;
        sa.sa_mask = *mask;
        sa.sa_flags = flags;
        ok_or_errno(libc::sigaction(sig, &sa, std::ptr::null_mut()))
    }
}

/// `fcntl(fd, F_SETLK, {F_WRLCK, whole file})`.
pub fn lock_whole_file(fd: Fd) -> Result<(), i32> {
    // SAFETY: flock is plain old data.
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = libc::F_WRLCK as i16;
    fl.l_whence = libc::SEEK_SET as i16;
    // SAFETY: fl is a valid struct flock.
    ok_or_errno(unsafe { libc::fcntl(fd, libc::F_SETLK, &fl) })
}

pub fn fchdir(fd: Fd) -> Result<(), i32> {
    // SAFETY: no pointers involved.
    ok_or_errno(unsafe { libc::fchdir(fd) })
}

/// `sscanf(s, "%4d%2d%2d", &y, &m, &d) == 3`.
pub fn scan_ymd(s: &[u8]) -> Option<(i32, i32, i32)> {
    let c = to_cstring(s);
    let (mut y, mut m, mut d) = (0 as c_int, 0 as c_int, 0 as c_int);
    // SAFETY: valid string and format; three int out-pointers.
    let n = unsafe { libc::sscanf(c.as_ptr(), c"%4d%2d%2d".as_ptr(), &mut y, &mut m, &mut d) };
    (n == 3).then_some((y, m, d))
}

/// `gethostname(buf, 512)`.
pub fn gethostname_512() -> Option<Vec<u8>> {
    let mut buf = [0u8; 512];
    // SAFETY: buf is valid for its length.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    Some(c_str_prefix(&buf).to_vec())
}

/// Raise SIGFPE, as an integer division by zero does in C.
pub fn raise_sigfpe() -> ! {
    // SAFETY: restoring the default action and raising have no
    // memory-safety implications; the process ends here.
    unsafe {
        libc::signal(libc::SIGFPE, libc::SIG_DFL);
        libc::raise(libc::SIGFPE);
        libc::abort()
    }
}

/// `waitpid(pid, &status, WNOHANG)`: (returned pid, status).
pub fn waitpid_pid_nohang(pid: i32) -> (i32, c_int) {
    let mut st: c_int = 0;
    // SAFETY: st is a valid out-pointer.
    let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
    if r < 0 {
        fail();
    }
    (r, st)
}

/// `setsid()`, reporting failure.
pub fn setsid_checked() -> Result<(), i32> {
    // SAFETY: no pointers involved.
    if unsafe { libc::setsid() } == -1 { Err(fail()) } else { Ok(()) }
}
