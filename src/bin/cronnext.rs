//! `cronnext`: when will the next cron job run?  Port of src/cronnext.c.
//!
//! Upstream license: GPL-2.0-or-later (see COPYING.anacron).

use cronie::bcat;
use cronie::entry::{DOM_STAR, DOW_STAR, Entry};
use cronie::getopt::Getopt;
use cronie::stdio::{self, err, out};
use cronie::sys::{self, Passwd};
use cronie::user::{self, CronDb, SPOOL_DIR};
use cronie::{PACKAGE_STRING, globals, num};

/// Print entries.
const ENTRIES: i32 = 0x01;
/// Print crontabs.
const CRONTABS: i32 = 0x02;
/// Include the system crontab.
const SYSTEM: i32 = 0x04;
/// Print all jobs in the interval.
const ALLJOBS: i32 = 0x08;

const FLAGNAME: [&str; 6] = ["MIN_STAR", "HR_STAR", "DOM_STAR", "DOW_STAR", "WHEN_REBOOT", "DONT_LOG"];

/// `printflags()`.
fn printflags(indent: &str, flags: i32) {
    let mut s = format!("{indent}    flagnames:");
    let mut first = true;
    for (f, name) in FLAGNAME.iter().enumerate() {
        if flags & (1 << f) != 0 {
            s += if first { " " } else { "|" };
            s += name;
            first = false;
        }
    }
    s.push('\n');
    out(s.as_bytes());
}

/// `printentry()`.
fn printentry(indent: &str, e: &Entry, next: i64) {
    out(&bcat!(indent, "  - user: ", e.pwd.name, "\n"));
    out(&bcat!(indent, "    cmd: \"", e.cmd, "\"\n"));
    out(format!("{indent}    flags: 0x{:02X}\n", e.flags).as_bytes());
    printflags(indent, e.flags);
    out(format!("{indent}    delay: {}\n", e.delay).as_bytes());
    out(format!("{indent}    next: {next}\n").as_bytes());
    out(format!("{indent}    nextstring: ").as_bytes());
    let text = sys::localtime(next).map(|tm| sys::asctime_text(&tm)).unwrap_or_default();
    out(&text);
}

/// `printcrontab()`.
fn printcrontab(u: &user::User) {
    out(&bcat!("  - user: \"", u.name, "\"\n"));
    out(&bcat!("    crontab: ", u.tabname, "\n"));
    out(format!("    system: {}\n", i32::from(u.system)).as_bytes());
    out(b"    entries:\n");
}

/// `matchday()`: dom if dow is `*`, dow if dom is `*`, else either.
fn matchday(e: &Entry, time: i64) -> bool {
    let tm = sys::localtime_r(time);
    if e.flags & DOW_STAR != 0 {
        return e.dom.test(tm.tm_mday - 1);
    }
    if e.flags & DOM_STAR != 0 {
        return e.dow.test(tm.tm_wday);
    }
    e.dom.test(tm.tm_mday - 1) || e.dow.test(tm.tm_wday)
}

/// `nextmatch()`: the next time in [start, end] matching the entry, or -1.
fn nextmatch(e: &Entry, start: i64, end: i64) -> i64 {
    let mut time = start;
    while time <= end {
        let mut tm = sys::localtime_r(time);
        // month doesn't match: move to the 1st of next month
        if !e.month.test(tm.tm_mon) {
            tm.tm_mon += 1;
            if tm.tm_mon >= 12 {
                tm.tm_year += 1;
                tm.tm_mon = 0;
            }
            tm.tm_mday = 1;
            tm.tm_hour = 0;
            tm.tm_min = 0;
            time = sys::mktime(&mut tm);
            continue;
        }
        // neither time nor time+1day match day: increase 1 day
        if !matchday(e, time) && !matchday(e, time + 24 * 60 * 60) {
            time += 24 * 60 * 60;
            continue;
        }
        if e.month.test(tm.tm_mon) && matchday(e, time) && e.hour.test(tm.tm_hour) && e.minute.test(tm.tm_min) {
            return time;
        }
        time += 60;
    }
    -1
}

/// `matchuser()`: is `user_name` an element of the comma-separated `list`?
fn matchuser(user_name: &[u8], list: &[u8]) -> bool {
    let l = user_name.len();
    let mut pos = 0;
    while let Some(off) = cronie::bytes::find(&list[pos..], user_name) {
        let at = pos + off;
        let before_ok = at == 0 || list[at - 1] == b',';
        let after = list.get(at + l).copied().unwrap_or(0);
        if before_ok && (after == 0 || after == b',') {
            return true;
        }
        if l == 0 {
            // strstr("") matches everywhere; C would loop forever here.
            return false;
        }
        pos = at + l;
        if pos > list.len() {
            break;
        }
    }
    false
}

/// `cronnext()`: the closest next run, printing as `flags` asks.
fn cronnext(
    db: &CronDb,
    start: i64,
    end: i64,
    include: Option<&[u8]>,
    exclude: Option<&[u8]>,
    command: Option<&[u8]>,
    flags: i32,
) -> i64 {
    let mut indent = "";
    if flags & CRONTABS != 0 {
        out(b"crontabs:\n");
        indent = "    ";
    } else if flags & ALLJOBS != 0 {
        out(b"jobs:\n");
    }
    let mut closest = -1;
    for u in &db.users {
        if include.is_some_and(|i| !matchuser(&u.name, i)) {
            continue;
        }
        if exclude.is_some_and(|x| matchuser(&u.name, x)) {
            continue;
        }
        if flags & SYSTEM == 0 && u.system {
            continue;
        }
        if flags & CRONTABS != 0 {
            printcrontab(u);
        }
        for e in &u.crontab {
            if command.is_some_and(|c| cronie::bytes::find(&e.cmd, c).is_none()) {
                continue;
            }
            let mut next = nextmatch(e, start, end);
            while next <= end {
                if next < 0 {
                    break;
                }
                if closest < 0 || next < closest {
                    closest = next;
                }
                if flags & ENTRIES != 0 {
                    printentry(indent, e, next);
                }
                if flags & ALLJOBS == 0 {
                    break;
                }
                next = nextmatch(e, next + 60, end);
            }
        }
    }
    closest
}

