//! Port of src/user.c and the database half of src/database.c: crontab
//! files and the set of installed crontabs.
//!
//! Upstream license: ISC (see LICENSE).

use crate::bcat;
use crate::entry::{Entry, load_entry};
use crate::env::{self, Env, LoadEnv};
use crate::globals::{self, DLOAD, DPARS};
use crate::reader::{self, Reader};
use crate::sys::{self, Passwd};

pub const MAX_USER_ENVS: usize = 1000;
pub const MAX_USER_ENTRIES: usize = 10000;
pub const SPOOL_DIR: &[u8] = b"/var/spool/cron/crontabs";
pub const SYS_CROND_DIR: &[u8] = b"/etc/cron.d";
pub const SYSCRONTAB: &[u8] = b"/etc/crontab";
pub const CRON_HOSTNAME: &[u8] = b".cron.hostname";
/// `NAME_MAX + 1`, the size of the tabname buffers.
const NAME_BUF: usize = 256;

/// `struct _user`: one crontab.
#[derive(Debug, Clone, Default)]
pub struct User {
    pub name: Vec<u8>,
    pub tabname: Vec<u8>,
    pub mtime: i64,
    /// In C's list order: each entry is pushed on the front, so this is
    /// the reverse of the file's order.
    pub crontab: Vec<Entry>,
    pub system: bool,
}

/// `cron_db`: the users, in list order.
#[derive(Debug, Clone, Default)]
pub struct CronDb {
    pub users: Vec<User>,
    pub mtime: i64,
}

/// `load_user(crontab_fd, pw, uname, fname, tabname)`.  Takes ownership of
/// the fd (closes it).
pub fn load_user(crontab_fd: sys::Fd, pw: Option<&Passwd>, uname: &[u8], fname: &[u8], tabname: &[u8]) -> Option<User> {
    let data = sys::read_to_end(crontab_fd);
    sys::close(crontab_fd);
    let mut file = Reader::new(data);
    globals::debug(DPARS, || b"load_user()\n".to_vec());

    let mut u = User { name: fname.to_vec(), tabname: tabname.to_vec(), system: pw.is_none(), ..User::default() };
    let mut envp: Env = Vec::new();
    env::env_set_from_environ(&mut envp);

    // QUIRK: `FileName` is a static in user.c, set only just before an
    // entry is parsed or a missing newline reported.  Earlier messages use
    // whatever it last held: the previous crontab's name, or NULL (printed
    // as "(null)") before any.
    let log_error = |msg: &[u8]| {
        let name = FILE_NAME.with(|f| f.borrow().clone()).unwrap_or_else(|| b"(null)".to_vec());
        globals::log_it(b"CRON", sys::getpid(), msg, &name, 0)
    };
    let set_file_name = |t: &[u8]| FILE_NAME.with(|f| *f.borrow_mut() = Some(t.to_vec()));

    let mut envs = 0usize;
    let mut entries = 0usize;
    // status: TRUE (1), FALSE (0), ERR (-1), as in C.
    let mut status: i32 = 1;
    while status >= 0 {
        if !reader::skip_comments(&mut file) && !u.system {
            log_error(b"too many garbage characters");
            status = 1;
            break;
        }
        match env::load_env(&mut file) {
            LoadEnv::Eof(partial) => {
                status = -1;
                if !partial.is_empty() {
                    set_file_name(tabname);
                    log_error(b"missing newline before EOF");
                }
            }
            LoadEnv::NotEnv => {
                status = 0;
                entries += 1;
                if !u.system && entries > MAX_USER_ENTRIES {
                    log_error(b"too many entries");
                    status = 1;
                    break;
                }
                set_file_name(tabname);
                let mut on_error = |msg: &[u8]| log_error(msg);
                if let Some(e) = load_entry(&mut file, &mut on_error, pw, &envp) {
                    u.crontab.insert(0, e);
                }
            }
            LoadEnv::Env(envstr) => {
                status = 1;
                envs += 1;
                if !u.system && envs > MAX_USER_ENVS {
                    log_error(b"too many environment variables");
                    break;
                }
                env::env_set(&mut envp, &envstr);
            }
        }
    }
    globals::debug(DPARS, || b"...load_user() done\n".to_vec());
    if status == 1 {
        globals::log_it(uname, sys::getpid(), b"FAILED", b"loading cron table", 0);
        return None;
    }
    Some(u)
}

