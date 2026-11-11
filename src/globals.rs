//! cronie's globals (globals.h), and `log_it()` / `set_debug_flags()` from
//! misc.c.
//!
//! The C programs keep this state in globals that the shared code reads
//! (`ChangePath` decides a job's default PATH, `RandomScale` scales
//! `RANDOM_DELAY`, and so on). They're single-threaded, so thread-locals
//! give the same behavior.

use std::cell::{Cell, RefCell};

use crate::bcat;
use crate::stdio;
use crate::sys;

thread_local! {
    static LINE_NUMBER: Cell<i32> = const { Cell::new(0) };
    static CHANGE_PATH: Cell<bool> = const { Cell::new(false) };
    static RANDOM_SCALE: Cell<f64> = const { Cell::new(0.0) };
    static PERMIT_ANY_CRONTAB: Cell<bool> = const { Cell::new(false) };
    static ENABLE_CLUSTERING: Cell<bool> = const { Cell::new(false) };
    static DEBUG_FLAGS: Cell<i32> = const { Cell::new(0) };
    static SYSLOG_OPEN: Cell<bool> = const { Cell::new(false) };
    static PROGRAM_NAME: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

macro_rules! accessor {
    ($get:ident, $set:ident, $cell:ident, $t:ty) => {
        pub fn $get() -> $t {
            $cell.with(Cell::get)
        }
        pub fn $set(v: $t) {
            $cell.with(|c| c.set(v));
        }
    };
}

accessor!(line_number, set_line_number, LINE_NUMBER, i32);
accessor!(change_path, set_change_path, CHANGE_PATH, bool);
accessor!(random_scale, set_random_scale, RANDOM_SCALE, f64);
accessor!(permit_any_crontab, set_permit_any_crontab, PERMIT_ANY_CRONTAB, bool);
accessor!(enable_clustering, set_enable_clustering, ENABLE_CLUSTERING, bool);
accessor!(debug_flags, set_debug_flags_value, DEBUG_FLAGS, i32);

/// `ProgramName`: the syslog ident (`None`: glibc's program short name).
pub fn set_program_name(name: &[u8]) {
    PROGRAM_NAME.with(|p| *p.borrow_mut() = Some(name.to_vec()));
}

pub const MONTH_NAMES: [&[u8]; 12] =
    [b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec"];
pub const DOW_NAMES: [&[u8]; 8] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat", b"Sun"];
pub const DEBUG_FLAG_NAMES: [&[u8]; 8] = [b"ext", b"sch", b"proc", b"pars", b"load", b"misc", b"test", b"bit"];

pub const DEXT: i32 = 0x0001;
pub const DSCH: i32 = 0x0002;
pub const DPROC: i32 = 0x0004;
pub const DPARS: i32 = 0x0008;
pub const DLOAD: i32 = 0x0010;
pub const DMISC: i32 = 0x0020;
pub const DTEST: i32 = 0x0040;

/// `Debug(mask, (...))`: printf to stdout when a debug flag is on.
pub fn debug(mask: i32, msg: impl FnOnce() -> Vec<u8>) {
    if debug_flags() & mask != 0 {
        stdio::out(&msg());
    }
}

/// `strcmp_until(left, right, until)`: equal up to `until` or the end.
pub fn strcmp_until(left: &[u8], right: &[u8], until: u8) -> i32 {
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while at(left, i) != 0 && at(left, i) != until && at(left, i) == at(right, i) {
        i += 1;
    }
    let (l, r) = (at(left, i), at(right, i));
    if (l == 0 || l == until) && (r == 0 || r == until) {
        return 0;
    }
    i32::from(l) - i32::from(r)
}

/// `set_debug_flags(flags)` (DEBUGGING builds): "flag[,flag...]".
pub fn set_debug_flags(flags: &[u8]) -> bool {
    set_debug_flags_value(0);
    let mut pc = flags;
    let mut mask_all = 0;
    while !pc.is_empty() {
        let Some(i) = DEBUG_FLAG_NAMES.iter().position(|n| strcmp_until(n, pc, b',') == 0) else {
            stdio::err(&bcat!("unrecognized debug flag <", flags, "> <", pc, ">\n"));
            set_debug_flags_value(mask_all);
            return false;
        };
        mask_all |= 1 << i;
        set_debug_flags_value(mask_all);
        let skip = pc.iter().position(|&b| b == b',').map_or(pc.len(), |p| p + 1);
        pc = &pc[skip..];
    }
    if mask_all != 0 {
        let mut msg = b"debug flags enabled:".to_vec();
        for (i, n) in DEBUG_FLAG_NAMES.iter().enumerate() {
            if mask_all & (1 << i) != 0 {
                msg.extend_from_slice(&bcat!(" ", n));
            }
        }
        msg.push(b'\n');
        stdio::err(&msg);
    }
    true
}

/// `log_close()`: close syslog (the next `log_it` reopens it).
pub fn log_close() {
    sys::closelog();
    SYSLOG_OPEN.with(|c| c.set(false));
}

/// The current `ProgramName` (the syslog ident).
pub fn program_name() -> Option<Vec<u8>> {
    PROGRAM_NAME.with(|p| p.borrow().clone())
}

/// Change `ProgramName` in place, as C's upshifting does: an open syslog
/// connection keeps C's pointer, so it shows the new name too.
pub fn rename_program(name: &[u8]) {
    set_program_name(name);
    if SYSLOG_OPEN.with(Cell::get) {
        sys::openlog(Some(sys::leak_cstring(name)), libc::LOG_PID, libc::LOG_CRON);
    }
}

/// `log_it(username, pid, event, detail, err)`: syslog (cronie is built
/// with SYSLOG, not LOG_FILE), plus stderr when debugging.
pub fn log_it(username: &[u8], pid: i32, event: &[u8], detail: &[u8], err: i32) {
    if !SYSLOG_OPEN.with(Cell::get) {
        let ident = PROGRAM_NAME.with(|p| p.borrow().as_deref().map(sys::leak_cstring));
        sys::openlog(ident, libc::LOG_PID, libc::LOG_CRON);
        SYSLOG_OPEN.with(|c| c.set(true));
    }
    let tail = if err != 0 { bcat!(": ", sys::strerror(err)) } else { Vec::new() };
    let priority = if err != 0 { libc::LOG_ERR } else { libc::LOG_INFO };
    sys::syslog(priority, &bcat!("(", username, ") ", event, " (", detail, ")", tail));
    if debug_flags() != 0 {
        stdio::err(&bcat!("log_it: (", username, " ", pid.to_string(), ") ", event, " (", detail, ")", tail, "\n"));
    }
}
