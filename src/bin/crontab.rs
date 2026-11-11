//! `crontab`: install, list, edit and remove users' crontabs.  Port of
//! src/crontab.c.  Installed setgid `crontab` (mode 2751), so it runs with
//! the invoking user's uid and the crontab group's egid.
//!
//! Upstream license: ISC (see LICENSE).

use std::ffi::c_int;
use std::sync::atomic::{AtomicPtr, Ordering};

use cronie::entry::load_entry;
use cronie::env::{Env, LoadEnv, glue_strings, load_env};
use cronie::getopt::Getopt;
use cronie::globals::{self, DMISC, log_it};
use cronie::reader::{EOF, Reader, get_char, get_string, skip_comments};
use cronie::security::{self, CRON_ALLOW, CRON_DENY};
use cronie::stdio::{self, CFile, err, out};
use cronie::sys::{self, Passwd};
use cronie::user::{CRON_HOSTNAME, MAX_USER_ENTRIES, MAX_USER_ENVS, SPOOL_DIR};
use cronie::{PACKAGE_STRING, bcat};

const MAX_UNAME: usize = 256;
const MAX_FNAME: usize = 4096;
const MAX_TEMPSTR: usize = 131_072;
const MAXHOSTNAMELEN: usize = 64;
const EDITOR: &[u8] = b"/usr/bin/vi";
const PATH_BSHELL: &[u8] = b"/bin/sh";
const ERROR_EXIT: i32 = 1;
const COMMENT_COLOR: &[u8] = b"\x1B[34;1m";
const ERROR_COLOR: &[u8] = b"\x1B[31;1m";
const RESET_COLOR: &[u8] = b"\x1B[0m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opt {
    Unknown,
    List,
    Delete,
    Edit,
    Replace,
    HostSet,
    HostGet,
    Test,
}

impl Opt {
    fn name(self) -> &'static str {
        match self {
            Opt::Unknown => "???",
            Opt::List => "list",
            Opt::Delete => "delete",
            Opt::Edit => "edit",
            Opt::Replace => "replace",
            Opt::HostSet => "hostset",
            Opt::HostGet => "hostget",
            Opt::Test => "test",
        }
    }
}

/// crontab.c's globals.
#[derive(Debug)]
struct Crontab {
    program: Vec<u8>,
    pid: i32,
    user: Vec<u8>,
    real_user: Vec<u8>,
    filename: Vec<u8>,
    host: Vec<u8>,
    new_crontab: Option<Reader>,
    check_error_count: i32,
    prompt_on_delete: bool,
    host_specified: bool,
    option: Opt,
    pw: Passwd,
    stdin: Reader,
}

/// `TempFilename`, for the `die()` signal handler (a leaked C string).
static TEMP_FILENAME: AtomicPtr<libc::c_char> = AtomicPtr::new(std::ptr::null_mut());

fn set_temp(name: Option<&[u8]>) {
    let new = name.map_or(std::ptr::null_mut(), |n| cronie::bytes::to_cstring(n).into_raw());
    let old = TEMP_FILENAME.swap(new, Ordering::SeqCst);
    if !old.is_null() {
        // SAFETY: old came from CString::into_raw above and is freed once.
        drop(unsafe { std::ffi::CString::from_raw(old) });
    }
}

fn temp() -> Option<Vec<u8>> {
    let p = TEMP_FILENAME.load(Ordering::SeqCst);
    if p.is_null() {
        return None;
    }
    // SAFETY: p is a live C string owned by TEMP_FILENAME.
    Some(unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes().to_vec())
}

/// `die()`: remove the temporary file and `_exit(ERROR_EXIT)`.
extern "C" fn die(_sig: c_int) {
    let p = TEMP_FILENAME.load(Ordering::SeqCst);
    if !p.is_null() {
        // SAFETY: p is a live C string; unlink is async-signal-safe.
        sys::unlink_c(unsafe { std::ffi::CStr::from_ptr(p) });
    }
    sys::exit_now(ERROR_EXIT)
}

fn exit(code: i32) -> ! {
    stdio::exit(code)
}

/// `swap_uids()` (the setregid/setreuid variant: swap real and effective).
fn swap_uids() -> Result<(), i32> {
    sys::setregid(sys::getegid(), sys::getgid())?;
    sys::setreuid(sys::geteuid(), sys::getuid())
}

fn swap_uids_back() -> Result<(), i32> {
    swap_uids()
}

fn open_read(path: &[u8]) -> Result<Reader, i32> {
    sys::open(path, libc::O_RDONLY, 0).map(Reader::from_fd)
}

