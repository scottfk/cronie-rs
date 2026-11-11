//! `crond`: the cron daemon.  Port of src/cron.c, src/job.c,
//! src/do_command.c, src/popen.c, and the inotify half of src/database.c.
//!
//! Upstream license: ISC (see LICENSE).

use std::ffi::c_int;
use std::sync::atomic::{AtomicBool, Ordering};

use cronie::entry::{DOM_STAR, DONT_LOG, DOW_STAR, HR_STAR, MAIL_WHEN_ERR, MIN_STAR, WHEN_REBOOT};
use cronie::env::{env_get, env_update_home};
use cronie::getopt::Getopt;
use cronie::globals::{self, DEBUG_FLAG_NAMES, DLOAD, DPROC, DSCH, DTEST, debug, log_it};
use cronie::misc::{expand_envvar, first_word, mkprints};
use cronie::security;
use cronie::stdio::{self, CFile, err, out};
use cronie::sys;
use cronie::user::{self, CronDb, SPOOL_DIR, SYS_CROND_DIR, SYSCRONTAB};
use cronie::{PACKAGE_STRING, bcat};

const PACKAGE_VERSION: &[u8] = b"1.7.2";
const PATH_STDPATH: &[u8] = b"/usr/bin:/bin:/usr/sbin:/sbin";
const PATH_CRON_PID: &[u8] = b"/run/crond.pid";
const REBOOT_LOCK: &[u8] = b"/run/cron.reboot";
const MAILARG: &[u8] = b"/usr/sbin/sendmail";
const MAX_COMMAND: usize = 131_072;
const MAX_EMAILSTR: usize = 255;
const MINUTE_COUNT: i32 = 60;
const ERROR_EXIT: i32 = 1;

static GOT_SIGHUP: AtomicBool = AtomicBool::new(false);
static GOT_SIGCHLD: AtomicBool = AtomicBool::new(false);
static GOT_SIGINTTERM: AtomicBool = AtomicBool::new(false);
static GOT_SIGURG: AtomicBool = AtomicBool::new(false);

extern "C" fn sighup_handler(_: c_int) {
    GOT_SIGHUP.store(true, Ordering::SeqCst);
}
extern "C" fn sigchld_handler(_: c_int) {
    GOT_SIGCHLD.store(true, Ordering::SeqCst);
}
extern "C" fn sigintterm_handler(_: c_int) {
    GOT_SIGINTTERM.store(true, Ordering::SeqCst);
}
extern "C" fn sigurg_handler(_: c_int) {
    GOT_SIGURG.store(true, Ordering::SeqCst);
}

fn intterm() -> bool {
    GOT_SIGINTTERM.load(Ordering::SeqCst)
}

/// A queued job: (user index, entry index) in the database.
type Job = (usize, usize);

/// cron.c's and do_command.c's globals.
#[derive(Debug)]
struct Crond {
    program: Vec<u8>,
    no_fork: bool,
    syslog_output: bool,
    disable_inotify: bool,
    mail_cmd: Vec<u8>,
    mail_charset: Vec<u8>,
    time_running: i32,
    virtual_time: i32,
    clock_time: i32,
    gmtoff: i64,
    isdst: i32,
    start_time: i64,
    inotify_enabled: bool,
    wd: [c_int; 3],
    jobs: Vec<Job>,
    lock_fd: sys::Fd,
}

const WATCHPATHS: [&[u8]; 3] = [SPOOL_DIR, SYS_CROND_DIR, SYSCRONTAB];

impl Crond {
    fn usage(&self) -> ! {
        let p = &self.program;
        err(b"Usage:\n");
        err(&bcat!(" ", p, " [options]\n"));
        err(b"\n");
        err(b"Options:\n");
        err(b" -h         print this message \n");
        err(b" -i         daemon runs without inotify support\n");
        err(b" -m <comm>  off, or specify preferred client for sending mails\n");
        err(b" -n         run in foreground\n");
        err(b" -f         run in foreground, the same as -n\n");
        err(b" -p         permit any crontab\n");
        err(b" -P         inherit PATH from environment instead of using default value");
        err(&bcat!("            of \"", PATH_STDPATH, "\"\n"));
        err(b" -c         enable clustering support\n");
        err(b" -s         log into syslog instead of sending mails\n");
        err(b" -V         print version and exit\n");
        err(b" -x <flag>  print debug information\n");
        err(b"\n");
        err(b"Debugging flags are: ");
        for (i, f) in DEBUG_FLAG_NAMES.iter().enumerate() {
            err(&bcat!(f, if i + 1 < DEBUG_FLAG_NAMES.len() { "," } else { "\n" }));
        }
        stdio::exit(ERROR_EXIT)
    }

    /// `parse_args()`.
    fn parse_args(&mut self, args: Vec<Vec<u8>>) {
        let mut g = Getopt::new(args, "hnfpsiPx:m:cV");
        while let Some(c) = g.next_short() {
            let optarg = g.optarg.clone().unwrap_or_default();
            match u8::try_from(c).unwrap_or(0) {
                b'x' => {
                    if !globals::set_debug_flags(&optarg) {
                        self.usage();
                    }
                }
                b'n' | b'f' => self.no_fork = true,
                b'p' => globals::set_permit_any_crontab(true),
                b's' => self.syslog_output = true,
                b'i' => self.disable_inotify = true,
                b'P' => globals::set_change_path(false),
                b'm' => self.mail_cmd = optarg[..optarg.len().min(MAX_COMMAND)].to_vec(),
                b'c' => globals::set_enable_clustering(true),
                b'V' => {
                    out(format!("{PACKAGE_STRING}\n").as_bytes());
                    stdio::exit(libc::EXIT_SUCCESS);
                }
                _ => self.usage(),
            }
        }
    }

