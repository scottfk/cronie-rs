//! `anacron`: run periodic jobs that a machine that isn't always on would
//! otherwise miss.  Port of anacron/ (main.c, readtab.c, lock.c, runjob.c,
//! log.c, gregor.c, matchrx.c).
//!
//! Upstream license: GPL-2.0-or-later (see COPYING.anacron).

use std::ffi::{CStr, c_int};
use std::sync::atomic::{AtomicBool, Ordering};

use cronie::getopt::Getopt;
use cronie::misc::expand_envvar;
use cronie::reader::{EOF, Reader};
use cronie::stdio::{self, err, out};
use cronie::sys;
use cronie::{PACKAGE_STRING, bcat};

const ANACRONTAB: &[u8] = b"/etc/anacrontab";
const ANACRON_SPOOL_DIR: &[u8] = b"/var/spool/anacron";
const SENDMAIL: &[u8] = b"/usr/sbin/sendmail";
const MAX_MSG: usize = 150;
const MAX_EMAILSTR: usize = 255;
const FAILURE_EXIT: i32 = 1;
const PATH_MAX: usize = 4096;

static GOT_SIGALRM: AtomicBool = AtomicBool::new(false);
static GOT_SIGCHLD: AtomicBool = AtomicBool::new(false);
static GOT_SIGUSR1: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigalrm(_: c_int) {
    GOT_SIGALRM.store(true, Ordering::SeqCst);
}
extern "C" fn handle_sigchld(_: c_int) {
    GOT_SIGCHLD.store(true, Ordering::SeqCst);
}
extern "C" fn handle_sigusr1(_: c_int) {
    GOT_SIGUSR1.store(true, Ordering::SeqCst);
}

/// `job_rec`.
#[derive(Debug, Clone, Default)]
struct Job {
    period: i32,
    named_period: i32,
    delay: i32,
    ident: Vec<u8>,
    command: Vec<u8>,
    mailto: Option<Vec<u8>>,
    no_mail_output: bool,
    tab_line: i32,
    arg_num: i32,
    timestamp_fd: sys::Fd,
    input_fd: sys::Fd,
    output_fd: sys::Fd,
    mail_header_size: i64,
    job_pid: i32,
    mailer_pid: i32,
    /// Index of the last environment assignment before this job, if any.
    prev_env_rec: Option<usize>,
}

/// anacron's globals.
#[derive(Debug, Default)]
struct Anacron {
    primary_pid: i32,
    program_name: Vec<u8>,
    anacrontab: Vec<u8>,
    spooldir: Vec<u8>,
    serialize: bool,
    force: bool,
    update_only: bool,
    now: bool,
    no_daemon: bool,
    quiet: bool,
    testing_only: bool,
    day_now: i32,
    year: i32,
    month: i32,
    day_of_month: i32,
    in_background: bool,
    jobs: Vec<Job>,
    env: Vec<Vec<u8>>,
    job_args: Vec<Vec<u8>>,
    /// Indices into `jobs`, in execution order.
    job_array: Vec<usize>,
    running_jobs: i32,
    running_mailers: i32,
    complaints: i32,
    start_sec: i64,
    range_start: i32,
    range_stop: i32,
    preferred_hour: i32,
    random_number: i32,
    line_num: i32,
    log_open: bool,
    old_sigmask: Option<libc::sigset_t>,
    username: Option<Vec<u8>>,
}

const EXPLAIN_LEVEL: c_int = libc::LOG_NOTICE;
const COMPLAIN_LEVEL: c_int = libc::LOG_ERR;

impl Anacron {
    // --- log.c -------------------------------------------------------------

    fn xopenlog(&mut self) {
        if !self.log_open {
            sys::openlog(Some(sys::leak_cstring(&self.program_name)), libc::LOG_PID, libc::LOG_CRON);
            self.log_open = true;
        }
    }

    fn xcloselog(&mut self) {
        if self.log_open {
            sys::closelog();
        }
        self.log_open = false;
    }

    /// `make_msg()`: vsnprintf into a 151-byte buffer.
    ///
    /// QUIRK: a message of exactly 150 characters fits, but is still
    /// marked " (truncated)" (the check is `len >= sizeof(msg) - 1`).
    fn make_msg(text: &[u8]) -> Vec<u8> {
        let mut m = text[..text.len().min(MAX_MSG)].to_vec();
        if text.len() >= MAX_MSG {
            let truncated = b" (truncated)";
            m.truncate(MAX_MSG + 1 - truncated.len() - 1);
            m.extend_from_slice(truncated);
        }
        m
    }

    fn slog(&mut self, priority: c_int, text: &[u8]) {
        let msg = Self::make_msg(text);
        self.xopenlog();
        sys::syslog(priority, &msg);
        if !self.in_background {
            if priority == EXPLAIN_LEVEL && !self.quiet {
                err(&bcat!(msg, "\n"));
            } else if priority == COMPLAIN_LEVEL {
                err(&bcat!(self.program_name, ": ", msg, "\n"));
            }
        }
    }

    fn log_e(&mut self, priority: c_int, text: &[u8]) {
        let saved = sys::strerror(sys::cerrno());
        let msg = Self::make_msg(text);
        self.xopenlog();
        sys::syslog(priority, &bcat!(msg, ": ", saved));
        if !self.in_background {
            if priority == EXPLAIN_LEVEL && !self.quiet {
                err(&bcat!(msg, ": ", saved, "\n"));
            } else if priority == COMPLAIN_LEVEL {
                err(&bcat!(self.program_name, ": ", msg, ": ", saved, "\n"));
            }
        }
    }

