//! Port of src/entry.c: one crontab entry, parsed from a crontab file.
//!
//! Upstream license: ISC (see LICENSE).

use crate::env::{self, Env, MAX_ENVSTR, env_get, env_set, glue_strings};
use crate::globals::{self, DEXT, DOW_NAMES, DPARS, MONTH_NAMES};
use crate::reader::{EOF, Reader, get_char, get_string, skip_blanks, skip_nonblanks, unget_char};
use crate::sys::{self, Passwd};
use crate::{bcat, random};

pub const MAX_COMMAND: usize = 131_072;
pub const MAX_TEMPSTR: usize = 131_072;
const PATH_BSHELL: &[u8] = b"/bin/sh";
const PATH_STDPATH: &[u8] = b"/usr/bin:/bin:/usr/sbin:/sbin";

pub const FIRST_MINUTE: i32 = 0;
pub const LAST_MINUTE: i32 = 59;
pub const FIRST_HOUR: i32 = 0;
pub const LAST_HOUR: i32 = 23;
pub const FIRST_DOM: i32 = 1;
pub const LAST_DOM: i32 = 31;
pub const FIRST_MONTH: i32 = 1;
pub const LAST_MONTH: i32 = 12;
pub const FIRST_DOW: i32 = 0;
pub const LAST_DOW: i32 = 7;

pub const MIN_STAR: i32 = 0x01;
pub const HR_STAR: i32 = 0x02;
pub const DOM_STAR: i32 = 0x04;
pub const DOW_STAR: i32 = 0x08;
pub const WHEN_REBOOT: i32 = 0x10;
pub const DONT_LOG: i32 = 0x20;
pub const MAIL_WHEN_ERR: i32 = 0x40;

/// A `bitstr_t` of up to 64 bits (minutes are the widest field, 60).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bits(pub u64);

impl Bits {
    pub fn test(self, i: i32) -> bool {
        (0..64).contains(&i) && self.0 & (1 << i) != 0
    }
    pub fn set(&mut self, i: i32) {
        self.0 |= 1 << i;
    }
    /// `bit_nset(bits, 0, n)`: bits 0..=n.
    pub fn set_range(&mut self, n: i32) {
        for i in 0..=n {
            self.set(i);
        }
    }
}

/// One time field for the parse loop: bits, range, names, star flag, error.
type Field<'a> = (&'a mut Bits, i32, i32, Option<&'a [&'a [u8]]>, i32, Ecode);

/// `struct _entry`.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub pwd: Passwd,
    pub envp: Env,
    pub cmd: Vec<u8>,
    pub minute: Bits,
    pub hour: Bits,
    pub dom: Bits,
    pub month: Bits,
    pub dow: Bits,
    pub flags: i32,
    pub delay: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ecode {
    None,
    Minute,
    Hour,
    Dom,
    Month,
    Dow,
    Cmd,
    Timespec,
    Option,
}

impl Ecode {
    fn text(self) -> &'static [u8] {
        match self {
            Ecode::None => b"no error",
            Ecode::Minute => b"bad minute",
            Ecode::Hour => b"bad hour",
            Ecode::Dom => b"bad day-of-month",
            Ecode::Month => b"bad month",
            Ecode::Dow => b"bad day-of-week",
            Ecode::Cmd => b"bad command",
            Ecode::Timespec => b"bad time specifier",
            Ecode::Option => b"bad option",
        }
    }
}

const NL: i32 = b'\n' as i32;
const SP: i32 = b' ' as i32;
const TAB: i32 = b'\t' as i32;