/// `check_open()`: open a crontab and check its type, mode, owner and
/// link count.
fn check_open(tabname: &[u8], uname: &[u8], pw: Option<&Passwd>, mtime: &mut i64) -> Option<sys::Fd> {
    let pid = sys::getpid();
    let fd = match sys::open(tabname, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
        Ok(fd) => fd,
        Err(e) => {
            globals::log_it(uname, pid, b"CAN'T OPEN", tabname, e);
            return None;
        }
    };
    let st = match sys::fstat(fd) {
        Ok(st) => st,
        Err(e) => {
            globals::log_it(uname, pid, b"STAT FAILED", tabname, e);
            sys::close(fd);
            return None;
        }
    };
    *mtime = st.mtime;
    if !globals::permit_any_crontab() {
        let bad = if !sys::is_reg(st.mode) {
            Some(&b"NOT REGULAR"[..])
        } else if st.mode & 0o7533 != 0o400 {
            Some(&b"BAD FILE MODE"[..])
        } else if st.uid != 0 && pw.is_none_or(|p| st.uid != p.uid || uname != p.name.as_slice()) {
            Some(&b"WRONG FILE OWNER"[..])
        } else if pw.is_some() && st.nlink != 1 {
            Some(&b"BAD LINK COUNT"[..])
        } else {
            None
        };
        if let Some(event) = bad {
            globals::log_it(uname, pid, event, tabname, 0);
            sys::close(fd);
            return None;
        }
    }
    Some(fd)
}

/// Crontabs whose user doesn't exist (yet): retried by `check_orphans()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Orphan {
    pub uname: Vec<u8>,
    pub fname: Option<Vec<u8>>,
    pub tabname: Vec<u8>,
}