    fn explain(&mut self, text: &[u8]) {
        self.slog(EXPLAIN_LEVEL, text);
    }

    fn complain(&mut self, text: &[u8]) {
        self.slog(COMPLAIN_LEVEL, text);
        self.complaints += 1;
    }

    #[allow(dead_code)]
    fn complain_e(&mut self, text: &[u8]) {
        self.log_e(COMPLAIN_LEVEL, text);
        self.complaints += 1;
    }

    fn die(&mut self, text: &[u8]) -> ! {
        self.slog(COMPLAIN_LEVEL, text);
        if sys::getpid() == self.primary_pid {
            self.complain(b"Aborted");
        }
        stdio::exit(FAILURE_EXIT)
    }

    fn die_e(&mut self, text: &[u8]) -> ! {
        self.log_e(COMPLAIN_LEVEL, text);
        if sys::getpid() == self.primary_pid {
            self.complain(b"Aborted");
        }
        stdio::exit(FAILURE_EXIT)
    }

    // --- main.c ------------------------------------------------------------

    fn print_version() {
        out(format!(
            "Anacron from project {PACKAGE_STRING}\nCopyright (C) 1998  Itai Tzur <itzur@actcom.co.il>\nCopyright (C) 1999  Sean 'Shaleh' Perry <shaleh@debian.org>\nCopyright (C) 2004  Pascal Hakim <pasc@redellipse.net>\n\nMail comments, suggestions and bug reports to <pasc@redellipse.net>.\n\n"
        )
        .as_bytes());
    }

    fn print_usage(&self) {
        let p = &self.program_name;
        out(b"Usage:\n");
        out(&bcat!(" ", p, " [options] [job] ...\n"));
        out(&bcat!(" ", p, " -T [-t anacrontab-file]\n"));
        out(b"\nOptions:\n");
        out(b" -s         Serialize execution of jobs\n");
        out(b" -f         Force execution of jobs, even before their time\n");
        out(b" -n         Run jobs with no delay, implies -s\n");
        out(b" -d         Don't fork to the background\n");
        out(b" -q         Suppress stderr messages, only applicable with -d\n");
        out(b" -u         Update the timestamps without actually running anything\n");
        out(b" -V         Print version information\n");
        out(b" -h         Print this message\n");
        out(b" -t <file>  Use alternative anacrontab\n");
        out(b" -T         Test an anacrontab\n");
        out(b" -S <dir>   Select a different spool directory\n");
        out(b"\nSee the anacron(8) manpage for more details.\n");
    }

    fn parse_opts(&mut self, args: Vec<Vec<u8>>) {
        let mut g = Getopt::new(args, "sfundqt:TS:Vh");
        g.opterr = false;
        while let Some(opt) = g.next_short() {
            match u8::try_from(opt).unwrap_or(0) {
                b's' => self.serialize = true,
                b'f' => self.force = true,
                b'u' => self.update_only = true,
                b'n' => {
                    self.now = true;
                    self.serialize = true;
                }
                b'd' => self.no_daemon = true,
                b'q' => self.quiet = true,
                b't' => self.anacrontab = g.optarg.clone().unwrap_or_default(),
                b'T' => self.testing_only = true,
                b'S' => self.spooldir = g.optarg.clone().unwrap_or_default(),
                b'V' => {
                    Self::print_version();
                    stdio::exit(libc::EXIT_SUCCESS);
                }
                b'h' => {
                    self.print_usage();
                    stdio::exit(libc::EXIT_SUCCESS);
                }
                _ => {
                    err(&bcat!(self.program_name, ": invalid option: ", [g.optopt as u8], "\n"));
                    err(&bcat!("type: `", self.program_name, " -h' for more information\n"));
                    stdio::exit(FAILURE_EXIT);
                }
            }
        }
        self.job_args = if g.optind >= g.args.len() { vec![b"*".to_vec()] } else { g.args[g.optind..].to_vec() };
    }

    fn xfork(&mut self) -> i32 {
        match sys::fork() {
            Ok(p) => p,
            Err(_) => self.die_e(b"Can't fork"),
        }
    }

    /// `xopen(fd, file, flags)`: open, expecting to get `fd` if it's >= 0.
    fn xopen(&mut self, fd: c_int, file: &[u8], flags: c_int) -> sys::Fd {
        let r = sys::open(file, flags, 0).unwrap_or(-1);
        if fd >= 0 && r != fd {
            self.die_e(&bcat!("Can't open ", file, " on file-descriptor ", fd.to_string()));
        } else if r < 0 {
            self.die_e(&bcat!("Can't open ", file));
        }
        r
    }

    fn xclose(&mut self, fd: c_int) {
        if sys::close(fd) != 0 {
            self.die_e(&bcat!("Can't close file descriptor ", fd.to_string()));
        }
    }

    fn go_background(&mut self) {
        self.xclose(1);
        self.xopen(1, b"/dev/null", libc::O_WRONLY);
        self.xclose(2);
        self.xopen(2, b"/dev/null", libc::O_WRONLY);
        let pid = self.xfork();
        if pid != 0 {
            stdio::exit(libc::EXIT_SUCCESS);
        }
        self.primary_pid = sys::getpid();
        if sys::setsid_checked().is_err() {
            self.die_e(b"setsid() error");
        }
        self.in_background = true;
    }