    /// `acquire_daemonlock(closeflag)`.
    fn acquire_daemonlock(&mut self, closeflag: bool) {
        let pid = sys::getpid();
        if closeflag {
            if self.lock_fd != -1 {
                sys::close(self.lock_fd);
                self.lock_fd = -1;
            }
            sys::signal_default(libc::SIGINT);
            sys::signal_default(libc::SIGTERM);
            return;
        }
        if self.lock_fd == -1 {
            let fd = match sys::open(PATH_CRON_PID, libc::O_RDWR | libc::O_CREAT, 0o600) {
                Ok(fd) => fd,
                Err(e) => {
                    let buf = bcat!("can't open or create ", PATH_CRON_PID);
                    err(&bcat!(self.program, ": ", buf, ": ", sys::strerror(e), "\n"));
                    log_it(b"CRON", pid, b"DEATH", &buf, e);
                    stdio::exit(ERROR_EXIT);
                }
            };
            if let Err(e) = sys::flock_try(fd) {
                let mut data = [0u8; 3 * 4096];
                let n = sys::read(fd, &mut data[..3 * 4096 - 1]);
                let text = cronie::bytes::c_str_prefix(&data[..n.max(0) as usize]);
                let parsed = cronie::num::strtoimax(text, 10);
                let buf = if n > 0
                    && parsed.value > 0
                    && parsed.end != 0
                    && text.get(parsed.end) == Some(&b'\n')
                    && parsed.value != i64::MAX
                {
                    bcat!("can't lock ", PATH_CRON_PID, ", otherpid may be ", parsed.value.to_string())
                } else {
                    bcat!("can't lock ", PATH_CRON_PID, ", otherpid unknown")
                };
                err(&bcat!(self.program, ": ", buf, ": ", sys::strerror(e), "\n"));
                log_it(b"CRON", pid, b"DEATH", &buf, e);
                stdio::exit(ERROR_EXIT);
            }
            let _ = sys::fchmod(fd, 0o644);
            sys::set_cloexec(fd);
            self.lock_fd = fd;
        }
        let buf = format!("{pid}\n");
        let _ = sys::lseek_set(self.lock_fd, 0);
        let n = sys::write(self.lock_fd, buf.as_bytes());
        if n != buf.len() as isize {
            log_it(b"CRON", pid, b"ERROR", b"write() failed", sys::cerrno());
        } else if let Err(e) = sys::ftruncate(self.lock_fd, n as i64) {
            log_it(b"CRON", pid, b"ERROR", b"ftruncate() failed", e);
        }
    }

    // --- inotify -----------------------------------------------------------

    fn reset_watches(&mut self) {
        self.wd = [-2; 3];
    }

    fn set_cron_unwatched(&mut self, fd: c_int) {
        for w in &mut self.wd {
            if *w > 0 {
                sys::inotify_rm_watch(fd, *w);
                *w = -1;
            }
        }
    }

    fn set_cron_watched(&mut self, fd: c_int) {
        let pid = sys::getpid();
        if fd < 0 {
            self.inotify_enabled = false;
            return;
        }
        for (i, &path) in WATCHPATHS.iter().enumerate() {
            let mask = libc::IN_CREATE
                | libc::IN_CLOSE_WRITE
                | libc::IN_ATTRIB
                | libc::IN_MODIFY
                | libc::IN_MOVED_TO
                | libc::IN_MOVED_FROM
                | libc::IN_MOVE_SELF
                | libc::IN_DELETE
                | libc::IN_DELETE_SELF;
            let w = match sys::inotify_add_watch(fd, path, mask) {
                Ok(w) => w,
                Err(e) if e == libc::ENOENT => -1,
                Err(e) => {
                    if self.wd[i] != -1 {
                        log_it(b"CRON", pid, b"This directory or file can't be watched", path, e);
                        log_it(b"CRON", pid, b"INFO", b"running without inotify support", 0);
                    }
                    self.inotify_enabled = false;
                    self.set_cron_unwatched(fd);
                    return;
                }
            };
            self.wd[i] = w;
        }
        if !self.inotify_enabled {
            log_it(b"CRON", pid, b"INFO", b"running with inotify support", 0);
        }
        self.inotify_enabled = true;
    }

    /// `check_inotify_database()`: reload everything if inotify saw a change.
    fn check_inotify_database(&mut self, old_db: &mut CronDb) {
        let pid = sys::getpid();
        match sys::readable_now(old_db.ifd) {
            Err(e) => {
                if e != libc::EINTR {
                    log_it(b"CRON", pid, b"INOTIFY", b"select failed", e);
                }
                return;
            }
            Ok(false) => return,
            Ok(true) => {}
        }
        let mut new_db = CronDb { users: Vec::new(), mtime: sys::time_now() - 1, ifd: old_db.ifd };
        let mut buf = vec![0u8; 1024 * (16 + 16)];
        let n = loop {
            let n = sys::read(old_db.ifd, &mut buf);
            if n == -1 && sys::cerrno() == libc::EINTR {
                continue;
            }
            break n;
        };
        if n <= 0 {
            let e = if n == 0 { libc::ENOMEM } else { sys::cerrno() };
            log_it(b"CRON", pid, b"INOTIFY", b"read failed", e);
            stdio::exit(ERROR_EXIT);
        }
        self.set_cron_watched(old_db.ifd);
        user::process_crontab(b"root", None, SYSCRONTAB, &mut new_db, Some(old_db));
        match sys::read_dir(SYS_CROND_DIR) {
            Err(e) => log_it(b"CRON", pid, b"OPENDIR FAILED", SYS_CROND_DIR, e),
            Ok(names) => {
                for n in names {
                    if user::not_a_crontab(&n) {
                        continue;
                    }
                    let Some(tab) = cronie::env::glue_strings(256, SYS_CROND_DIR, &n, b'/') else { continue };
                    user::process_crontab(b"root", None, &tab, &mut new_db, Some(old_db));
                }
            }
        }
        match sys::read_dir(SPOOL_DIR) {
            Err(e) => log_it(b"CRON", pid, b"OPENDIR FAILED", SPOOL_DIR, e),
            Ok(names) => {
                for n in names {
                    if user::not_a_crontab(&n) {
                        continue;
                    }
                    let Some(tab) = cronie::env::glue_strings(256, SPOOL_DIR, &n, b'/') else { continue };
                    user::process_crontab(&n, Some(&n), &tab, &mut new_db, Some(old_db));
                }
            }
        }
        sys::endpwent();
        debug(DLOAD, || b"unlinking old database:\n".to_vec());
        for u in &old_db.users {
            debug(DLOAD, || bcat!("\t", u.name, "\n"));
        }
        *old_db = new_db;
        debug(DLOAD, || b"check_inotify_database is done\n".to_vec());
    }