impl Crontab {
    fn usage(&self, msg: &str) -> ! {
        let p = &self.program;
        err(&bcat!(p, ": usage error: ", msg, "\n"));
        err(b"Usage:\n");
        err(&bcat!(" ", p, " [options] file\n"));
        err(&bcat!(" ", p, " [options]\n"));
        err(&bcat!(" ", p, " -n [hostname]\n"));
        err(b"\n");
        err(b"Options:\n");
        err(b" -u <user>  define user\n");
        err(b" -e         edit user's crontab\n");
        err(b" -l         list user's crontab\n");
        err(b" -r         delete user's crontab\n");
        err(b" -i         prompt before deleting\n");
        err(b" -n <host>  set host in cluster to run users' crontabs\n");
        err(b" -c         get host in cluster to run users' crontabs\n");
        err(b" -T <file>  test a crontab file syntax\n");
        err(b" -V         print version and exit\n");
        err(b" -x <mask>  enable debugging\n");
        err(b"\nDefault operation is replace, per 1003.2\n");
        exit(ERROR_EXIT)
    }

    /// `parse_args()`.
    fn parse_args(&mut self, args: Vec<Vec<u8>>) {
        let Some(pw) = sys::getpwuid(sys::getuid()) else {
            err(&bcat!(self.program, ": your UID isn't in the passwd file.\n"));
            err(b"bailing out.\n");
            exit(ERROR_EXIT);
        };
        if pw.name.len() >= MAX_UNAME {
            err(b"username too long\n");
            exit(ERROR_EXIT);
        }
        self.user = pw.name.clone();
        self.real_user = pw.name.clone();
        self.pw = pw;
        let mut g = Getopt::new(args, "u:lerincx:VT");
        while let Some(c) = g.next_short() {
            let optarg = g.optarg.clone().unwrap_or_default();
            match u8::try_from(c).unwrap_or(0) {
                b'x' => {
                    if !globals::set_debug_flags(&optarg) {
                        self.usage("bad debug option");
                    }
                }
                b'u' => {
                    if sys::getuid() != 0 {
                        err(b"must be privileged to use -u\n");
                        exit(ERROR_EXIT);
                    }
                    if matches!(self.option, Opt::HostSet | Opt::HostGet | Opt::Test) {
                        err(b"cannot use -u with -n, -c or -T\n");
                        exit(ERROR_EXIT);
                    }
                    let Some(pw) = sys::getpwnam(&optarg) else {
                        err(&bcat!(self.program, ":  user `", optarg, "' unknown\n"));
                        exit(ERROR_EXIT);
                    };
                    self.pw = pw;
                    if optarg.len() >= MAX_UNAME {
                        self.usage("username too long");
                    }
                    self.user = optarg;
                }
                c @ (b'l' | b'r' | b'e' | b'T') => {
                    if self.option != Opt::Unknown {
                        self.usage("only one operation permitted");
                    }
                    self.option = match c {
                        b'l' => Opt::List,
                        b'r' => Opt::Delete,
                        b'e' => Opt::Edit,
                        _ => Opt::Test,
                    };
                }
                b'i' => self.prompt_on_delete = true,
                b'n' => {
                    if sys::getuid() != 0 {
                        err(b"must be privileged to set host with -n\n");
                        exit(ERROR_EXIT);
                    }
                    if self.option != Opt::Unknown {
                        self.usage("only one operation permitted");
                    }
                    if self.user != self.real_user {
                        err(b"cannot use -u with -n or -c\n");
                        exit(ERROR_EXIT);
                    }
                    self.option = Opt::HostSet;
                }
                b'c' => {
                    if self.option != Opt::Unknown {
                        self.usage("only one operation permitted");
                    }
                    if self.user != self.real_user {
                        err(b"cannot use -u with -n or -c\n");
                        exit(ERROR_EXIT);
                    }
                    self.option = Opt::HostGet;
                }
                b'V' => {
                    out(format!("{PACKAGE_STRING}\n").as_bytes());
                    exit(libc::EXIT_SUCCESS);
                }
                _ => self.usage("unrecognized option"),
            }
        }
        sys::endpwent();

        let mut optind = g.optind;
        let arg = |i: usize| g.args.get(i).cloned();
        if self.option == Opt::HostSet
            && let Some(h) = arg(optind)
        {
            self.host_specified = true;
            if h.len() >= MAXHOSTNAMELEN {
                self.usage("hostname too long");
            }
            self.host = h;
            optind += 1;
        }
        if self.option == Opt::Unknown {
            // replace is the default option
            self.option = Opt::Replace;
        }
        if matches!(self.option, Opt::Replace | Opt::Test) {
            if let Some(f) = arg(optind) {
                if f.len() >= MAX_FNAME {
                    self.usage("filename too long");
                }
                self.filename = f;
                optind += 1;
            } else if sys::isatty(0) {
                self.usage("file name or - (for stdin) must be specified");
            } else {
                self.filename = b"-".to_vec();
            }
        }
        if arg(optind).is_some() {
            self.usage("no arguments permitted after this option");
        }

        if !self.filename.is_empty() {
            if self.filename == b"-" {
                self.new_crontab = Some(Reader::from_fd(0));
            } else {
                // Open as the real user, so nobody reads files they can't.
                if swap_uids().is_err() {
                    stdio::perror(b"swapping uids");
                    exit(ERROR_EXIT);
                }
                let f = match open_read(&self.filename) {
                    Ok(f) => f,
                    Err(_) => {
                        stdio::perror(&self.filename);
                        exit(ERROR_EXIT);
                    }
                };
                match sys::fstat(f.fd().expect("fd")) {
                    Err(_) => {
                        stdio::perror(&self.filename);
                        exit(ERROR_EXIT);
                    }
                    Ok(st) if sys::is_dir(st.mode) => {
                        err(&bcat!("invalid crontab file: '", self.filename, "' is a directory\n"));
                        sys::close(f.fd().expect("fd"));
                        exit(ERROR_EXIT);
                    }
                    Ok(_) => {}
                }
                if swap_uids_back().is_err() {
                    stdio::perror(b"swapping uids back");
                    exit(ERROR_EXIT);
                }
                self.new_crontab = Some(f);
            }
        }
        globals::debug(DMISC, || {
            bcat!("user=", self.user, ", file=", self.filename, ", option=", self.option.name(), "\n")
        });
    }