/// `database()`: the installed crontabs and/or the given files.
fn database(installed: bool, additional: &[Vec<u8>]) -> CronDb {
    let mut db = CronDb::default();
    if installed {
        user::load_database(&mut db);
    }
    for path in additional {
        let fd = match sys::open(path, libc::O_RDONLY, 0) {
            Ok(fd) => fd,
            Err(_) => {
                stdio::perror(path);
                continue;
            }
        };
        if let Ok(st) = sys::fstat(fd)
            && sys::is_dir(st.mode)
        {
            err(&bcat!(path, " is a directory - skipping\n"));
            sys::close(fd);
            continue;
        }
        let pw = Passwd { name: path.clone(), dir: b".".to_vec(), ..Passwd::default() };
        match user::load_user(fd, Some(&pw), path, path, path) {
            Some(u) => db.users.push(u),
            None => out(&bcat!("cannot load crontab ", path, "\n")),
        }
    }
    db
}

fn usage() {
    err(b"Find the time of the next scheduled cron job.\n");
    err(b"Usage:\n");
    err(b" cronnext [options] [file ...]\n");
    err(b"\n");
    err(b"Options:\n");
    err(b" -i users  include only the crontab of these users\n");
    err(b" -e users  exclude the crontab of these users\n");
    err(b" -s        do not include the system crontab\n");
    err(b" -a        examine installed crontabs even if files are given\n");
    err(b" -t time   start from this time (seconds since epoch)\n");
    err(b" -q time   end check at this time (seconds since epoch)\n");
    err(b" -j cmd    only check jobs that contain cmd as a substring\n");
    err(b" -l        print next jobs to be executed\n");
    err(b" -c        print next execution of each job\n");
    err(b" -f        print all jobs executed in the given interval\n");
    err(b" -h        this help\n");
    err(b" -V        print version and exit\n");
}

fn main() {
    let mut include: Option<Vec<u8>> = None;
    let mut exclude: Option<Vec<u8>> = None;
    let mut command: Option<Vec<u8>> = None;
    let mut flags = SYSTEM;
    let mut endtime = false;
    let mut printjobs = false;
    let mut installed = false;
    let mut start: i64 = (sys::time_now() + 59) / 60 * 60;
    let mut end: i64 = 0;

    let args: Vec<Vec<u8>> = std::env::args_os().map(std::os::unix::ffi::OsStringExt::into_vec).collect();
    let mut g = Getopt::new(args, "i:e:ast:q:j:lcfhV");
    while let Some(opt) = g.next_short() {
        let arg = g.optarg.clone().unwrap_or_default();
        match u8::try_from(opt).unwrap_or(0) {
            b'i' => include = Some(arg),
            b'e' => exclude = Some(arg),
            b'a' => installed = true,
            b's' => flags &= !SYSTEM,
            // int arithmetic, as in C: (atoi(optarg) + 59) / 60 * 60
            b't' => start = i64::from(num::atoi(&arg).wrapping_add(59) / 60 * 60),
            b'q' => {
                end = i64::from(num::atoi(&arg) / 60 * 60);
                endtime = true;
            }
            b'j' => command = Some(arg),
            b'l' => printjobs = true,
            b'c' => flags |= ENTRIES | CRONTABS,
            b'f' => flags |= ALLJOBS | ENTRIES,
            b'h' => {
                usage();
                stdio::exit(libc::EXIT_SUCCESS);
            }
            b'V' => {
                out(format!("{PACKAGE_STRING}\n").as_bytes());
                stdio::exit(libc::EXIT_SUCCESS);
            }
            _ => {
                let prev = g.args.get(g.optind.wrapping_sub(1)).cloned().unwrap_or_default();
                err(&bcat!("unrecognized option: ", prev, "\n"));
                usage();
                stdio::exit(libc::EXIT_FAILURE);
            }
        }
    }

    if flags & ALLJOBS != 0 && !endtime {
        err(b"no ending time specified: -f requires -q\n");
        usage();
        stdio::exit(libc::EXIT_FAILURE);
    }
    // The longest gap between two matches is 8 years ('* * 29 2 *' from
    // 1 March 2096 is next due on 29 February 2104).
    if !endtime {
        end = start + 8 * 12 * 31 * 24 * 60 * 60;
    }
    if flags & CRONTABS != 0 {
        out(&bcat!("spool: ", SPOOL_DIR, "\n"));
        globals::set_debug_flags(b"");
    }

    let files: Vec<Vec<u8>> = g.args[g.optind.min(g.args.len())..].to_vec();
    let db = database(installed || files.is_empty(), &files);

    let next = cronnext(&db, start, end, include.as_deref(), exclude.as_deref(), command.as_deref(), flags);
    if next == -1 {
        stdio::exit(libc::EXIT_FAILURE);
    }
    out(format!("next: {next}\n").as_bytes());
    if printjobs {
        out(b"nextjobs:\n");
        cronnext(
            &db,
            next,
            next,
            include.as_deref(),
            exclude.as_deref(),
            command.as_deref(),
            (flags & SYSTEM) | ENTRIES,
        );
    }
    stdio::exit(libc::EXIT_SUCCESS)
}