    // --- signals and time --------------------------------------------------

    fn handle_signals(&mut self, db: &mut CronDb) {
        if GOT_SIGHUP.swap(false, Ordering::SeqCst) | GOT_SIGURG.swap(false, Ordering::SeqCst) {
            if self.inotify_enabled && !globals::enable_clustering() {
                self.set_cron_unwatched(db.ifd);
                self.reset_watches();
                self.inotify_enabled = false;
            }
            db.mtime = 0;
            globals::log_close();
        }
        if GOT_SIGCHLD.swap(false, Ordering::SeqCst) {
            sigchld_reaper();
        }
    }

    /// `set_time()`: StartTime, and clockTime in local minutes.
    fn set_time(&mut self, initialize: bool) {
        self.start_time = sys::time_now();
        let tm = sys::localtime(self.start_time).unwrap_or_else(|| sys::localtime_r(self.start_time));
        if initialize || tm.tm_isdst != self.isdst {
            self.isdst = tm.tm_isdst;
            self.gmtoff = tm.tm_gmtoff;
            debug(DSCH, || format!("[{}] GMToff={}\n", sys::getpid(), self.gmtoff).into_bytes());
        }
        self.clock_time = ((self.start_time + self.gmtoff) / 60) as i32;
    }

    /// `cron_sleep()`: sleep until the target minute, servicing signals.
    fn cron_sleep(&mut self, target: i32, db: &mut CronDb) {
        let mut t1 = sys::time_now() + self.gmtoff;
        let mut seconds_to_wait = (i64::from(target) * 60 - t1) as i32;
        if seconds_to_wait == 0 {
            seconds_to_wait = 1;
        }
        debug(DSCH, || {
            format!("[{}] Target time={}, sec-to-wait={}\n", sys::getpid(), i64::from(target) * 60, seconds_to_wait)
                .into_bytes()
        });
        while seconds_to_wait > 0 && seconds_to_wait < 65 {
            sys::sleep(seconds_to_wait as u32);
            if intterm() {
                return;
            }
            self.handle_signals(db);
            let t2 = sys::time_now() + self.gmtoff;
            seconds_to_wait -= (t2 - t1) as i32;
            t1 = t2;
        }
    }

    // --- jobs --------------------------------------------------------------

    /// `job_add()`: queue a job once, refreshing its passwd entry.
    fn job_add(&mut self, db: &mut CronDb, ui: usize, ei: usize) {
        if self.jobs.contains(&(ui, ei)) {
            return;
        }
        let e = &mut db.users[ui].crontab[ei];
        let uname = e.pwd.name.clone();
        match sys::getpwnam(&uname) {
            Some(pw) => {
                debug(DSCH | globals::DEXT, || {
                    bcat!(
                        "user [",
                        e.pwd.name,
                        ":",
                        pw.uid.to_string(),
                        ":",
                        pw.gid.to_string(),
                        ":...] cmd=\"",
                        e.cmd,
                        "\"\n"
                    )
                });
                e.pwd = pw;
                let dir = e.pwd.dir.clone();
                env_update_home(&mut e.envp, &dir);
            }
            None => {
                log_it(&uname, sys::getpid(), b"ERROR", b"getpwnam() failed - user unknown", sys::cerrno());
                return;
            }
        }
        self.jobs.push((ui, ei));
    }

    /// `job_runqueue()`: start every queued job; how many there were.
    fn job_runqueue(&mut self, db: &CronDb) -> usize {
        let jobs = std::mem::take(&mut self.jobs);
        for &(ui, ei) in &jobs {
            self.do_command(db, ui, ei);
        }
        jobs.len()
    }

    fn run_reboot_jobs(&mut self, db: &mut CronDb) {
        let pid = sys::getpid();
        if sys::access(REBOOT_LOCK, libc::F_OK) {
            log_it(b"CRON", pid, b"INFO", b"@reboot jobs will be run at computer's startup.", 0);
            return;
        }
        // QUIRK: S_IRUSR & S_IWUSR is 0: the lock file gets mode 0 (it
        // only needs to exist).
        match sys::creat(REBOOT_LOCK, libc::S_IRUSR & libc::S_IWUSR) {
            Err(e) => log_it(b"CRON", pid, b"INFO", b"Can't create lock for reboot jobs.", e),
            Ok(fd) => {
                sys::close(fd);
            }
        }
        for ui in 0..db.users.len() {
            for ei in 0..db.users[ui].crontab.len() {
                if db.users[ui].crontab[ei].flags & WHEN_REBOOT != 0 {
                    self.job_add(db, ui, ei);
                }
            }
        }
        self.job_runqueue(db);
    }