    fn spool_path(&self, name: &[u8]) -> Vec<u8> {
        match glue_strings(MAX_FNAME, SPOOL_DIR, name, b'/') {
            Some(p) => p,
            None => {
                err(b"path too long\n");
                exit(ERROR_EXIT);
            }
        }
    }

    /// `list_cmd()`.
    fn list_cmd(&mut self) {
        let colorize = sys::isatty(1) && sys::getenv(b"NO_COLOR").is_none();
        log_it(&self.real_user, self.pid, b"LIST", &self.user, 0);
        let n = self.spool_path(&self.user.clone());
        let mut f = match open_read(&n) {
            Ok(f) => f,
            Err(e) => {
                if e == libc::ENOENT {
                    err(&bcat!("no crontab for ", self.user, "\n"));
                } else {
                    stdio::perror(&n);
                }
                exit(ERROR_EXIT);
            }
        };
        globals::set_line_number(1);
        let mut new_line = true;
        let mut in_comment = false;
        loop {
            let ch = get_char(&mut f);
            if ch == EOF {
                break;
            }
            if colorize {
                if !in_comment && new_line && ch == i32::from(b'#') {
                    in_comment = true;
                    out(COMMENT_COLOR);
                }
                if in_comment && ch == i32::from(b'\n') {
                    in_comment = false;
                    out(RESET_COLOR);
                }
            }
            out(&[ch as u8]);
            new_line = ch == i32::from(b'\n');
        }
        if colorize && !new_line {
            out(b"\n");
            out(&bcat!(ERROR_COLOR, "No end-of-line character at the end of file", RESET_COLOR));
            out(b"\n");
        }
        if let Some(fd) = f.fd() {
            sys::close(fd);
        }
    }

    /// `delete_cmd()`.
    fn delete_cmd(&mut self) {
        if self.prompt_on_delete {
            out(&bcat!("crontab: really delete ", self.user, "'s crontab? "));
            stdio::flush_stdout();
            match self.stdin.fgets(MAX_FNAME - 1) {
                Some(l) if l.first() == Some(&b'Y') || l.first() == Some(&b'y') => {}
                _ => exit(0),
            }
        }
        log_it(&self.real_user, self.pid, b"DELETE", &self.user, 0);
        let n = self.spool_path(&self.user.clone());
        // QUIRK: only "no crontab" (-1) stops the delete; a failed backup
        // (-2) doesn't.
        if self.backup_crontab(&n) == -1 {
            err(&bcat!("no crontab for ", self.user, "\n"));
            exit(ERROR_EXIT);
        }
        if sys::unlink(&n).is_err() {
            stdio::perror(&n);
            exit(ERROR_EXIT);
        }
        self.poke_daemon();
    }