/// `load_entry(file, error_func, pw, envp)`: the next entry, or `None` at
/// EOF or after a syntax error (reported through `error_func`).  `pw` is
/// `None` for system crontabs, which have a user name column.
pub fn load_entry(
    file: &mut Reader,
    error_func: &mut dyn FnMut(&[u8]),
    pw: Option<&Passwd>,
    envp: &Env,
) -> Option<Entry> {
    globals::debug(DPARS, || b"load_entry()...about to eat comments\n".to_vec());
    let mut ch = get_char(file);
    if ch == EOF {
        return None;
    }
    let mut e = Entry::default();
    let ecode = match parse(file, &mut ch, &mut e, pw, envp) {
        Ok(()) => return Some(e),
        Err(Some(code)) => code,
        Err(None) => return None,
    };
    // eof: skip the rest of the line.
    let mut i = 0;
    while i < MAX_COMMAND && ch != NL && !file.feof() {
        ch = get_char(file);
        i += 1;
    }
    if ecode != Ecode::None {
        error_func(ecode.text());
    }
    None
}

/// The body of `load_entry()`.  `Err(None)` is the early `return NULL`
/// that skips the error path.
fn parse(file: &mut Reader, ch: &mut i32, e: &mut Entry, pw: Option<&Passwd>, envp: &Env) -> Result<(), Option<Ecode>> {
    let mut cmd = Vec::new();

    if *ch == i32::from(b'-') {
        if pw.is_none_or(|p| p.uid == 0) {
            e.flags |= DONT_LOG;
        } else {
            globals::log_it(b"CRON", sys::getpid(), b"ERROR", b"Only privileged user can disable logging", 0);
            return Err(Some(Ecode::Option));
        }
        *ch = get_char(file);
        if *ch == EOF {
            return Err(None);
        }
    }

    if *ch == i32::from(b'@') {
        *ch = get_string(&mut cmd, MAX_COMMAND, file, b" \t\n");
        match cmd.as_slice() {
            b"reboot" => e.flags |= WHEN_REBOOT,
            b"yearly" | b"annually" => {
                e.minute.set(0);
                e.hour.set(0);
                e.dom.set(0);
                e.month.set(0);
                e.dow.set_range(LAST_DOW - FIRST_DOW);
                e.flags |= DOW_STAR;
            }
            b"monthly" => {
                e.minute.set(0);
                e.hour.set(0);
                e.dom.set(0);
                e.month.set_range(LAST_MONTH - FIRST_MONTH);
                e.dow.set_range(LAST_DOW - FIRST_DOW);
                e.flags |= DOW_STAR;
            }
            b"weekly" => {
                e.minute.set(0);
                e.hour.set(0);
                e.dom.set_range(LAST_DOM - FIRST_DOM);
                e.month.set_range(LAST_MONTH - FIRST_MONTH);
                e.dow.set(0);
                e.flags |= DOM_STAR;
            }
            b"daily" | b"midnight" => {
                e.minute.set(0);
                e.hour.set(0);
                e.dom.set_range(LAST_DOM - FIRST_DOM);
                e.month.set_range(LAST_MONTH - FIRST_MONTH);
                e.dow.set_range(LAST_DOW - FIRST_DOW);
            }
            b"hourly" => {
                e.minute.set(0);
                e.hour.set_range(LAST_HOUR - FIRST_HOUR);
                e.dom.set_range(LAST_DOM - FIRST_DOM);
                e.month.set_range(LAST_MONTH - FIRST_MONTH);
                e.dow.set_range(LAST_DOW - FIRST_DOW);
                e.flags |= HR_STAR;
            }
            _ => return Err(Some(Ecode::Timespec)),
        }
        *ch = skip_blanks(*ch, file);
        if *ch == EOF || *ch == NL {
            return Err(Some(Ecode::Cmd));
        }
    } else {
        globals::debug(DPARS, || b"load_entry()...about to parse numerics\n".to_vec());
        let fields: [Field<'_>; 5] = [
            (&mut e.minute, FIRST_MINUTE, LAST_MINUTE, None, MIN_STAR, Ecode::Minute),
            (&mut e.hour, FIRST_HOUR, LAST_HOUR, None, HR_STAR, Ecode::Hour),
            (&mut e.dom, FIRST_DOM, LAST_DOM, None, DOM_STAR, Ecode::Dom),
            (&mut e.month, FIRST_MONTH, LAST_MONTH, Some(&MONTH_NAMES), 0, Ecode::Month),
            (&mut e.dow, FIRST_DOW, LAST_DOW, Some(&DOW_NAMES), DOW_STAR, Ecode::Dow),
        ];
        let mut flags = 0;
        for (bits, low, high, names, star, code) in fields {
            if *ch == i32::from(b'*') {
                flags |= star;
            }
            *ch = get_list(bits, low, high, names, *ch, file);
            if *ch == EOF {
                e.flags |= flags;
                return Err(Some(code));
            }
        }
        e.flags |= flags;
    }

    // make sundays equivalent
    if e.dow.test(0) || e.dow.test(7) {
        e.dow.set(0);
        e.dow.set(7);
    }

    // check for premature EOL and catch a common typo
    if *ch == NL || *ch == i32::from(b'*') {
        return Err(Some(Ecode::Cmd));
    }

    unget_char(*ch, file);

    let looked_up;
    let pw: Passwd = match pw {
        Some(p) => p.clone(),
        None => {
            globals::debug(DPARS, || b"load_entry()...about to parse username\n".to_vec());
            let mut username = Vec::new();
            *ch = get_string(&mut username, MAX_COMMAND, file, b" \t\n");
            globals::debug(DPARS, || bcat!("load_entry()...got ", username, "\n"));
            if *ch == EOF || *ch == NL || *ch == i32::from(b'*') {
                return Err(Some(Ecode::Cmd));
            }
            looked_up = sys::getpwnam(&username);
            let p = match &looked_up {
                None => {
                    globals::debug(DPARS, || b"load_entry()...unknown user entry\n".to_vec());
                    Passwd { name: username.clone(), ..Passwd::default() }
                }
                Some(p) => {
                    globals::debug(DPARS, || {
                        bcat!("load_entry()...uid ", p.uid.to_string(), ", gid ", p.gid.to_string(), "\n")
                    });
                    p.clone()
                }
            };
            *ch = skip_blanks(*ch, file);
            if *ch == EOF || *ch == NL {
                return Err(Some(Ecode::Cmd));
            }
            unget_char(*ch, file);
            p
        }
    };

    // pw_dup(), then the password is wiped.
    e.pwd = pw.clone();
    e.pwd.passwd = vec![0; e.pwd.passwd.len()];

    if let Some(p) = env_get(b"RANDOM_DELAY", envp) {
        let parsed = crate::num::strtoimax(p, 10);
        if parsed.erange || parsed.value < 0 || parsed.value > 24 * 60 {
            globals::log_it(b"CRON", sys::getpid(), b"ERROR", b"bad value of RANDOM_DELAY", 0);
        } else {
            e.delay = (parsed.value as f64 * globals::random_scale()) as i32;
        }
    }

    // copy and fix up environment.
    e.envp = envp.clone();
    if env_get(b"SHELL", &e.envp).is_none() {
        match glue_strings(MAX_ENVSTR, b"SHELL", PATH_BSHELL, b'=') {
            Some(s) => env_set(&mut e.envp, &s),
            None => globals::log_it(b"CRON", sys::getpid(), b"ERROR", b"can't set SHELL", 0),
        }
    }
    env::env_update_home(&mut e.envp, &pw.dir);
    if env_get(b"PATH", &e.envp).is_none() {
        let defpath = if globals::change_path() {
            PATH_STDPATH.to_vec()
        } else {
            sys::getenv(b"PATH").unwrap_or_else(|| PATH_STDPATH.to_vec())
        };
        match glue_strings(MAX_ENVSTR, b"PATH", &defpath, b'=') {
            Some(s) => env_set(&mut e.envp, &s),
            None => globals::log_it(b"CRON", sys::getpid(), b"ERROR", b"can't set PATH", 0),
        }
    }
    for var in [&b"LOGNAME"[..], b"USER"] {
        match glue_strings(MAX_ENVSTR, var, &pw.name, b'=') {
            Some(s) => env_set(&mut e.envp, &s),
            None => globals::log_it(b"CRON", sys::getpid(), b"ERROR", &bcat!("can't set ", var), 0),
        }
    }

    globals::debug(DPARS, || b"load_entry()...about to parse command\n".to_vec());

    // A leading '-' on the command is a cron option.
    *ch = get_char(file);
    while *ch == i32::from(b'-') {
        *ch = get_char(file);
        if *ch == i32::from(b'n') {
            if e.flags & MAIL_WHEN_ERR == MAIL_WHEN_ERR {
                return Err(Some(Ecode::Option));
            }
            e.flags |= MAIL_WHEN_ERR;
        } else {
            return Err(Some(Ecode::Option));
        }
        *ch = get_char(file);
        if *ch != TAB && *ch != SP {
            return Err(Some(Ecode::Option));
        }
        *ch = skip_blanks(*ch, file);
        if *ch == EOF || *ch == NL {
            return Err(Some(Ecode::Cmd));
        }
    }
    unget_char(*ch, file);

    *ch = get_string(&mut cmd, MAX_COMMAND, file, b"\n");
    // a file without a \n before the EOF is rude, so we'll complain...
    if *ch == EOF {
        return Err(Some(Ecode::Cmd));
    }
    e.cmd = cmd;
    globals::debug(DPARS, || b"load_entry()...returning successfully\n".to_vec());
    Ok(())
}