    /// `find_jobs()`: queue the jobs due at `vtime` (local minutes).
    fn find_jobs(&mut self, vtime: i32, db: &mut CronDb, do_wild: bool, do_non_wild: bool, v_gmtoff: i64) {
        let orig_tz = sys::getenv(b"TZ");
        for ui in 0..db.users.len() {
            for ei in 0..db.users[ui].crontab.len() {
                let e = &db.users[ui].crontab[ei];
                let virtual_second = (i64::from(vtime) - i64::from(e.delay)) * 60;
                let virtual_gmt_second = virtual_second - v_gmtoff;
                let job_tz = env_get(b"CRON_TZ", &e.envp).map(<[u8]>::to_vec);
                // maketime(job_tz, orig_tz)
                let tm = match &job_tz {
                    Some(t) if !t.is_empty() => {
                        let _ = sys::setenv(b"TZ", t);
                        sys::localtime(virtual_gmt_second)
                    }
                    _ => {
                        match &orig_tz {
                            Some(t) => {
                                let _ = sys::setenv(b"TZ", t);
                            }
                            None => sys::unsetenv(b"TZ"),
                        }
                        sys::gmtime(virtual_second)
                    }
                };
                let Some(tm) = tm else { continue };
                let minute = tm.tm_min;
                let hour = tm.tm_hour;
                let dom = tm.tm_mday - 1;
                let month = tm.tm_mon;
                let dow = tm.tm_wday;
                let day = if e.flags & DOM_STAR != 0 || e.flags & DOW_STAR != 0 {
                    e.dow.test(dow) && e.dom.test(dom)
                } else {
                    e.dow.test(dow) || e.dom.test(dom)
                };
                if e.minute.test(minute) && e.hour.test(hour) && e.month.test(month) && day {
                    if job_tz.is_some() && v_gmtoff != self.gmtoff {
                        // not during the DST switch of the default zone
                        continue;
                    }
                    let wild = e.flags & (MIN_STAR | HR_STAR) != 0;
                    if (do_non_wild && !wild) || (do_wild && wild) {
                        self.job_add(db, ui, ei);
                    }
                }
            }
        }
        match orig_tz {
            Some(t) => {
                let _ = sys::setenv(b"TZ", &t);
            }
            None => sys::unsetenv(b"TZ"),
        }
    }

    /// `do_command()`: run one job in a child, asynchronously.
    fn do_command(&mut self, db: &CronDb, ui: usize, ei: usize) {
        let pid = sys::getpid();
        let u = &db.users[ui];
        let e = &u.crontab[ei];
        debug(DPROC, || {
            bcat!(
                "[",
                pid.to_string(),
                "] do_command(",
                e.cmd,
                ", (",
                u.name,
                ",",
                e.pwd.uid.to_string(),
                ",",
                e.pwd.gid.to_string(),
                "))\n"
            )
        });
        match sys::fork() {
            Err(errno) => log_it(b"CRON", pid, b"CAN'T FORK", b"do_command", errno),
            Ok(0) => {
                self.acquire_daemonlock(true);
                let Some(jobenv) = security::cron_set_job_security_context(e, u.system) else {
                    sys::exit_now(ERROR_EXIT);
                };
                let ev = self.child_process(e, jobenv);
                security::cron_close_pam();
                debug(DPROC, || format!("[{}] child process done, exiting\n", sys::getpid()).into_bytes());
                // _exit(): buffered stdout (debug output) is discarded, as in C.
                sys::exit_now(ev);
            }
            Ok(_) => {}
        }
        debug(DPROC, || format!("[{pid}] main process returning to work\n").into_bytes());
    }