    /// `backup_crontab()`: 0 on success, -1 if there's no crontab, -2 if
    /// the backup couldn't be written.
    fn backup_crontab(&mut self, crontab_path: &[u8]) -> i32 {
        let backup_dir = if let Some(v) = sys::getenv(b"XDG_CACHE_HOME") {
            // glue_strings(dir, v, "", '\0'): v itself.
            match glue_strings(MAX_FNAME, &v, b"", 0) {
                Some(d) => cronie::bytes::c_str_prefix(&d).to_vec(),
                None => {
                    err(b"$XDG_CACHE_HOME path too long\n");
                    return -2;
                }
            }
        } else if let Some(v) = sys::getenv(b"HOME") {
            match glue_strings(MAX_FNAME, &v, b".cache", b'/') {
                Some(d) => d,
                None => {
                    err(b"$HOME path too long\n");
                    return -2;
                }
            }
        } else {
            err(b"Could not find environment variable XDG_CACHE_HOME or HOME to save the backup\n");
            return -2;
        };
        let Some(backup_dir) = glue_strings(MAX_FNAME, &backup_dir, b"crontab", b'/') else {
            err(b"backup path too long\n");
            return -2;
        };
        let Some(mut backup_path) = glue_strings(MAX_FNAME, &backup_dir, b"crontab", b'/') else {
            err(b"backup path too long\n");
            return -2;
        };
        if sys::getuid() != self.pw.uid {
            // -u was used
            match glue_strings(MAX_FNAME, &backup_path, &self.user, b'.') {
                Some(p) => backup_path = p,
                None => {
                    err(b"backup path too long\n");
                    return -2;
                }
            }
        }
        let Some(backup_path) = glue_strings(MAX_FNAME, &backup_path, b"bak", b'.') else {
            err(b"backup path too long\n");
            return -2;
        };

        let mut crontab_file = match open_read(crontab_path) {
            Ok(f) => f,
            Err(e) => {
                if e != libc::ENOENT {
                    stdio::perror(crontab_path);
                    exit(ERROR_EXIT);
                }
                return -1;
            }
        };
        if swap_uids().is_err() {
            stdio::perror(b"swapping uids");
            exit(ERROR_EXIT);
        }
        let mut retval = 0;
        let mut backup_file = None;
        // QUIRK: if mkdir fails, C restores an umask it never saved (an
        // uninitialized variable); here the umask is left as it was.
        let mut old_umask = None;
        if let Err(e) = sys::stat(&backup_dir)
            && e == libc::ENOENT
            && sys::mkdir(&backup_dir, 0o755).is_err()
        {
            err(&bcat!(backup_dir, ": "));
            stdio::perror(b"mkdir");
            retval = -2;
        }
        if retval == 0 {
            old_umask = Some(sys::umask(0o077));
            match sys::open(&backup_path, libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC, 0o666) {
                Ok(fd) => backup_file = Some(CFile::from_fd(fd)),
                Err(_) => {
                    err(b"Failed to write to the backup file: ");
                    stdio::perror(&backup_path);
                    retval = -2;
                }
            }
        }
        if let Some(m) = old_umask {
            sys::umask(m);
        }
        if swap_uids_back().is_err() {
            stdio::perror(b"swapping uids back");
            exit(ERROR_EXIT);
        }
        if retval != 0 {
            return retval;
        }
        let mut backup_file = backup_file.expect("opened");
        loop {
            let ch = get_char(&mut crontab_file);
            if ch == EOF {
                break;
            }
            backup_file.write(&[ch as u8]);
        }
        if let Some(fd) = crontab_file.fd() {
            sys::close(fd);
        }
        backup_file.close();
        out(&bcat!("Backup of ", self.user, "'s previous crontab saved to ", backup_path, "\n"));
        0
    }

    /// `tmp_path()`: $TMPDIR, unless running setuid or setgid.
    fn tmp_path() -> Vec<u8> {
        let tmpdir = if sys::getuid() == sys::geteuid() && sys::getgid() == sys::getegid() {
            sys::getenv(b"TMPDIR")
        } else {
            None
        };
        tmpdir.unwrap_or_else(|| b"/tmp".to_vec())
    }

    /// `host_specific_filename(prefix, suffix)`: prefix.HOST.suffix.
    fn host_specific_filename(prefix: &[u8], suffix: &[u8]) -> Option<Vec<u8>> {
        let hostname = sys::gethostname()?;
        let hostname = glue_strings(MAX_FNAME, prefix, &hostname, b'.')?;
        glue_strings(MAX_FNAME, &hostname, suffix, b'.')
    }