    fn set_signal_handling(&mut self) {
        GOT_SIGALRM.store(false, Ordering::SeqCst);
        GOT_SIGCHLD.store(false, Ordering::SeqCst);
        GOT_SIGUSR1.store(false, Ordering::SeqCst);
        let ss = sys::sigset_of(&[libc::SIGALRM, libc::SIGCHLD, libc::SIGUSR1]);
        if sys::sigmask_block(&ss).is_err() {
            self.die_e(b"sigprocmask error");
        }
        if sys::sigaction_masked(libc::SIGALRM, handle_sigalrm, &ss, 0).is_err()
            || sys::sigaction_masked(libc::SIGCHLD, handle_sigchld, &ss, libc::SA_NOCLDSTOP).is_err()
            || sys::sigaction_masked(libc::SIGUSR1, handle_sigusr1, &ss, 0).is_err()
        {
            self.die_e(b"sigaction error");
        }
    }

    fn wait_signal(&mut self) {
        if sys::sigsuspend_unblocking(&[libc::SIGALRM, libc::SIGCHLD, libc::SIGUSR1]).is_err() {
            self.die_e(b"sigprocmask error");
        }
    }

    fn wait_children(&mut self) {
        while self.running_jobs > 0 || self.running_mailers > 0 {
            self.wait_signal();
            if GOT_SIGCHLD.load(Ordering::SeqCst) {
                self.tend_children();
            }
            GOT_SIGCHLD.store(false, Ordering::SeqCst);
            if GOT_SIGUSR1.load(Ordering::SeqCst) {
                self.explain(b"Received SIGUSR1");
            }
            GOT_SIGUSR1.store(false, Ordering::SeqCst);
        }
    }

    fn orderly_termination(&mut self) -> ! {
        self.explain(b"Received SIGUSR1");
        GOT_SIGUSR1.store(false, Ordering::SeqCst);
        self.wait_children();
        self.explain(b"Exited");
        stdio::exit(libc::EXIT_SUCCESS)
    }

    /// `xsleep(n)`: alarm(n), servicing SIGCHLD and SIGUSR1 meanwhile.
    fn xsleep(&mut self, n: u32) {
        if n == 0 {
            return;
        }
        sys::alarm(n);
        loop {
            self.wait_signal();
            if GOT_SIGCHLD.load(Ordering::SeqCst) {
                self.tend_children();
            }
            GOT_SIGCHLD.store(false, Ordering::SeqCst);
            if GOT_SIGUSR1.load(Ordering::SeqCst) {
                self.orderly_termination();
            }
            if GOT_SIGALRM.load(Ordering::SeqCst) {
                break;
            }
        }
        GOT_SIGALRM.store(false, Ordering::SeqCst);
    }

    fn wait_jobs(&mut self) {
        while self.running_jobs > 0 {
            self.wait_signal();
            if GOT_SIGCHLD.load(Ordering::SeqCst) {
                self.tend_children();
            }
            GOT_SIGCHLD.store(false, Ordering::SeqCst);
            if GOT_SIGUSR1.load(Ordering::SeqCst) {
                self.orderly_termination();
            }
        }
    }

    fn record_start_time(&mut self) {
        self.start_sec = sys::time_now();
        let tm = sys::localtime(self.start_sec).unwrap_or_else(|| sys::localtime_r(self.start_sec));
        self.year = tm.tm_year + 1900;
        self.month = tm.tm_mon + 1;
        self.day_of_month = tm.tm_mday;
        self.day_now = day_num(self.year, self.month, self.day_of_month);
        if self.day_now == -1 {
            self.die(b"Invalid date (this is really embarrassing)");
        }
        if !self.update_only && !self.testing_only {
            self.explain(
                format!("Anacron started on {:04}-{:02}-{:02}", self.year, self.month, self.day_of_month).as_bytes(),
            );
        }
    }

    fn time_till(&mut self, ji: usize) -> u32 {
        if self.now {
            return 0;
        }
        let tn = sys::time_now();
        let tj = self.start_sec + i64::from(self.jobs[ji].delay) * 60;
        if tj < tn {
            return 0;
        }
        if tj - tn > 3600 * 24 {
            let ident = self.jobs[ji].ident.clone();
            self.explain(&bcat!("System time manipulation detected, job `", ident, "' will run immediately"));
            return 0;
        }
        (tj - tn) as u32
    }

    fn fake_jobs(&mut self) {
        for k in 0..self.job_array.len() {
            let ji = self.job_array[k];
            self.fake_job(ji);
            let ident = self.jobs[ji].ident.clone();
            self.explain(&bcat!(
                "Updated timestamp for job `",
                ident,
                format!("' to {:04}-{:02}-{:02}", self.year, self.month, self.day_of_month)
            ));
        }
    }

    fn explain_intentions(&mut self) {
        for k in 0..self.job_array.len() {
            let j = &self.jobs[self.job_array[k]];
            let msg = if self.now {
                bcat!("Will run job `", j.ident, "'")
            } else {
                bcat!("Will run job `", j.ident, format!("' in {} min.", j.delay))
            };
            self.explain(&msg);
        }
        if self.serialize && !self.job_array.is_empty() {
            self.explain(b"Jobs will be executed sequentially");
        }
    }

    // --- readtab.c ---------------------------------------------------------