    /// `child_process()`: run the job, feed it its `%` input, and mail (or
    /// log) its output.
    fn child_process(&mut self, e: &cronie::entry::Entry, jobenv: cronie::env::Env) -> i32 {
        let pid = sys::getpid();
        sys::sigaction_ignore(libc::SIGPIPE);
        sys::sigaction(libc::SIGCHLD, None, 0);
        debug(DPROC, || bcat!("[", sys::getpid().to_string(), "] child_process('", e.cmd, "')\n"));
        // CAPITALIZE_FOR_PS: upshift ProgramName (argv[0]) in place.
        let upper = sys::capitalize_program_name();
        globals::rename_program(&upper);

        let usernm = e.pwd.name.clone();
        let mut mailto = env_get(b"MAILTO", &jobenv).map(<[u8]>::to_vec);
        let mut mailfrom = env_get(b"MAILFROM", &e.envp).map(<[u8]>::to_vec);
        if let Some(m) = &mailto {
            match expand_envvar(m, MAX_EMAILSTR) {
                Some(x) => mailto = Some(x),
                None => log_it(
                    b"CRON",
                    pid,
                    b"WARNING",
                    b"The environment variable 'MAILTO' could not be expanded. The non-expanded value will be used.",
                    0,
                ),
            }
        }
        if let Some(m) = &mailfrom {
            match expand_envvar(m, MAX_EMAILSTR) {
                Some(x) => mailfrom = Some(x),
                None => log_it(
                    b"CRON",
                    pid,
                    b"WARNING",
                    b"The environment variable 'MAILFROM' could not be expanded. The non-expanded value will be used.",
                    0,
                ),
            }
        }

        let stdin_pipe = match sys::pipe() {
            Ok(p) => p,
            Err(errno) => {
                log_it(b"CRON", pid, b"PIPE() FAILED", b"stdin_pipe", errno);
                return ERROR_EXIT;
            }
        };
        let stdout_pipe = match sys::pipe() {
            Ok(p) => p,
            Err(errno) => {
                log_it(b"CRON", pid, b"PIPE() FAILED", b"stdout_pipe", errno);
                return ERROR_EXIT;
            }
        };

        // Split at the first unescaped '%': the command, then its input.
        // "\%" before it becomes "%".
        let (cmd, input_data) = {
            let src = &e.cmd;
            let mut cmd = Vec::new();
            let mut escaped = false;
            let mut i = 0;
            let mut input: &[u8] = &[];
            while i < src.len() {
                let ch = src[i];
                cmd.push(ch);
                if escaped {
                    if ch == b'%' {
                        cmd.pop();
                        cmd.pop();
                        cmd.push(b'%');
                    }
                    escaped = false;
                    i += 1;
                    continue;
                }
                if ch == b'\\' {
                    escaped = true;
                    i += 1;
                    continue;
                }
                if ch == b'%' {
                    cmd.pop();
                    input = &src[i + 1..];
                    break;
                }
                i += 1;
            }
            (cmd, input.to_vec())
        };

        let jobpid = match sys::fork() {
            Err(errno) => {
                log_it(b"CRON", pid, b"CAN'T FORK", b"child_process", errno);
                return ERROR_EXIT;
            }
            Ok(0) => {
                debug(DPROC, || format!("[{}] grandchild process fork()'ed\n", sys::getpid()).into_bytes());
                if e.flags & DONT_LOG == 0 {
                    log_it(&usernm, sys::getpid(), b"CMD", &mkprints(&cmd), 0);
                }
                if security::cron_change_user_permanently(&e.pwd, env_get(b"HOME", &jobenv)) < 0 {
                    sys::exit_now(ERROR_EXIT);
                }
                sys::setsid();
                sys::sigaction(libc::SIGPIPE, None, 0);
                sys::close(stdin_pipe[1]);
                sys::close(stdout_pipe[0]);
                if stdin_pipe[0] != 0 {
                    sys::dup2(stdin_pipe[0], 0);
                    sys::close(stdin_pipe[0]);
                }
                if stdout_pipe[1] != 1 {
                    sys::dup2(stdout_pipe[1], 1);
                    sys::close(stdout_pipe[1]);
                }
                sys::dup2(1, 2);
                let shell = env_get(b"SHELL", &jobenv).map(<[u8]>::to_vec).unwrap_or_default();
                match sys::open_fds() {
                    Some(fds) => {
                        for fd in fds {
                            if fd > 2 {
                                sys::close(fd);
                            }
                        }
                    }
                    None => {
                        for fd in 3..sys::getdtablesize().min(10000) {
                            sys::close(fd);
                        }
                    }
                }
                if globals::debug_flags() & DTEST != 0 {
                    err(b"debug DTEST is on, not exec'ing command.\n");
                    err(&bcat!("\tcmd='", cmd, "' shell='", shell, "'\n"));
                    sys::exit_now(0);
                }
                sys::execle(&shell, &[&shell, b"-c", &cmd], &jobenv);
                err(&bcat!("execl: couldn't exec `", shell, "'\n"));
                stdio::perror(b"execl");
                sys::exit_now(ERROR_EXIT);
            }
            Ok(p) => p,
        };
        let mut children = 1;
        debug(DPROC, || format!("[{}] child continues, closing pipes\n", sys::getpid()).into_bytes());
        sys::close(stdin_pipe[0]);
        sys::close(stdout_pipe[1]);

        // The input after '%', from yet another child (so a job that
        // doesn't read its stdin can't block us).
        if !input_data.is_empty() && sys::fork() == Ok(0) {
            debug(DPROC, || format!("[{}] child2 sending data to grandchild\n", sys::getpid()).into_bytes());
            sys::sigaction(libc::SIGPIPE, None, 0);
            sys::close(stdout_pipe[0]);
            if security::cron_change_user_permanently(&e.pwd, env_get(b"HOME", &jobenv)) < 0 {
                sys::exit_now(ERROR_EXIT);
            }
            let mut out = CFile::from_fd(stdin_pipe[1]);
            let mut need_newline = false;
            let mut escaped = false;
            for &c in &input_data {
                let mut ch = c;
                if escaped {
                    if ch != b'%' {
                        out.write(b"\\");
                    }
                } else if ch == b'%' {
                    ch = b'\n';
                }
                escaped = ch == b'\\';
                if !escaped {
                    out.write(&[ch]);
                    need_newline = ch != b'\n';
                }
            }
            if escaped {
                out.write(b"\\");
            }
            if need_newline {
                out.write(b"\n");
            }
            out.close();
            debug(DPROC, || format!("[{}] child2 done sending to grandchild\n", sys::getpid()).into_bytes());
            sys::exit_now(0);
        }
        sys::close(stdin_pipe[1]);
        children += 1;

        debug(DPROC, || format!("[{}] child reading output from grandchild\n", sys::getpid()).into_bytes());
        let mut input = cronie::reader::Reader::from_fd(stdout_pipe[0]);
        let ch = input.getc();
        if ch != cronie::reader::EOF {
            let mut mail: Option<Popen> = None;
            let mut bytes = 1usize;
            let mut status = 0;
            let mut logbuf: Vec<u8> = Vec::new();
            if self.syslog_output && ch != i32::from(b'\n') {
                logbuf.push(ch as u8);
            }
            debug(DPROC | globals::DEXT, || {
                format!("[{}] got data ({ch:x}:{}) from grandchild\n", sys::getpid(), ch as u8 as char).into_bytes()
            });
            let mailto = match mailto {
                Some(m) if m.is_empty() => None,
                Some(m) => Some(m),
                None => Some(usernm.clone()),
            };
            let mailfrom = match mailfrom {
                Some(m) if !m.is_empty() && safe_p(&usernm, &m) => m,
                _ => e.pwd.name.clone(),
            };
            if let Some(mailto) = mailto.as_deref().filter(|m| safe_p(&usernm, m))
                && !self.mail_cmd.starts_with(b"off")
                && !self.syslog_output
            {
                let hostname = sys::gethostname().unwrap_or_default();
                let mailcmd = if self.mail_cmd.is_empty() {
                    let c = bcat!(MAILARG, " -FCronDaemon -i -odi -oem -oi -t -f ", mailfrom);
                    if c.len() > MAX_COMMAND {
                        err(b"mailcmd too long\n");
                        sys::exit_now(ERROR_EXIT);
                    }
                    c
                } else {
                    self.mail_cmd.clone()
                };
                let Some(mut m) = cron_popen(&mailcmd, &e.pwd, &jobenv) else {
                    stdio::perror(&mailcmd);
                    sys::exit_now(ERROR_EXIT);
                };
                m.file.write(&bcat!("From: \"(Cron Daemon)\" <", mailfrom, ">\n"));
                m.file.write(&bcat!("To: ", mailto, "\n"));
                m.file.write(&bcat!("Subject: Cron <", usernm, "@", first_word(&hostname, b"."), "> ", cmd, "\n"));
                m.file.write(b"MIME-Version: 1.0\n");
                match env_get(b"CONTENT_TYPE", &jobenv) {
                    None => m.file.write(&bcat!("Content-Type: text/plain; charset=", self.mail_charset, "\n")),
                    Some(ct) => {
                        let ct: Vec<u8> = ct.iter().map(|&b| if b == b'\n' { b' ' } else { b }).collect();
                        m.file.write(&bcat!("Content-Type: ", ct, "\n"));
                    }
                }
                match env_get(b"CONTENT_TRANSFER_ENCODING", &jobenv) {
                    None => m.file.write(b"Content-Transfer-Encoding: 8bit\n"),
                    Some(ct) => {
                        let ct: Vec<u8> = ct.iter().map(|&b| if b == b'\n' { b' ' } else { b }).collect();
                        m.file.write(&bcat!("Content-Transfer-Encoding: ", ct, "\n"));
                    }
                }
                m.file.write(b"Auto-Submitted: auto-generated\n");
                m.file.write(b"Precedence: bulk\n");
                for v in &jobenv {
                    m.file.write(&bcat!("X-Cron-Env: <", v, ">\n"));
                }
                m.file.write(b"\n");
                // QUIRK: the first byte isn't checked for '\r' (later
                // ones are dropped).
                m.file.write(&[ch as u8]);
                mail = Some(m);
            }
            loop {
                let ch = input.getc();
                if ch == cronie::reader::EOF {
                    break;
                }
                if ch == i32::from(b'\r') {
                    continue;
                }
                bytes += 1;
                if let Some(m) = &mut mail {
                    m.file.write(&[ch as u8]);
                }
                if self.syslog_output {
                    logbuf.push(ch as u8);
                    if ch == i32::from(b'\n') || logbuf.len() == 1023 {
                        if ch == i32::from(b'\n') {
                            logbuf.pop();
                        }
                        log_it(&usernm, sys::getpid(), b"CMDOUT", &logbuf, 0);
                        logbuf.clear();
                    }
                }
            }
            if mail.is_some() && e.flags & MAIL_WHEN_ERR != 0 {
                let mut jobstatus = -1;
                if jobpid > 0 {
                    loop {
                        let (r, st) = sys::waitpid(jobpid);
                        if r == -1 {
                            if sys::cerrno() == libc::EINTR {
                                continue;
                            }
                            log_it(b"CRON", sys::getpid(), b"error", b"invalid job pid", sys::cerrno());
                            break;
                        }
                        jobstatus = st;
                        break;
                    }
                } else {
                    log_it(b"CRON", sys::getpid(), b"error", b"invalid job pid", 0);
                }
                if libc::WIFEXITED(jobstatus) && libc::WEXITSTATUS(jobstatus) == 0 {
                    debug(DPROC, || format!("[{}] aborting pipe to mail\n", sys::getpid()).into_bytes());
                    status = cron_finalize(mail.take().expect("mail"), libc::SIGKILL);
                    if status == libc::SIGKILL {
                        status = 0;
                    }
                }
            }
            let had_mail = mail.is_some();
            if let Some(m) = mail {
                debug(DPROC, || format!("[{}] closing pipe to mail\n", sys::getpid()).into_bytes());
                status = cron_finalize(m, 0);
            }
            if self.syslog_output && !logbuf.is_empty() {
                log_it(&usernm, sys::getpid(), b"CMDOUT", &logbuf, 0);
            }
            if had_mail && status != 0 && !self.syslog_output {
                let msg = format!(
                    "mailed {bytes} byte{} of output but got status 0x{status:04x}\n",
                    if bytes == 1 { "" } else { "s" }
                );
                log_it(&usernm, sys::getpid(), b"MAIL", msg.as_bytes(), 0);
            }
        }
        debug(DPROC, || format!("[{}] got EOF from grandchild\n", sys::getpid()).into_bytes());
        sys::close(stdout_pipe[0]);

        while children > 0 {
            debug(DPROC, || format!("[{}] waiting for grandchild #{children} to finish\n", sys::getpid()).into_bytes());
            let (child, waiter) = loop {
                let r = sys::wait();
                if r.0 < 0 && sys::cerrno() == libc::EINTR {
                    continue;
                }
                break r;
            };
            if child < 0 {
                debug(DPROC, || format!("[{}] no more grandchildren--mail written?\n", sys::getpid()).into_bytes());
                break;
            }
            debug(DPROC, || {
                format!("[{}] grandchild #{child} finished, status={:04x}", sys::getpid(), libc::WEXITSTATUS(waiter))
                    .into_bytes()
            });
            if libc::WIFSIGNALED(waiter) && libc::WCOREDUMP(waiter) {
                debug(DPROC, || b", dumped core".to_vec());
            }
            debug(DPROC, || b"\n".to_vec());
            children -= 1;
        }
        if e.flags & DONT_LOG == 0 {
            log_it(&usernm, sys::getpid(), b"CMDEND", &mkprints(&cmd), 0);
        }
        0
    }
}