    /// `edit_cmd()`.
    fn edit_cmd(&mut self) {
        log_it(&self.real_user, self.pid, b"BEGIN EDIT", &self.user, 0);
        let n = self.spool_path(&self.user.clone());
        let mut f = match open_read(&n) {
            Ok(f) => f,
            Err(e) => {
                if e != libc::ENOENT {
                    stdio::perror(&n);
                    exit(ERROR_EXIT);
                }
                err(&bcat!("no crontab for ", self.user, " - using an empty one\n"));
                match open_read(b"/dev/null") {
                    Ok(f) => f,
                    Err(_) => {
                        stdio::perror(b"/dev/null");
                        exit(ERROR_EXIT);
                    }
                }
            }
        };
        sys::signal_ignore(libc::SIGHUP);
        sys::signal_ignore(libc::SIGINT);
        sys::signal_ignore(libc::SIGQUIT);

        let Some(mut filename) = glue_strings(MAX_FNAME, &Self::tmp_path(), b"crontab.XXXXXX", b'/') else {
            err(b"path too long\n");
            exit(ERROR_EXIT);
        };
        if swap_uids().is_err() {
            stdio::perror(b"swapping uids");
            exit(ERROR_EXIT);
        }
        let fatal = |filename: &[u8]| -> ! {
            let _ = sys::unlink(filename);
            exit(ERROR_EXIT)
        };
        let t = match sys::mkstemp(&mut filename) {
            Ok(t) => t,
            Err(_) => {
                stdio::perror(&filename);
                self.filename = filename.clone();
                fatal(&filename);
            }
        };
        self.filename = filename.clone();
        if swap_uids_back().is_err() {
            stdio::perror(b"swapping uids back");
            fatal(&filename);
        }
        let mut tmp = CFile::from_fd(t);
        globals::set_line_number(1);
        loop {
            let ch = get_char(&mut f);
            if ch == EOF {
                break;
            }
            tmp.write(&[ch as u8]);
        }
        if let Some(fd) = f.fd() {
            sys::close(fd);
        }
        if !tmp.flush() {
            stdio::perror(&filename);
            exit(ERROR_EXIT);
        }
        if swap_uids().is_err() {
            stdio::perror(b"swapping uids");
            exit(ERROR_EXIT);
        }
        // Set it to 1970
        let _ = sys::utime(&filename, Some(0));
        if swap_uids_back().is_err() {
            stdio::perror(b"swapping uids");
            exit(ERROR_EXIT);
        }
        // NewCrontab is the temp file (r+) until the editor is done.
        let mut new_crontab_fd = t;
        loop {
            // again:
            let _ = sys::lseek_set(new_crontab_fd, 0);
            let editor = match (sys::getenv(b"VISUAL"), sys::getenv(b"EDITOR")) {
                (Some(v), _) if !v.is_empty() => v,
                (_, Some(e)) if !e.is_empty() => e,
                _ => EDITOR.to_vec(),
            };
            let q = glue_strings(MAX_TEMPSTR, &editor, &filename, b' ');
            let pid = match sys::fork() {
                Ok(p) => p,
                Err(_) => {
                    stdio::perror(b"fork");
                    fatal(&filename);
                }
            };
            if pid == 0 {
                if sys::setgid(sys::getgid()).is_err() {
                    stdio::perror(b"setgid(getgid())");
                    exit(ERROR_EXIT);
                }
                if sys::setuid(sys::getuid()).is_err() {
                    stdio::perror(b"setuid(getuid())");
                    exit(ERROR_EXIT);
                }
                let Some(q) = q else {
                    err(&bcat!(self.program, ": editor command line too long\n"));
                    exit(ERROR_EXIT);
                };
                sys::execl(PATH_BSHELL, &[PATH_BSHELL, b"-c", &q]);
                stdio::perror(&editor);
                exit(ERROR_EXIT);
            }
            loop {
                let (xpid, waiter) = sys::waitpid(pid);
                if xpid == -1 {
                    if sys::cerrno() != libc::EINTR {
                        err(&bcat!(
                            self.program,
                            ": waitpid() failed waiting for PID ",
                            pid.to_string(),
                            " from \"",
                            editor,
                            "\": ",
                            sys::strerror(sys::cerrno()),
                            "\n"
                        ));
                    }
                } else if xpid != pid {
                    err(&bcat!(
                        self.program,
                        ": wrong PID (",
                        xpid.to_string(),
                        " != ",
                        pid.to_string(),
                        ") from \"",
                        editor,
                        "\"\n"
                    ));
                    fatal(&filename);
                } else if libc::WIFEXITED(waiter) && libc::WEXITSTATUS(waiter) != 0 {
                    err(&bcat!(
                        self.program,
                        ": \"",
                        editor,
                        "\" exited with status ",
                        libc::WEXITSTATUS(waiter).to_string(),
                        "\n"
                    ));
                    fatal(&filename);
                } else if libc::WIFSIGNALED(waiter) {
                    let core = if libc::WCOREDUMP(waiter) { "" } else { "no " };
                    err(&bcat!(
                        self.program,
                        ": \"",
                        editor,
                        "\" killed; signal ",
                        libc::WTERMSIG(waiter).to_string(),
                        " (",
                        core,
                        "core dumped)\n"
                    ));
                    fatal(&filename);
                } else {
                    break;
                }
            }
            sys::signal_default(libc::SIGHUP);
            sys::signal_default(libc::SIGINT);
            sys::signal_default(libc::SIGQUIT);

            let statbuf = match sys::lstat(&filename) {
                Ok(s) => s,
                Err(_) => {
                    stdio::perror(b"lstat");
                    fatal(&filename);
                }
            };
            let mut remove = true;
            if !sys::is_reg(statbuf.mode) {
                err(&bcat!(self.program, ": illegal crontab\n"));
            } else if statbuf.mtime == 0 {
                err(&bcat!(self.program, ": no changes made to crontab\n"));
            } else {
                err(&bcat!(self.program, ": installing new crontab\n"));
                sys::close(new_crontab_fd);
                if swap_uids().is_err() {
                    stdio::perror(b"swapping uids");
                } else {
                    match sys::open(&filename, libc::O_RDWR, 0) {
                        Err(_) => stdio::perror(b"cannot read new crontab"),
                        Ok(fd) => {
                            if swap_uids_back().is_err() {
                                stdio::perror(b"swapping uids back");
                                exit(ERROR_EXIT);
                            }
                            new_crontab_fd = fd;
                            self.new_crontab = Some(Reader::from_fd(fd));
                            match self.replace_cmd() {
                                0 => {}
                                -1 => {
                                    let retry = loop {
                                        out(b"Do you want to retry the same edit? (Y/N) ");
                                        stdio::flush_stdout();
                                        // QUIRK: at EOF fgets keeps failing, and this asks forever.
                                        let Some(q) = self.stdin.fgets(MAX_TEMPSTR) else { continue };
                                        match q.first() {
                                            Some(b'y' | b'Y') => break true,
                                            Some(b'n' | b'N') => break false,
                                            _ => err(b"Enter Y or N\n"),
                                        }
                                    };
                                    if retry {
                                        continue;
                                    }
                                    err(&bcat!(self.program, ": edits left in ", filename, "\n"));
                                    remove = false;
                                }
                                _ => {
                                    err(&bcat!(self.program, ": edits left in ", filename, "\n"));
                                    remove = false;
                                }
                            }
                        }
                    }
                }
            }
            if remove {
                let _ = sys::unlink(&filename);
            }
            log_it(&self.real_user, self.pid, b"END EDIT", &self.user, 0);
            return;
        }
    }