    /// `read_tab_line()`: one line, with backslash-newline joins (the
    /// newline becomes a space).  `None` at end of file.
    fn read_tab_line(tab: &mut Reader) -> Option<Vec<u8>> {
        if tab.feof() {
            return None;
        }
        let mut line = Vec::new();
        let mut prev: i32 = 0;
        loop {
            let c = tab.getc();
            if (c == i32::from(b'\n') && prev != i32::from(b'\\')) || c == EOF {
                if prev != 0 {
                    line.push(prev as u8);
                }
                break;
            }
            if (prev != i32::from(b'\\') || c != i32::from(b'\n')) && prev != 0 && prev != i32::from(b'\n') {
                line.push(prev as u8);
            } else if prev == i32::from(b'\n') {
                line.push(b' ');
            }
            prev = c;
        }
        Some(line)
    }

    fn job_arg_num(&mut self, ident: &[u8]) -> i32 {
        for i in 0..self.job_args.len() {
            let r = sys::fnmatch(&self.job_args[i], ident);
            if r == 0 {
                return i as i32;
            }
            if r != sys::FNM_NOMATCH {
                self.die(b"fnmatch() error");
            }
        }
        -1
    }

    fn register_env(&mut self, var: &[u8], value: &[u8]) {
        if var.is_empty() {
            return;
        }
        self.env.push(bcat!(var, "=", value));
    }

    fn new_job(&mut self, ident: &[u8], command: &[u8], period: i32, named_period: i32, delay: i32) {
        let arg_num = self.job_arg_num(ident);
        self.jobs.push(Job {
            period,
            named_period,
            delay: delay.wrapping_add(self.random_number),
            ident: ident.to_vec(),
            command: command.to_vec(),
            tab_line: self.line_num,
            arg_num,
            prev_env_rec: self.env.len().checked_sub(1),
            timestamp_fd: -1,
            input_fd: -1,
            output_fd: -1,
            ..Job::default()
        });
    }

    fn register_job(&mut self, periods: &[u8], delays: &[u8], ident: &[u8], command: &[u8]) {
        let (period, delay) = (conv2int(periods), conv2int(delays));
        if period < 0 || delay < 0 {
            let msg = bcat!(self.anacrontab, format!(": number out of range on line {}, skipping", self.line_num));
            self.complain(&msg);
            return;
        }
        self.new_job(ident, command, period, 0, delay);
    }

    fn register_period_job(&mut self, periods: &[u8], delays: &[u8], ident: &[u8], command: &[u8]) {
        let delay = conv2int(delays);
        if delay < 0 {
            let msg = bcat!(self.anacrontab, format!(": number out of range on line {}, skipping", self.line_num));
            self.complain(&msg);
            return;
        }
        let named = if periods.starts_with(b"@monthly") {
            1
        } else if periods.starts_with(b"@yearly")
            || periods.starts_with(b"@annually")
            || periods.starts_with(b"@annualy")
        {
            2
        } else if periods.starts_with(b"@daily") {
            3
        } else if periods.starts_with(b"@weekly") {
            4
        } else {
            // QUIRK: the job is registered anyway.  C leaves its named
            // period uninitialized; here it is 0 (no named period), so with
            // period 0 the job is due every run.
            let msg = bcat!(self.anacrontab, format!(": Unknown named period on line {}, skipping", self.line_num));
            self.complain(&msg);
            0
        };
        self.new_job(ident, command, 0, named, delay);
    }

    /// `unbiased_rand(max)`.
    fn unbiased_rand(max: i64) -> i64 {
        const RAND_MAX: i64 = 2_147_483_647;
        let divisor = RAND_MAX / (max + 1);
        if divisor == 0 {
            // QUIRK: RANDOM_DELAY above RAND_MAX/2 divides by zero (SIGFPE).
            sys::raise_sigfpe();
        }
        loop {
            let rn = cronie::random::random() / divisor;
            if rn <= max {
                return rn;
            }
        }
    }

    fn parse_tab_line(&mut self, line: &[u8]) {
        let rx = |re: &CStr, s: &[u8], n: usize| sys::match_rx(re, s, n);
        macro_rules! try_rx {
            ($r:expr) => {
                match $r {
                    Err(_) => {
                        let msg = bcat!("Regex error reading ", self.anacrontab);
                        self.die(&msg);
                    }
                    Ok(v) => v,
                }
            };
        }
        if try_rx!(rx(c"^[ \t]*($|#)", line, 0)).is_some() {
            return;
        }
        if let Some(subs) = try_rx!(rx(c"^[ \t]*([^ \t=]+)[ \t]*=(.*)$", line, 2)) {
            let (var, value) = (&subs[0], &subs[1]);
            if var.starts_with(b"START_HOURS_RANGE") {
                match try_rx!(rx(c"^([[:digit:]]+)-([[:digit:]]+)$", value, 2)) {
                    None => return self.invalid(),
                    Some(r) => {
                        self.range_start = cronie::num::atoi(&r[0]);
                        self.range_stop = cronie::num::atoi(&r[1]);
                        if self.range_stop < self.range_start {
                            self.range_start = 0;
                            self.range_stop = 0;
                            return self.invalid();
                        }
                    }
                }
            } else if var.starts_with(b"RANDOM_DELAY") {
                if try_rx!(rx(c"^([[:digit:]]+)$", value, 0)).is_none() {
                    return self.invalid();
                }
                self.random_number = Self::unbiased_rand(i64::from(cronie::num::atoi(value))) as i32;
            } else if var.starts_with(b"PREFERRED_HOUR")
                && let Some(r) = try_rx!(rx(c"^([[:digit:]]+)$", value, 1))
            {
                self.preferred_hour = cronie::num::atoi(&r[0]);
                if self.preferred_hour < 0 || self.preferred_hour > 24 {
                    self.preferred_hour = -1;
                    return self.invalid();
                }
            }
            let (var, value) = (var.clone(), value.clone());
            self.register_env(&var, &value);
            return;
        }
        if let Some(s) =
            try_rx!(rx(c"^[ \t]*([[:digit:]]+)[ \t]+([[:digit:]]+)[ \t]+([^ \t/]+)[ \t]+([^ \t].*)$", line, 4))
        {
            self.register_job(&s[0], &s[1], &s[2], &s[3]);
            return;
        }
        if let Some(s) = try_rx!(rx(c"^[ \t]*(@[^ \t]+)[ \t]+([[:digit:]]+)[ \t]+([^ \t/]+)[ \t]+([^ \t].*)$", line, 4))
        {
            self.register_period_job(&s[0], &s[1], &s[2], &s[3]);
            return;
        }
        self.invalid();
    }