/// `sigchld_reaper()`.
fn sigchld_reaper() {
    loop {
        let (pid, waiter) = sys::waitpid_nohang();
        match pid {
            -1 => {
                if sys::cerrno() == libc::EINTR {
                    continue;
                }
                debug(DPROC, || format!("[{}] sigchld...no children\n", sys::getpid()).into_bytes());
            }
            0 => debug(DPROC, || format!("[{}] sigchld...no dead kids\n", sys::getpid()).into_bytes()),
            _ => debug(DPROC, || {
                format!("[{}] sigchld...pid #{pid} died, stat={}\n", sys::getpid(), libc::WEXITSTATUS(waiter))
                    .into_bytes()
            }),
        }
        if pid <= 0 {
            break;
        }
    }
}

/// `safe_p()`: is a mail address safe to pass to sendmail?
fn safe_p(usernm: &[u8], s: &[u8]) -> bool {
    const SAFE_DELIM: &[u8] = b"@!:%-.,_+";
    for (i, &ch) in s.iter().enumerate() {
        if ch.is_ascii()
            && (ch.is_ascii_graphic() || ch == b' ')
            && (ch.is_ascii_alphanumeric() || (i != 0 && SAFE_DELIM.contains(&ch)))
        {
            continue;
        }
        log_it(usernm, sys::getpid(), b"UNSAFE", s, 0);
        return false;
    }
    true
}