    /// `test_cmd()`.
    fn test_cmd(&mut self) -> i32 {
        let mut f = self.new_crontab.take().expect("parse_args opened it");
        if self.check_syntax(&mut f) < 0 {
            err(b"Invalid crontab file. Syntax issues were found.\n");
            return -2;
        }
        err(b"No syntax issues were found in the crontab file.\n");
        0
    }

    /// `replace_cmd()`: 0 on success, -1 on a syntax error, -2 on an
    /// install error.
    fn replace_cmd(&mut self) -> i32 {
        let Some(safename) = Self::host_specific_filename(b"#tmp", b"XXXXXXXXXX") else {
            err(b"path too long\n");
            return -2;
        };
        let Some(mut temp_name) = glue_strings(MAX_FNAME, SPOOL_DIR, &safename, b'/') else {
            err(b"path too long\n");
            return -2;
        };
        let fd = match sys::mkstemp(&mut temp_name) {
            Ok(fd) => fd,
            Err(_) => {
                stdio::perror(&temp_name);
                return -2;
            }
        };
        set_temp(Some(&temp_name));
        sys::signal_handler(libc::SIGHUP, die);
        sys::signal_handler(libc::SIGINT, die);
        sys::signal_handler(libc::SIGQUIT, die);

        let error = self.replace_body(fd, &temp_name);

        sys::signal_default(libc::SIGHUP);
        sys::signal_default(libc::SIGINT);
        sys::signal_default(libc::SIGQUIT);
        if let Some(t) = temp() {
            let _ = sys::unlink(&t);
            set_temp(None);
        }
        error
    }