    fn invalid(&mut self) {
        let msg =
            bcat!("Invalid syntax in ", self.anacrontab, format!(" on line {} - skipping this line", self.line_num));
        self.complain(&msg);
    }

    fn read_tab(&mut self, cwd: sys::Fd) {
        self.line_num = 0;
        if sys::fchdir(cwd).is_err() {
            self.die_e(b"Can't chdir to original cwd");
        }
        let tab = sys::open(&self.anacrontab, libc::O_RDONLY, 0);
        let open_errno = sys::cerrno();
        if sys::chdir(Some(&self.spooldir.clone())).is_err() {
            let msg = bcat!("Can't chdir to ", self.spooldir);
            self.die_e(&msg);
        }
        let Ok(fd) = tab else {
            sys::set_cerrno(open_errno);
            let msg = bcat!("Error opening ", self.anacrontab);
            self.die_e(&msg);
        };
        let mut r = Reader::from_fd(fd);
        while let Some(line) = Self::read_tab_line(&mut r) {
            self.line_num += 1;
            let line = cronie::bytes::c_str_prefix(&line).to_vec();
            self.parse_tab_line(&line);
        }
        if sys::close(fd) != 0 {
            let msg = bcat!("Error closing ", self.anacrontab);
            self.die_e(&msg);
        }
    }

    fn arrange_jobs(&mut self) {
        let mut arr = Vec::new();
        for ji in 0..self.jobs.len() {
            if self.jobs[ji].arg_num != -1 && (self.update_only || self.testing_only || self.consider_job(ji)) {
                arr.push(ji);
            }
        }
        let now = self.now;
        let jobs = &self.jobs;
        arr.sort_by(|&a, &b| {
            let (a, b) = (&jobs[a], &jobs[b]);
            let d = a.arg_num - b.arg_num;
            if d != 0 && now {
                return d.cmp(&0);
            }
            (a.delay - b.delay).cmp(&0).then((a.tab_line - b.tab_line).cmp(&0))
        });
        self.job_array = arr;
    }

    // --- lock.c ------------------------------------------------------------

    fn open_tsfile(&mut self, ji: usize) {
        let ident = self.jobs[ji].ident.clone();
        let fd = match sys::open(&ident, libc::O_RDWR | libc::O_CREAT, 0o600) {
            Ok(fd) => fd,
            Err(_) => self.die_e(&bcat!("Can't open timestamp file for job ", ident)),
        };
        self.jobs[ji].timestamp_fd = fd;
        sys::set_cloexec(fd);
        if sys::fchown(fd, sys::getuid(), sys::getgid()).is_err() {
            self.die_e(&bcat!("Can't chown timestamp file ", ident));
        }
        if sys::fchmod(fd, 0o600).is_err() {
            self.die_e(&bcat!("Can't chmod timestamp file ", ident));
        }
    }

    fn consider_job(&mut self, ji: usize) -> bool {
        self.open_tsfile(ji);
        let fd = self.jobs[ji].timestamp_fd;
        let mut ts = [0u8; 9];
        let b = sys::read(fd, &mut ts[..8]);
        if b == -1 {
            let ident = self.jobs[ji].ident.clone();
            self.die_e(&bcat!("Error reading timestamp file ", ident));
        }
        // QUIRK: everything below, START_HOURS_RANGE and PREFERRED_HOUR
        // included, only applies once the timestamp file holds 8 bytes: a
        // job's first run (or one after its timestamp is lost) ignores them.
        if !self.force && b == 8 {
            let dn = match sys::scan_ymd(cronie::bytes::c_str_prefix(&ts[..8])) {
                Some((y, m, d)) => day_num(y, m, d),
                None => 0,
            };
            let day_delta = self.day_now - dn;
            let j = &self.jobs[ji];
            if day_delta >= 0 && day_delta < j.period {
                self.xclose(fd);
                return false;
            }
            if j.named_period != 0 {
                let (period, bypass) = match j.named_period {
                    1 => (days_last_month(), days_this_month()),
                    2 => (days_last_year(), days_this_year()),
                    3 => (1, 1),
                    4 => (7, 7),
                    n => {
                        let msg = bcat!("Unknown named period for ", j.ident, format!(" ({n})"));
                        self.die(&msg);
                    }
                };
                // QUIRK: a leftover debugging printf, on stdout.
                out(format!("Checking against {day_delta} with {period}\n").as_bytes());
                if day_delta < period && day_delta != bypass {
                    self.xclose(fd);
                    return false;
                }
            }
            let j = &self.jobs[ji];
            let jobtime = self.start_sec + i64::from(j.delay) * 60;
            let t = sys::localtime(jobtime).unwrap_or_else(|| sys::localtime_r(jobtime));
            if !self.now && self.preferred_hour != -1 && t.tm_hour != self.preferred_hour {
                self.xclose(fd);
                return false;
            }
            if !self.now
                && self.range_start != -1
                && self.range_stop != -1
                && (t.tm_hour < self.range_start || t.tm_hour >= self.range_stop)
            {
                self.xclose(fd);
                return false;
            }
        }
        match sys::lock_whole_file(fd) {
            Ok(()) => return true,
            Err(e) if e != libc::EACCES && e != libc::EAGAIN => self.die_e(b"fcntl() error"),
            Err(_) => {}
        }
        self.xclose(fd);
        let ident = self.jobs[ji].ident.clone();
        self.explain(&bcat!("Job `", ident, "' locked by another anacron - skipping"));
        false
    }