/// A `cron_popen()`ed mail pipe.
#[derive(Debug)]
struct Popen {
    file: CFile,
    pid: i32,
}

/// `cron_popen(program, "w", pw, jobenv)`: run `program` (split on blanks,
/// no shell) as the user, with a pipe to its stdin.
fn cron_popen(program: &[u8], pw: &sys::Passwd, jobenv: &cronie::env::Env) -> Option<Popen> {
    let fds = sys::getdtablesize().min(10000);
    if fds <= 0 {
        return None;
    }
    let pdes = sys::pipe().ok()?;
    if pdes[0] >= fds || pdes[1] >= fds {
        sys::close(pdes[0]);
        sys::close(pdes[1]);
        return None;
    }
    let argv: Vec<Vec<u8>> =
        program.split(|b| b" \t\n".contains(b)).filter(|w| !w.is_empty()).take(1023).map(<[u8]>::to_vec).collect();
    match sys::fork() {
        Err(_) => {
            sys::close(pdes[0]);
            sys::close(pdes[1]);
            None
        }
        Ok(0) => {
            if pdes[0] != 0 {
                sys::dup2(pdes[0], 0);
                sys::close(pdes[0]);
            }
            sys::close(pdes[1]);
            sys::sigaction(libc::SIGPIPE, None, 0);
            for fd in 3..fds {
                sys::close(fd);
            }
            if security::cron_change_user_permanently(pw, env_get(b"HOME", jobenv)) != 0 {
                sys::exit_now(2);
            }
            let file = argv.first().cloned().unwrap_or_default();
            let e = sys::execvpe(&file, &argv, jobenv);
            log_it(b"CRON", sys::getpid(), b"EXEC FAILED", program, e);
            let mut buf = [0u8; 4096];
            loop {
                let n = sys::read(0, &mut buf);
                if n == 0 || (n == -1 && sys::cerrno() != libc::EINTR) {
                    break;
                }
            }
            sys::exit_now(1);
        }
        Ok(pid) => {
            sys::close(pdes[0]);
            Some(Popen { file: CFile::from_fd(pdes[1]), pid })
        }
    }
}

/// `cron_finalize()`: close (or kill) the mailer and wait for it; its
/// exit status or terminating signal.
fn cron_finalize(p: Popen, sig: c_int) -> i32 {
    let Popen { file, pid } = p;
    let file = if sig == 0 {
        file.close();
        None
    } else {
        if sys::kill(pid, sig).is_err() {
            return -1;
        }
        Some(file)
    };
    let oset = sys::block_int_quit_hup();
    let (got, stat) = loop {
        let (r, st) = sys::wait();
        if r == pid || r == -1 {
            break (r, st);
        }
    };
    sys::sigrestore(&oset);
    if let Some(f) = file {
        f.close();
    }
    if got < 0 {
        return got;
    }
    if libc::WIFEXITED(stat) { libc::WEXITSTATUS(stat) } else { libc::WTERMSIG(stat) }
}