thread_local! {
    static FILE_NAME: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    static ORPHANS: std::cell::RefCell<Vec<Orphan>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn add_orphan(uname: &[u8], fname: Option<&[u8]>, tabname: &[u8]) {
    let o = Orphan { uname: uname.to_vec(), fname: fname.map(<[u8]>::to_vec), tabname: tabname.to_vec() };
    ORPHANS.with(|os| {
        let mut os = os.borrow_mut();
        if !os.contains(&o) {
            os.insert(0, o);
        }
    });
}

/// `find_user(db, name, tabname)`.
pub fn find_user(db: &CronDb, name: &[u8], tabname: Option<&[u8]>) -> Option<usize> {
    db.users.iter().position(|u| u.name == name && tabname.is_none_or(|t| t == u.tabname.as_slice()))
}

/// `process_crontab(uname, fname, tabname, new_db, old_db)`.
fn process_crontab(
    uname: &[u8],
    fname: Option<&[u8]>,
    tabname: &[u8],
    new_db: &mut CronDb,
    old_db: Option<&mut CronDb>,
) {
    let crond_crontab = fname.is_none() && tabname != SYSCRONTAB;
    let mut pw = None;
    let fname: &[u8] = match fname {
        None => b"*system*",
        Some(f) => {
            pw = sys::getpwnam(uname);
            if pw.is_none() {
                globals::log_it(uname, sys::getpid(), b"ORPHAN", b"no passwd entry", 0);
                add_orphan(uname, Some(f), tabname);
                return;
            }
            f
        }
    };
    let mut mtime = 0;
    let Some(fd) = check_open(tabname, uname, pw.as_ref(), &mut mtime) else {
        return;
    };
    mtime = mtime.min(new_db.mtime);
    globals::debug(DLOAD, || bcat!("\t", fname, ":"));
    if let Some(old) = old_db
        && let Some(i) = find_user(old, fname, if crond_crontab { Some(tabname) } else { None })
    {
        if old.users[i].mtime == mtime {
            globals::debug(DLOAD, || b" [no change, using old data]".to_vec());
            let u = old.users.remove(i);
            new_db.users.push(u);
            globals::debug(DLOAD, || b" [done]\n".to_vec());
            sys::close(fd);
            return;
        }
        globals::debug(DLOAD, || b" [delete old data]".to_vec());
        old.users.remove(i);
        globals::log_it(fname, sys::getpid(), b"RELOAD", tabname, 0);
    }
    if let Some(mut u) = load_user(fd, pw.as_ref(), uname, fname, tabname) {
        u.mtime = mtime;
        new_db.users.push(u);
    }
}

/// `not_a_crontab()`: names in the crontab directories to skip.
fn not_a_crontab(name: &[u8]) -> bool {
    if name.first() == Some(&b'.') || name.first() == Some(&b'#') {
        return true;
    }
    if name == CRON_HOSTNAME {
        return true;
    }
    let len = name.len();
    if len >= NAME_BUF - 1 || len == 0 {
        return true;
    }
    if name[len - 1] == b'~' {
        return true;
    }
    (len > 8 && (name.ends_with(b".rpmsave") || name.ends_with(b".rpmorig"))) || (len > 7 && name.ends_with(b".rpmnew"))
}

/// `max_mtime()`: the newest mtime of any crontab in a directory.
fn max_mtime(dir: &[u8], max: &mut i64) {
    let Ok(names) = sys::read_dir(dir) else {
        *max = 0;
        return;
    };
    for n in names {
        if not_a_crontab(&n) && n != CRON_HOSTNAME {
            continue;
        }
        let Some(tab) = env::glue_strings(NAME_BUF, dir, &n, b'/') else { continue };
        if let Ok(st) = sys::stat(&tab)
            && st.mtime > *max
        {
            *max = st.mtime;
        }
    }
}

/// `cluster_host_is_local()`.
fn cluster_host_is_local() -> bool {
    if !globals::enable_clustering() {
        return true;
    }
    let Some(filename) = env::glue_strings(NAME_BUF, SPOOL_DIR, CRON_HOSTNAME, b'/') else {
        return false;
    };
    let Ok(fd) = sys::open(&filename, libc::O_RDONLY, 0) else {
        globals::debug(DLOAD, || bcat!("cluster: file ", filename, " not found\n"));
        return false;
    };
    let mut r = Reader::new(sys::read_to_end(fd));
    sys::close(fd);
    let mut hostname = Vec::new();
    let ch = reader::get_string(&mut hostname, 64, &mut r, b"\n");
    match sys::gethostname() {
        Some(mine) if ch != reader::EOF => mine == hostname,
        _ => {
            globals::debug(DLOAD, || b"cluster: hostname comparison error\n".to_vec());
            false
        }
    }
}

/// `load_database(old_db)`: (re)load the system crontab, /etc/cron.d and
/// the spool, reusing unchanged crontabs.  Returns whether it reloaded.
pub fn load_database(old_db: &mut CronDb) -> bool {
    let pid = sys::getpid();
    globals::debug(DLOAD, || format!("[{pid}] load_database()\n").into_bytes());
    let now = sys::time_now();

    let dir_mtime = |dir: &[u8]| match sys::stat(dir) {
        Err(e) => {
            globals::log_it(b"CRON", pid, b"STAT FAILED", dir, e);
            0
        }
        Ok(st) => {
            let mut m = st.mtime;
            max_mtime(dir, &mut m);
            m
        }
    };
    let spool_mtime = dir_mtime(SPOOL_DIR);
    let crond_mtime = dir_mtime(SYS_CROND_DIR);
    let syscron_mtime = sys::stat(SYSCRONTAB).map_or(0, |s| s.mtime);

    if old_db.mtime != 0 && old_db.mtime == (now - 1).min(crond_mtime.max(spool_mtime.max(syscron_mtime))) {
        globals::debug(DLOAD, || format!("[{pid}] spool dir mtime unch, no load needed.\n").into_bytes());
        return false;
    }

    let mut new_db = CronDb { users: Vec::new(), mtime: now - 1 };
    if syscron_mtime != 0 {
        process_crontab(b"root", None, SYSCRONTAB, &mut new_db, Some(old_db));
    }
    match sys::read_dir(SYS_CROND_DIR) {
        Err(e) => globals::log_it(b"CRON", pid, b"OPENDIR FAILED", SYS_CROND_DIR, e),
        Ok(names) => {
            for n in names {
                if not_a_crontab(&n) {
                    continue;
                }
                let Some(tab) = env::glue_strings(NAME_BUF, SYS_CROND_DIR, &n, b'/') else { continue };
                process_crontab(b"root", None, &tab, &mut new_db, Some(old_db));
            }
        }
    }
    match sys::read_dir(SPOOL_DIR) {
        Err(e) => globals::log_it(b"CRON", pid, b"OPENDIR FAILED", SPOOL_DIR, e),
        Ok(names) => {
            if cluster_host_is_local() {
                for n in names {
                    if not_a_crontab(&n) {
                        continue;
                    }
                    let fname = &n[..n.len().min(NAME_BUF - 1)];
                    let Some(tab) = env::glue_strings(NAME_BUF, SPOOL_DIR, fname, b'/') else { continue };
                    process_crontab(fname, Some(fname), &tab, &mut new_db, Some(old_db));
                }
            }
        }
    }
    sys::endpwent();
    globals::debug(DLOAD, || b"unlinking old database:\n".to_vec());
    for u in &old_db.users {
        globals::debug(DLOAD, || bcat!("\t", u.name, "\n"));
    }
    *old_db = new_db;
    globals::debug(DLOAD, || b"load_database is done\n".to_vec());
    true
}