    fn update_timestamp(&mut self, ji: usize) {
        let stamp = format!("{:04}{:02}{:02}\n", self.year, self.month, self.day_of_month);
        let stamp = &stamp.as_bytes()[..stamp.len().min(9)];
        let fd = self.jobs[ji].timestamp_fd;
        let ident = self.jobs[ji].ident.clone();
        if sys::lseek_set(fd, 0) != Ok(0) {
            self.die_e(&bcat!("Can't lseek timestamp file for job ", ident));
        }
        if sys::write(fd, stamp) != 9 {
            self.die_e(&bcat!("Can't write timestamp file for job ", ident));
        }
        if sys::ftruncate(fd, 9).is_err() {
            self.die_e(b"ftruncate error");
        }
    }

    fn fake_job(&mut self, ji: usize) {
        self.open_tsfile(ji);
        self.update_timestamp(ji);
        let fd = self.jobs[ji].timestamp_fd;
        self.xclose(fd);
    }

    // --- runjob.c ----------------------------------------------------------

    fn temp_file(&mut self, ji: usize) {
        let dir = sys::getenv(b"TMPDIR").filter(|d| !d.is_empty()).unwrap_or_else(|| b"/tmp".to_vec());
        let mut template = bcat!(dir, "/$anacronXXXXXX");
        if template.len() > PATH_MAX {
            self.die_e(b"TMPDIR too long");
        }
        let fdout = match sys::mkstemp(&mut template) {
            Ok(fd) => fd,
            Err(_) => self.die_e(b"Can't open temporary file for writing"),
        };
        let fdin = match sys::open(&template, libc::O_RDONLY, 0o600) {
            Ok(fd) => fd,
            Err(_) => self.die_e(b"Can't open temporary file for reading"),
        };
        if sys::unlink(&template).is_err() {
            self.die_e(b"Can't unlink temporary file");
        }
        sys::set_cloexec(fdout);
        sys::set_cloexec(fdin);
        self.jobs[ji].input_fd = fdin;
        self.jobs[ji].output_fd = fdout;
    }

    fn file_size(&mut self, fd: sys::Fd) -> i64 {
        match sys::fstat(fd) {
            Ok(st) => st.size,
            Err(_) => self.die_e(b"Can't fstat temporary file"),
        }
    }

    fn username(&mut self) -> Vec<u8> {
        if let Some(u) = &self.username {
            return u.clone();
        }
        let Some(pw) = sys::getpwuid(sys::geteuid()) else {
            self.die_e(b"getpwuid() error");
        };
        self.username = Some(pw.name.clone());
        pw.name
    }

    /// `xputenv()`: setenv in anacron's own environment.
    fn xputenv(&mut self, s: &[u8]) {
        let Some(eq) = s.iter().position(|&b| b == b'=') else {
            self.die_e(b"Invalid environment string");
        };
        if sys::setenv(&s[..eq], &s[eq + 1..]).is_err() {
            self.die_e(b"Can't set the environment");
        }
    }

    /// `setup_env()`: apply the anacrontab's assignments up to the job's.
    ///
    /// QUIRK: they are set in anacron's own environment and never unset,
    /// so a job also sees assignments made for jobs that started before it,
    /// including ones that appear after it in the anacrontab.
    fn setup_env(&mut self, ji: usize) {
        let Some(last) = self.jobs[ji].prev_env_rec else { return };
        if self.env.is_empty() {
            return;
        }
        for i in 0..=last {
            let v = self.env[i].clone();
            self.xputenv(&v);
        }
    }

    fn run_job(&mut self, ji: usize) -> ! {
        let j = self.jobs[ji].clone();
        if !j.no_mail_output {
            self.xclose(1);
            self.xclose(2);
            sys::dup2(j.output_fd, 1);
            sys::dup2(j.output_fd, 2);
            self.in_background = false;
        }
        if sys::chdir(Some(b"/")).is_err() {
            self.die_e(b"Can't chdir to '/'");
        }
        if let Some(m) = self.old_sigmask
            && sys::sigmask_set(&m).is_err()
        {
            self.die_e(b"sigprocmask error");
        }
        self.xcloselog();
        sys::execl(b"/bin/sh", &[b"/bin/sh", b"-c", &j.command]);
        self.die_e(b"execl() error")
    }