    fn replace_body(&mut self, fd: sys::Fd, temp_name: &[u8]) -> i32 {
        let mut tmp = CFile::from_fd(fd);
        let mut copy = Vec::new();
        {
            let src = self.new_crontab.as_mut().expect("parse_args opened it");
            src.rewind();
            globals::set_line_number(1);
            loop {
                let ch = get_char(src);
                if ch == EOF {
                    break;
                }
                copy.push(ch as u8);
            }
        }
        tmp.write(&copy);
        let write_error = |p: &[u8]| err(&bcat!(p, ": error while writing new crontab to ", temp_name, "\n"));
        if sys::ftruncate(fd, copy.len() as i64).is_err() {
            write_error(&self.program);
            tmp.close();
            return -2;
        }
        if tmp.has_error() || !tmp.flush() || sys::fsync(fd).is_err() {
            write_error(&self.program);
            tmp.close();
            return -2;
        }
        let mut check = Reader::new(copy);
        let error = self.check_syntax(&mut check);
        if error < 0 {
            err(b"Invalid crontab file, can't install.\n");
            tmp.close();
            return error;
        }
        let file_owner =
            if sys::getgid() == sys::geteuid() && sys::getgid() == sys::getegid() { 0 } else { self.pw.uid };
        if sys::fchown(fd, file_owner, u32::MAX).is_err() {
            stdio::perror(b"fchown");
            tmp.close();
            return -2;
        }
        if !tmp.close() {
            stdio::perror(b"fclose");
            return -2;
        }
        let Some(n) = glue_strings(MAX_FNAME, SPOOL_DIR, &self.user, b'/') else {
            err(b"path too long\n");
            return -2;
        };
        self.backup_crontab(&n);
        if sys::rename(temp_name, &n).is_err() {
            err(&bcat!(self.program, ": error renaming ", temp_name, " to ", n, "\n"));
            stdio::perror(b"rename");
            return -2;
        }
        set_temp(None);
        log_it(&self.real_user, self.pid, b"REPLACE", &self.user, 0);
        self.poke_daemon();
        0
    }

    /// `check_error()`: report a syntax error at the current line.
    fn check_error(&mut self, msg: &[u8]) {
        self.check_error_count += 1;
        err(&bcat!("\"", self.filename, "\":", (globals::line_number() - 1).to_string(), ": ", msg, "\n"));
    }

    /// `check_syntax()`: 0, -1 (syntax errors) or -2.
    fn check_syntax(&mut self, f: &mut Reader) -> i32 {
        let envp: Env = Vec::new();
        let mut eof = false;
        let mut envs = 0usize;
        let mut entries = 0usize;
        self.check_error_count = 0;
        globals::set_line_number(1);
        while self.check_error_count == 0 && !eof {
            if !skip_comments(f) {
                self.check_error(b"too much non-parseable content (comments, empty lines, spaces)");
                break;
            }
            match load_env(f) {
                LoadEnv::Eof(partial) => {
                    if !partial.is_empty() {
                        globals::set_line_number(globals::line_number() + 1);
                        self.check_error(b"premature EOF");
                    }
                    eof = true;
                }
                LoadEnv::NotEnv => {
                    let pw = self.pw.clone();
                    let mut errors: Vec<Vec<u8>> = Vec::new();
                    let mut on_error = |m: &[u8]| errors.push(m.to_vec());
                    // check_error reads LineNumber when called, which is
                    // right after load_entry skipped the bad line.
                    let e = load_entry(f, &mut on_error, Some(&pw), &envp);
                    for m in errors {
                        self.check_error(&m);
                    }
                    if e.is_some() {
                        entries += 1;
                    }
                }
                LoadEnv::Env(_) => envs += 1,
            }
        }
        if envs > MAX_USER_ENVS {
            err(format!("There are too many environment variables in the crontab file. Limit: {MAX_USER_ENVS}\n")
                .as_bytes());
            return -1;
        }
        if entries > MAX_USER_ENTRIES {
            err(format!("There are too many entries in the crontab file. Limit: {MAX_USER_ENTRIES}\n").as_bytes());
            return -1;
        }
        if self.check_error_count != 0 {
            return -1;
        }
        0
    }

    /// `hostset_cmd()`.
    fn hostset_cmd(&mut self) -> i32 {
        if !self.host_specified {
            self.host = sys::gethostname().unwrap_or_default();
        }
        let Some(safename) = Self::host_specific_filename(b"#tmp", b"XXXXXXXXXX") else {
            err(b"path too long\n");
            return -2;
        };
        let Some(mut temp_name) = glue_strings(MAX_FNAME, SPOOL_DIR, &safename, b'/') else {
            err(b"path too long\n");
            return -2;
        };
        let fd = match sys::mkstemp(&mut temp_name) {
            Ok(fd) => fd,
            Err(_) => {
                stdio::perror(&temp_name);
                return -2;
            }
        };
        set_temp(Some(&temp_name));
        sys::signal_handler(libc::SIGHUP, die);
        sys::signal_handler(libc::SIGINT, die);
        sys::signal_handler(libc::SIGQUIT, die);
        let _ = sys::fchmod(fd, 0o600);
        let mut error = 0;
        let mut tmp = CFile::from_fd(fd);
        tmp.write(&bcat!(self.host, "\n"));
        if !tmp.close() {
            err(&bcat!(self.program, ": error while writing to ", temp_name, "\n"));
            error = -2;
        } else {
            let n = glue_strings(MAX_FNAME, SPOOL_DIR, CRON_HOSTNAME, b'/');
            match n {
                None => {
                    err(b"path too long\n");
                    error = -2;
                }
                Some(n) => {
                    if sys::rename(&temp_name, &n).is_err() {
                        err(&bcat!(self.program, ": error renaming ", temp_name, " to ", n, "\n"));
                        stdio::perror(b"rename");
                        error = -2;
                    } else {
                        set_temp(None);
                        log_it(&self.real_user, self.pid, b"SET HOST", &self.host, 0);
                        self.poke_daemon();
                    }
                }
            }
        }
        sys::signal_default(libc::SIGHUP);
        sys::signal_default(libc::SIGINT);
        sys::signal_default(libc::SIGQUIT);
        if let Some(t) = temp() {
            let _ = sys::unlink(&t);
            set_temp(None);
        }
        error
    }