fn main() {
    let args: Vec<Vec<u8>> = std::env::args_os().map(std::os::unix::ffi::OsStringExt::into_vec).collect();
    let argv0 = args.first().cloned().unwrap_or_default();
    let program = match argv0.iter().rposition(|&b| b == b'/') {
        Some(i) => argv0[i + 1..].to_vec(),
        None => argv0,
    };
    globals::set_program_name(&program);
    let mut c = Crond {
        program,
        no_fork: false,
        syslog_output: false,
        disable_inotify: false,
        mail_cmd: Vec::new(),
        mail_charset: Vec::new(),
        time_running: 0,
        virtual_time: 0,
        clock_time: 0,
        gmtoff: 0,
        isdst: 0,
        start_time: 0,
        inotify_enabled: false,
        wd: [-2; 3],
        jobs: Vec::new(),
        lock_fd: -1,
    };
    let mut pid = sys::getpid();
    sys::setlocale_all();
    globals::set_change_path(true);
    c.parse_args(args);

    sys::sigaction(libc::SIGCHLD, Some(sigchld_handler), libc::SA_RESTART);
    sys::sigaction(libc::SIGHUP, Some(sighup_handler), libc::SA_RESTART);
    sys::sigaction(libc::SIGINT, Some(sigintterm_handler), libc::SA_RESTART);
    sys::sigaction(libc::SIGTERM, Some(sigintterm_handler), libc::SA_RESTART);
    sys::sigaction(libc::SIGURG, Some(sigurg_handler), libc::SA_RESTART);

    c.acquire_daemonlock(false);
    if sys::setuid(0).is_err() {
        stdio::perror(b"setuid");
        stdio::exit(ERROR_EXIT);
    }
    cronie::misc::check_spool_dir();
    if globals::change_path()
        && let Err(e) = sys::setenv(b"PATH", PATH_STDPATH)
    {
        log_it(b"CRON", pid, b"DEATH", b"can't setenv PATH", e);
        stdio::exit(1);
    }
    sys::setlocale_all();
    c.mail_charset = sys::codeset().unwrap_or_else(|| b"US-ASCII".to_vec());

    if globals::debug_flags() != 0 {
        err(format!("[{}] cron started\n", sys::getpid()).as_bytes());
    } else if !c.no_fork {
        match sys::fork() {
            Err(e) => {
                log_it(b"CRON", pid, b"DEATH", b"can't fork", e);
                stdio::exit(0);
            }
            Ok(0) => {
                sys::setsid();
                if let Ok(fd) = sys::open(b"/dev/null", libc::O_RDWR, 0) {
                    sys::dup2(fd, 0);
                    sys::dup2(fd, 1);
                    sys::dup2(fd, 2);
                    if fd != 2 {
                        sys::close(fd);
                    }
                }
            }
            Ok(_) => sys::exit_now(0),
        }
    }
    log_it(b"CRON", sys::getpid(), b"STARTUP", PACKAGE_VERSION, 0);
    if !c.syslog_output && c.mail_cmd.is_empty() && !sys::access(b"/usr/sbin/sendmail", libc::X_OK) {
        c.syslog_output = true;
        log_it(b"CRON", pid, b"INFO", b"Syslog will be used instead of sendmail.", 0);
    }
    pid = sys::getpid();
    let usec = sys::gettimeofday_usec();
    cronie::random::srandom((i64::from(pid) + usec) as u32);
    let scale = cronie::random::random() as f64 / (1u64 << 31) as f64;
    globals::set_random_scale(scale);
    log_it(
        b"CRON",
        pid,
        b"INFO",
        format!("RANDOM_DELAY will be scaled with factor {}% if used.", (scale * 100.0) as i32).as_bytes(),
        0,
    );
    c.acquire_daemonlock(false);

    let mut fd = -1;
    let mut db = CronDb { users: Vec::new(), mtime: 0, ifd: -1 };
    if c.disable_inotify || globals::enable_clustering() {
        log_it(b"CRON", sys::getpid(), b"No inotify - daemon runs with -i or -c option", b"", 0);
    } else {
        c.reset_watches();
        fd = match sys::inotify_init() {
            Ok(f) => f,
            Err(e) => {
                log_it(b"CRON", pid, b"INFO", b"Inotify init failed", e);
                -1
            }
        };
        db.ifd = fd;
        if fd >= 0 {
            sys::set_cloexec(fd);
        }
        c.set_cron_watched(fd);
    }
    user::load_database(&mut db);
    c.set_time(true);
    c.run_reboot_jobs(&mut db);
    c.time_running = c.clock_time;
    c.virtual_time = c.clock_time;
    let mut old_gmtoff = c.gmtoff;

    while !intterm() {
        loop {
            c.cron_sleep(c.time_running + 1, &mut db);
            c.set_time(false);
            if intterm() || c.clock_time != c.time_running {
                break;
            }
        }
        if intterm() {
            break;
        }
        c.time_running = c.clock_time;
        let time_diff = c.time_running - c.virtual_time;
        user::check_orphans(&mut db);
        if c.inotify_enabled {
            c.check_inotify_database(&mut db);
        } else if user::load_database(&mut db) && !globals::enable_clustering() {
            // try reinstating the watches
            c.set_cron_watched(fd);
        }
        if time_diff == 1 {
            c.virtual_time = c.time_running;
            old_gmtoff = c.gmtoff;
            c.find_jobs(c.virtual_time, &mut db, true, true, old_gmtoff);
        } else if !(-(3 * MINUTE_COUNT)..=3 * MINUTE_COUNT).contains(&time_diff) {
            debug(DSCH, || format!("[{pid}], clock jumped\n").into_bytes());
            c.virtual_time = c.time_running;
            old_gmtoff = c.gmtoff;
            c.find_jobs(c.time_running, &mut db, true, true, c.gmtoff);
        } else if time_diff > 5 {
            debug(DSCH, || format!("[{pid}], DST begins {time_diff} minutes to go\n").into_bytes());
            c.find_jobs(c.time_running, &mut db, true, false, c.gmtoff);
            loop {
                if c.job_runqueue(&db) > 0 {
                    sys::sleep(10);
                }
                c.virtual_time += 1;
                if c.virtual_time >= c.time_running {
                    old_gmtoff = c.gmtoff;
                }
                c.find_jobs(c.virtual_time, &mut db, false, true, old_gmtoff);
                c.set_time(false);
                if !(c.virtual_time < c.time_running && c.clock_time == c.time_running) {
                    break;
                }
            }
        } else if time_diff > 0 {
            debug(DSCH, || format!("[{pid}], normal case {time_diff} minutes to go\n").into_bytes());
            loop {
                if c.job_runqueue(&db) > 0 {
                    sys::sleep(10);
                }
                c.virtual_time += 1;
                if c.virtual_time >= c.time_running {
                    old_gmtoff = c.gmtoff;
                }
                c.find_jobs(c.virtual_time, &mut db, true, true, old_gmtoff);
                if c.virtual_time >= c.time_running {
                    break;
                }
            }
        } else {
            debug(DSCH, || format!("[{pid}], DST ends {time_diff} minutes to go\n").into_bytes());
            c.find_jobs(c.time_running, &mut db, true, false, c.gmtoff);
        }
        c.job_runqueue(&db);
        c.handle_signals(&mut db);
    }
    if c.inotify_enabled && !globals::enable_clustering() {
        c.set_cron_unwatched(fd);
    }
    if fd >= 0 && sys::close(fd) < 0 {
        log_it(b"CRON", pid, b"INFO", b"Inotify close failed", sys::cerrno());
    }
    log_it(b"CRON", pid, b"INFO", b"Shutting down", 0);
    let _ = sys::unlink(PATH_CRON_PID);
    stdio::exit(0)
}