    fn launch_mailer(&mut self, ji: usize) {
        let Some(mailto) = self.jobs[ji].mailto.clone() else {
            self.explain(b"Empty MAILTO set, not mailing output");
            return;
        };
        if sys::stat(SENDMAIL).is_err() {
            self.complain(&bcat!("Can't find sendmail at ", SENDMAIL, ", not mailing output"));
            return;
        }
        let pid = self.xfork();
        if pid == 0 {
            self.in_background = true;
            self.xclose(0);
            sys::dup2(self.jobs[ji].input_fd, 0);
            if sys::lseek_set(0, 0) != Ok(0) {
                self.die_e(b"Can't lseek()");
            }
            if let Some(m) = self.old_sigmask
                && sys::sigmask_set(&m).is_err()
            {
                self.die_e(b"sigprocmask error");
            }
            self.xcloselog();
            self.xclose(1);
            self.xopen(1, b"/dev/null", libc::O_WRONLY);
            self.xclose(2);
            self.xopen(2, b"/dev/null", libc::O_WRONLY);
            let ofd = self.jobs[ji].output_fd;
            self.xclose(ofd);
            sys::execl(SENDMAIL, &[SENDMAIL, b"-FAnacron", b"-odi", &mailto]);
            self.die_e(b"Can't exec /usr/sbin/sendmail");
        }
        self.jobs[ji].mailer_pid = pid;
        self.running_mailers += 1;
    }

    fn tend_mailer(&mut self, ji: usize, status: c_int) {
        let ident = self.jobs[ji].ident.clone();
        let pre = bcat!("Tried to mail output of job `", ident, "', but mailer process (", SENDMAIL, ")");
        if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) != 0 {
            self.complain(&bcat!(pre, format!(" exited with status {}", libc::WEXITSTATUS(status))));
        } else if !libc::WIFEXITED(status) && libc::WIFSIGNALED(status) {
            self.complain(&bcat!(pre, format!(" got signal {}", libc::WTERMSIG(status))));
        } else if !libc::WIFEXITED(status) && !libc::WIFSIGNALED(status) {
            self.complain(&bcat!(pre, " terminated abnormally"));
        }
        self.jobs[ji].mailer_pid = 0;
        self.running_mailers -= 1;
    }

    fn launch_job(&mut self, ji: usize) {
        let hostname = sys::gethostname_512().unwrap_or_else(|| b"unknown machine".to_vec());
        self.setup_env(ji);
        let no_mail = sys::getenv(b"NO_MAIL_OUTPUT").is_some_and(|v| !v.is_empty());
        self.jobs[ji].no_mail_output = no_mail;
        if !no_mail {
            let mailto = match sys::getenv(b"MAILTO") {
                None => self.username(),
                Some(m) => match expand_envvar(&m, MAX_EMAILSTR) {
                    Some(x) => x,
                    None => {
                        self.complain(b"The environment variable 'MAILTO' could not be expanded. The non-expanded value will be used.");
                        m
                    }
                },
            };
            let mailfrom = match sys::getenv(b"MAILFROM") {
                None => self.username(),
                Some(m) => match expand_envvar(&m, MAX_EMAILSTR) {
                    Some(x) => x,
                    None => {
                        self.complain(b"The environment variable 'MAILFROM' could not be expanded. The non-expanded value will be used.");
                        m
                    }
                },
            };
            self.temp_file(ji);
            let fd = self.jobs[ji].output_fd;
            let codeset = sys::codeset().unwrap_or_default();
            let ident = self.jobs[ji].ident.clone();
            for part in [
                &b"From: "[..],
                b"Anacron <",
                &mailfrom,
                b">\n",
                b"To: ",
                &mailto,
                b"\n",
                b"MIME-Version: 1.0\n",
                b"Content-Type: text/plain; charset=\"",
                &codeset,
                b"\"\n",
                b"Content-Transfer-Encoding: 8bit\n",
                b"Subject: Anacron job '",
                &ident,
                b"' on ",
                &hostname,
                b"\n\n",
            ] {
                if sys::write(fd, part) == -1 {
                    self.die_e(b"Can't write to temporary file");
                }
            }
            self.jobs[ji].mailto = if mailto.is_empty() { None } else { Some(mailto) };
            self.jobs[ji].mail_header_size = self.file_size(fd);
        }
        let pid = self.xfork();
        if pid == 0 {
            self.in_background = true;
            self.run_job(ji);
        }
        let ident = self.jobs[ji].ident.clone();
        self.explain(&bcat!("Job `", ident, "' started"));
        self.jobs[ji].job_pid = pid;
        self.running_jobs += 1;
    }

    fn tend_job(&mut self, ji: usize, status: c_int) {
        self.update_timestamp(ji);
        let tsfd = self.jobs[ji].timestamp_fd;
        self.xclose(tsfd);
        let j = self.jobs[ji].clone();
        let mail_output = !j.no_mail_output && self.file_size(j.output_fd) > j.mail_header_size;
        let m: &[u8] = if mail_output { b" (produced output)" } else { b"" };
        if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
            self.explain(&bcat!("Job `", j.ident, "' terminated", m));
        } else if libc::WIFEXITED(status) {
            self.explain(&bcat!(
                "Job `",
                j.ident,
                format!("' terminated (exit status: {})", libc::WEXITSTATUS(status)),
                m
            ));
        } else if libc::WIFSIGNALED(status) {
            self.complain(&bcat!(
                "Job `",
                j.ident,
                format!("' terminated due to signal {}", libc::WTERMSIG(status)),
                m
            ));
        } else {
            self.complain(&bcat!("Job `", j.ident, "' terminated abnormally", m));
        }
        self.jobs[ji].job_pid = 0;
        self.running_jobs -= 1;
        if mail_output {
            self.launch_mailer(ji);
        }
        if !j.no_mail_output {
            self.xclose(j.output_fd);
            self.xclose(j.input_fd);
        }
    }

    /// `xwait()`: has the child finished?  Its status if so.
    fn xwait(&mut self, pid: i32) -> Option<c_int> {
        match sys::waitpid_pid_nohang(pid) {
            (-1, _) => self.die_e(b"waitpid() error"),
            (0, _) => None,
            (_, st) => Some(st),
        }
    }

    fn tend_children(&mut self) {
        for k in 0..self.job_array.len() {
            let ji = self.job_array[k];
            if self.jobs[ji].mailer_pid != 0
                && let Some(st) = self.xwait(self.jobs[ji].mailer_pid)
            {
                self.tend_mailer(ji, st);
            }
            if self.jobs[ji].job_pid != 0
                && let Some(st) = self.xwait(self.jobs[ji].job_pid)
            {
                self.tend_job(ji, st);
            }
        }
    }
}