/// `get_list()`: a comma-separated list of ranges.
fn get_list(bits: &mut Bits, low: i32, high: i32, names: Option<&[&[u8]]>, ch: i32, file: &mut Reader) -> i32 {
    globals::debug(DPARS | DEXT, || b"get_list()...entered\n".to_vec());
    *bits = Bits(0);
    unget_char(ch, file);
    let mut ch;
    loop {
        ch = get_range(bits, low, high, names, file);
        if ch == EOF {
            return EOF;
        }
        if ch != i32::from(b',') {
            break;
        }
    }
    ch = skip_nonblanks(ch, file);
    ch = skip_blanks(ch, file);
    globals::debug(DPARS | DEXT, || format!("get_list()...exiting w/ {ch:02x}\n").into_bytes());
    ch
}

fn is_separator(ch: i32) -> bool {
    ch == TAB || ch == NL || ch == SP || ch == i32::from(b',')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum R {
    Start,
    Ast,
    Step,
    Terms,
    Num1,
    Range,
    RangeNum2,
    Random,
    Finish,
}

/// `get_range()`: `n`, `n-m[/s]`, `*[/s]`, or `[n]~[m]`.
fn get_range(bits: &mut Bits, low: i32, high: i32, names: Option<&[&[u8]]>, file: &mut Reader) -> i32 {
    let mut step = 1;
    let (mut low_, mut high_) = (0, 0);
    let mut state = R::Start;
    let mut ch = EOF;
    while state != R::Finish {
        ch = get_char(file);
        if ch == EOF {
            break;
        }
        match state {
            R::Start => {
                if ch == i32::from(b'*') {
                    low_ = low;
                    high_ = high;
                    state = R::Ast;
                } else if ch == i32::from(b'~') {
                    low_ = low;
                    state = R::Random;
                } else {
                    unget_char(ch, file);
                    if get_number(&mut low_, low, names, file) != EOF {
                        state = R::Num1;
                    } else {
                        return EOF;
                    }
                }
            }
            R::Ast => {
                if ch == i32::from(b'/') {
                    state = R::Step;
                } else if is_separator(ch) {
                    state = R::Finish;
                } else {
                    return EOF;
                }
            }
            R::Step => {
                unget_char(ch, file);
                if get_number(&mut step, 0, None, file) != EOF && step != 0 {
                    state = R::Terms;
                } else {
                    return EOF;
                }
            }
            R::Terms => {
                if is_separator(ch) {
                    state = R::Finish;
                } else {
                    return EOF;
                }
            }
            R::Num1 => {
                if ch == i32::from(b'-') {
                    state = R::Range;
                } else if ch == i32::from(b'~') {
                    state = R::Random;
                } else if is_separator(ch) {
                    high_ = low_;
                    state = R::Finish;
                } else {
                    return EOF;
                }
            }
            R::Range => {
                unget_char(ch, file);
                if get_number(&mut high_, low, names, file) != EOF {
                    state = R::RangeNum2;
                } else {
                    return EOF;
                }
            }
            R::RangeNum2 => {
                if ch == i32::from(b'/') {
                    state = R::Step;
                } else if is_separator(ch) {
                    state = R::Finish;
                } else {
                    return EOF;
                }
            }
            R::Random => {
                if is_separator(ch) {
                    high_ = high;
                    state = R::Finish;
                } else {
                    unget_char(ch, file);
                    if get_number(&mut high_, low, names, file) != EOF {
                        state = R::Terms;
                    } else {
                        return EOF;
                    }
                }
                if low_ > high_ {
                    return EOF;
                }
                let span = i64::from(high_) - i64::from(low_) + 1;
                low_ = (random::random() % span + i64::from(low_)) as i32;
                high_ = low_;
            }
            R::Finish => unreachable!(),
        }
    }
    if state != R::Finish || ch == EOF {
        return EOF;
    }

    if step > 1 && step > high_.wrapping_sub(low_) {
        let max = if high_ - low_ > 0 { high_ - low_ } else { 1 };
        crate::stdio::err(format!("Warning: Step size {step} higher than possible maximum of {max}\n").as_bytes());
    }

    let mut i = low_;
    while i <= high_ {
        if set_element(bits, low, high, i).is_err() {
            unget_char(ch, file);
            return EOF;
        }
        match i.checked_add(step) {
            Some(n) => i = n,
            // C's signed overflow; the loop would end (or misbehave).
            None => break,
        }
    }
    ch
}

/// `get_number()`: a number, or a name from `names` (case-insensitive).
fn get_number(numptr: &mut i32, low: i32, names: Option<&[&[u8]]>, file: &mut Reader) -> i32 {
    let mut temp = Vec::new();
    let mut ch;
    loop {
        ch = get_char(file);
        if !(0..256).contains(&ch) || !(ch as u8).is_ascii_alphanumeric() {
            break;
        }
        if temp.len() + 1 >= MAX_TEMPSTR {
            unget_char(ch, file);
            return EOF;
        }
        temp.push(ch as u8);
    }
    if temp.is_empty() {
        unget_char(ch, file);
        return EOF;
    }
    unget_char(ch, file);

    let parsed = crate::num::strtoimax(&temp, 10);
    if parsed.end == temp.len() && parsed.end != 0 {
        *numptr = parsed.value as i32;
        return 0;
    }
    if let Some(names) = names {
        for (i, n) in names.iter().enumerate() {
            globals::debug(DPARS | DEXT, || bcat!("get_num, compare(", n, ",", temp, ")\n"));
            if n.eq_ignore_ascii_case(&temp) {
                *numptr = i as i32 + low;
                return 0;
            }
        }
    }
    // QUIRK: `bad:` ungets `ch` again, after it was already pushed back
    // above, so the character after a bad word is read twice.
    unget_char(ch, file);
    EOF
}

/// `set_element()`.
fn set_element(bits: &mut Bits, low: i32, high: i32, number: i32) -> Result<(), ()> {
    globals::debug(DPARS | DEXT, || format!("set_element(?,{low},{high},{number})\n").into_bytes());
    if number < low || number > high {
        return Err(());
    }
    bits.set(number - low);
    Ok(())
}