    /// `hostget_cmd()`.
    fn hostget_cmd(&mut self) -> i32 {
        let Some(n) = glue_strings(MAX_FNAME, SPOOL_DIR, CRON_HOSTNAME, b'/') else {
            err(b"path too long\n");
            return -2;
        };
        let mut f = match open_read(&n) {
            Ok(f) => f,
            Err(e) => {
                if e == libc::ENOENT {
                    err(&bcat!("File ", n, " not found\n"));
                } else {
                    stdio::perror(&n);
                }
                return -2;
            }
        };
        let mut host = Vec::new();
        if get_string(&mut host, MAXHOSTNAMELEN, &mut f, b"\n") == EOF {
            err(&bcat!("Error reading from ", n, "\n"));
            if let Some(fd) = f.fd() {
                sys::close(fd);
            }
            return -2;
        }
        if let Some(fd) = f.fd() {
            sys::close(fd);
        }
        self.host = host;
        out(&bcat!(self.host, "\n"));
        stdio::flush_stdout();
        log_it(&self.real_user, self.pid, b"GET HOST", &self.host, 0);
        0
    }

    /// `poke_daemon()`: touch the spool so crond rereads it.
    fn poke_daemon(&self) {
        if sys::utime(SPOOL_DIR, None).is_err() {
            err(b"crontab: can't update mtime on spooldir\n");
            stdio::perror(SPOOL_DIR);
        }
    }
}

fn main() {
    let args: Vec<Vec<u8>> = std::env::args_os().map(std::os::unix::ffi::OsStringExt::into_vec).collect();
    let argv0 = args.first().cloned().unwrap_or_default();
    let program = match argv0.iter().rposition(|&b| b == b'/') {
        Some(i) => argv0[i + 1..].to_vec(),
        None => argv0,
    };
    globals::set_program_name(&program);
    let mut c = Crontab {
        program,
        pid: sys::getpid(),
        user: Vec::new(),
        real_user: Vec::new(),
        filename: Vec::new(),
        host: Vec::new(),
        new_crontab: None,
        check_error_count: 0,
        prompt_on_delete: false,
        host_specified: false,
        option: Opt::Unknown,
        pw: Passwd::default(),
        stdin: Reader::from_fd(0),
    };
    sys::setlocale_all();
    c.parse_args(args);
    cronie::misc::check_spool_dir();
    if !security::allowed(&c.real_user, CRON_ALLOW, CRON_DENY) {
        err(&bcat!("You (", c.user, ") are not allowed to use this program (", c.program, ")\n"));
        err(b"See crontab(1) for more information\n");
        log_it(&c.real_user, c.pid, b"AUTH", b"crontab command not allowed", 0);
        exit(ERROR_EXIT);
    }
    if sys::getuid() != 0 && security::cron_start_pam(&c.pw) != sys::PAM_SUCCESS {
        err(&bcat!(
            "You (",
            c.user,
            ") are not allowed to access to (",
            c.program,
            ") because of pam configuration.\n"
        ));
        exit(ERROR_EXIT);
    }
    let mut status = 0;
    match c.option {
        Opt::Unknown => status = ERROR_EXIT,
        Opt::List => c.list_cmd(),
        Opt::Delete => c.delete_cmd(),
        Opt::Edit => c.edit_cmd(),
        Opt::Replace => {
            if c.replace_cmd() < 0 {
                status = ERROR_EXIT;
            }
        }
        Opt::HostSet => {
            if c.hostset_cmd() < 0 {
                status = ERROR_EXIT;
            }
        }
        Opt::HostGet => {
            if c.hostget_cmd() < 0 {
                status = ERROR_EXIT;
            }
        }
        Opt::Test => {
            if c.test_cmd() < 0 {
                status = ERROR_EXIT;
            }
        }
    }
    security::cron_close_pam();
    exit(status)
}