/// `conv2int()`: the number, or -1 if it doesn't fit an int.
fn conv2int(s: &[u8]) -> i32 {
    let p = cronie::num::strtoimax(s, 10);
    if p.erange || p.value < 0 || p.value > i64::from(i32::MAX) { -1 } else { p.value as i32 }
}

// --- gregor.c ----------------------------------------------------------------

const DAYS_IN_MONTH: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// `day_num()`: days since 31 December 1 BC (proleptic Gregorian), or -1.
fn day_num(year: i32, month: i32, day: i32) -> i32 {
    if !(1..=i32::MAX / 366).contains(&year) || !(1..=12).contains(&month) || day < 1 {
        return -1;
    }
    let isleap = leap(year);
    if month != 2 {
        if day > DAYS_IN_MONTH[(month - 1) as usize] {
            return -1;
        }
    } else if (isleap && day > 29) || (!isleap && day > 28) {
        return -1;
    }
    let i = year - 1;
    let mut dn = i * 365 + (i / 4 - i / 100 + i / 400);
    for m in (1..month).rev() {
        dn += DAYS_IN_MONTH[(m - 1) as usize];
    }
    if month > 2 && isleap {
        dn += 1;
    }
    dn + day
}

fn now_tm() -> sys::Tm {
    sys::localtime_r(sys::time_now())
}

fn days_last_month() -> i32 {
    let t = now_tm();
    match t.tm_mon {
        0 => DAYS_IN_MONTH[11],
        2 => DAYS_IN_MONTH[1] + i32::from(leap(t.tm_year + 1900)),
        m => DAYS_IN_MONTH[(m - 1) as usize],
    }
}

fn days_this_month() -> i32 {
    let t = now_tm();
    match t.tm_mon {
        1 => DAYS_IN_MONTH[1] + i32::from(leap(t.tm_year + 1900)),
        m => DAYS_IN_MONTH[m as usize],
    }
}

fn days_last_year() -> i32 {
    if leap(now_tm().tm_year - 1 + 1900) { 366 } else { 365 }
}

fn days_this_year() -> i32 {
    if leap(now_tm().tm_year + 1900) { 366 } else { 365 }
}

fn main() {
    let args: Vec<Vec<u8>> = std::env::args_os().map(std::os::unix::ffi::OsStringExt::into_vec).collect();
    let mut a = Anacron { range_start: -1, range_stop: -1, preferred_hour: -1, ..Anacron::default() };
    sys::setlocale_all();
    let usec = sys::gettimeofday_usec();
    cronie::random::srandom((i64::from(sys::getpid()) + usec) as u32);
    let argv0 = args.first().cloned().unwrap_or_default();
    a.program_name = match argv0.iter().rposition(|&b| b == b'/') {
        Some(i) => argv0[i + 1..].to_vec(),
        None => argv0,
    };
    a.parse_opts(args);
    if a.anacrontab.is_empty() {
        a.anacrontab = ANACRONTAB.to_vec();
    }
    if a.spooldir.is_empty() {
        a.spooldir = ANACRON_SPOOL_DIR.to_vec();
    }
    let cwd = match sys::open(b"./", libc::O_RDONLY, 0) {
        Ok(fd) => fd,
        Err(_) => a.die_e(b"Can't save current directory"),
    };
    a.in_background = false;
    if sys::chdir(Some(&a.spooldir.clone())).is_err() {
        let msg = bcat!("Can't chdir to ", a.spooldir);
        a.die_e(&msg);
    }
    match sys::sigmask_current() {
        Ok(m) => a.old_sigmask = Some(m),
        Err(_) => a.die_e(b"sigset error"),
    }
    a.xclose(0);
    a.xopen(0, b"/dev/null", libc::O_RDONLY);
    if !a.no_daemon && !a.testing_only {
        a.go_background();
    } else {
        a.primary_pid = sys::getpid();
    }
    a.record_start_time();
    a.read_tab(cwd);
    sys::close(cwd);
    a.arrange_jobs();
    if a.testing_only {
        stdio::exit(if a.complaints != 0 { libc::EXIT_FAILURE } else { libc::EXIT_SUCCESS });
    }
    if a.update_only {
        a.fake_jobs();
        stdio::exit(libc::EXIT_SUCCESS);
    }
    a.explain_intentions();
    a.set_signal_handling();
    a.running_jobs = 0;
    a.running_mailers = 0;
    for k in 0..a.job_array.len() {
        let ji = a.job_array[k];
        let n = a.time_till(ji);
        a.xsleep(n);
        if a.serialize {
            a.wait_jobs();
        }
        a.launch_job(ji);
    }
    a.wait_children();
    let n = a.job_array.len();
    a.explain(format!("Normal exit ({n} job{} run)", if n == 1 { "" } else { "s" }).as_bytes());
    stdio::exit(libc::EXIT_SUCCESS)
}
