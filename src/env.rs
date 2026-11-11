//! Port of src/env.c: crontab environment lists and `NAME=value` lines.
//!
//! An environment is a list of `NAME=value` byte strings, in order (it
//! becomes the job's `envp`, so the order is observable).

use crate::globals::{self, DPARS, strcmp_until};
use crate::reader::{self, EOF, Reader};
use crate::{bcat, sys};

pub type Env = Vec<Vec<u8>>;

/// `MAX_ENVSTR`.
pub const MAX_ENVSTR: usize = 131_072;

/// `env_set(envp, envstr)`: replace the entry with the same name, or add.
pub fn env_set(envp: &mut Env, envstr: &[u8]) {
    // C remembers the *last* match.
    let found = envp.iter().rposition(|e| strcmp_until(e, envstr, b'=') == 0);
    match found {
        Some(i) => envp[i] = envstr.to_vec(),
        None => envp.push(envstr.to_vec()),
    }
}

/// `env_get(name, envp)`: the value of the first entry named `name`.
pub fn env_get<'a>(name: &[u8], envp: &'a Env) -> Option<&'a [u8]> {
    for p in envp {
        let Some(q) = p.iter().position(|&b| b == b'=') else { continue };
        if q == name.len() && &p[..q] == name {
            return Some(&p[q + 1..]);
        }
    }
    None
}

/// `env_set_from_environ()`: copy the locale variables (and RANDOM_DELAY,
/// MAILFROM) from the process environment.
pub fn env_set_from_environ(envp: &mut Env) {
    const NAMES: [&[u8]; 17] = [
        b"LANG",
        b"LC_CTYPE",
        b"LC_NUMERIC",
        b"LC_TIME",
        b"LC_COLLATE",
        b"LC_MONETARY",
        b"LC_MESSAGES",
        b"LC_PAPER",
        b"LC_NAME",
        b"LC_ADDRESS",
        b"LC_TELEPHONE",
        b"LC_MEASUREMENT",
        b"LC_IDENTIFICATION",
        b"LC_ALL",
        b"LANGUAGE",
        b"RANDOM_DELAY",
        b"MAILFROM",
    ];
    for procenv in sys::environ() {
        for name in NAMES {
            if procenv.starts_with(name) && procenv.get(name.len()) == Some(&b'=') {
                env_set(envp, &procenv);
            }
        }
    }
}

/// `glue_strings(buffer, size, a, b, separator)`: `a` + separator + `b`,
/// or `None` if it doesn't fit in `size` bytes (with the NUL).  A '/'
/// separator isn't doubled after a trailing '/'.
pub fn glue_strings(size: usize, a: &[u8], b: &[u8], separator: u8) -> Option<Vec<u8>> {
    if size == 0 {
        return None;
    }
    let mut buf = Vec::new();
    for &c in a {
        if buf.len() >= size {
            break;
        }
        buf.push(c);
    }
    if buf.len() == size {
        return None;
    }
    if separator != b'/' || buf.is_empty() || buf.last() != Some(&b'/') {
        buf.push(separator);
    }
    if buf.len() == size {
        return None;
    }
    for &c in b {
        if buf.len() >= size {
            break;
        }
        buf.push(c);
    }
    if buf.len() == size {
        return None;
    }
    Some(buf)
}

/// `env_update_home(envp, dir)`: set HOME unless it's set or dir is empty.
pub fn env_update_home(envp: &mut Env, dir: &[u8]) {
    if dir.is_empty() || env_get(b"HOME", envp).is_some() {
        return;
    }
    match glue_strings(MAX_ENVSTR, b"HOME", dir, b'=') {
        Some(s) => env_set(envp, &s),
        None => globals::log_it(b"CRON", sys::getpid(), b"ERROR", b"can't set HOME", 0),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum St {
    NameI,
    Name,
    Eq1,
    Eq2,
    ValueI,
    Value,
    Fini,
    Error,
}

impl St {
    fn next(self) -> St {
        match self {
            St::NameI => St::Name,
            St::Name => St::Eq1,
            St::Eq1 => St::Eq2,
            St::Eq2 => St::ValueI,
            St::ValueI => St::Value,
            St::Value => St::Fini,
            St::Fini => St::Error,
            St::Error => St::Error,
        }
    }
}

/// What `load_env()` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadEnv {
    /// `ERR`: end of file.  The partial line is kept (to warn about a
    /// missing final newline).
    Eof(Vec<u8>),
    /// `FALSE`: not an environment setting; the file was rewound.
    NotEnv,
    /// `TRUE`: `NAME=value`, unquoted.
    Env(Vec<u8>),
}

fn is_space(b: u8) -> bool {
    crate::num::is_c_space(b)
}

/// `load_env(envstr, f)`.
pub fn load_env(f: &mut Reader) -> LoadEnv {
    let filepos = f.tell();
    let fileline = globals::line_number();
    let mut line = Vec::new();
    if reader::get_string(&mut line, MAX_ENVSTR, f, b"\n") == EOF {
        return LoadEnv::Eof(line);
    }
    globals::debug(DPARS, || bcat!("load_env, read <", line, ">\n"));

    // The C code compacts the string in place: `str` writes behind `c`.
    let src = line.clone();
    let mut out: Vec<u8> = Vec::new();
    let mut val = 0usize;
    let mut state = St::NameI;
    let mut quotechar = 0u8;
    let mut c = 0usize;
    while state != St::Error && c < src.len() {
        match state {
            St::NameI | St::ValueI | St::Name | St::Value => {
                if state == St::NameI || state == St::ValueI {
                    if src[c] == b'\'' || src[c] == b'"' {
                        quotechar = src[c];
                        c += 1;
                    }
                    state = state.next();
                    // FALLTHROUGH, possibly at the end of the string: C
                    // then reads the NUL, which is neither quote, space nor
                    // '=' and gets copied; the loop ends after.
                }
                let ch = src.get(c).copied().unwrap_or(0);
                if quotechar != 0 {
                    if ch == quotechar {
                        state = state.next();
                        c += 1;
                        continue;
                    }
                    if state == St::Name && ch == b'=' {
                        state = St::Error;
                        continue;
                    }
                } else if state == St::Name {
                    if is_space(ch) {
                        c += 1;
                        state = state.next();
                        continue;
                    }
                    if ch == b'=' {
                        state = state.next();
                        continue;
                    }
                }
                if c >= src.len() {
                    // Copying the NUL ends the string, and the loop.
                    break;
                }
                out.push(ch);
                c += 1;
            }
            St::Eq1 => {
                if src[c] == b'=' {
                    state = state.next();
                    quotechar = 0;
                    out.push(src[c]);
                    val = out.len();
                } else if !is_space(src[c]) {
                    state = St::Error;
                }
                c += 1;
            }
            St::Eq2 | St::Fini => {
                if is_space(src[c]) {
                    c += 1;
                } else {
                    state = state.next();
                }
            }
            St::Error => unreachable!(),
        }
    }
    if state != St::Fini && state != St::Eq2 && !(state == St::Value && quotechar == 0) {
        globals::debug(DPARS, || bcat!("load_env, not an env var, state = ", (state as i32).to_string(), "\n"));
        f.seek(filepos);
        globals::set_line_number(fileline);
        return LoadEnv::NotEnv;
    }
    if state == St::Value {
        // End of unquoted value: trim trailing whitespace.
        while out.len() > val && is_space(out[out.len() - 1]) {
            out.pop();
        }
    }
    LoadEnv::Env(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(line: &str) -> LoadEnv {
        let mut r = Reader::new(format!("{line}\n").into_bytes());
        load_env(&mut r)
    }

    #[test]
    fn env_lines() {
        assert_eq!(env("A=b"), LoadEnv::Env(b"A=b".to_vec()));
        assert_eq!(env("A = b c  "), LoadEnv::Env(b"A=b c".to_vec()));
        // Leading blanks end the (empty) name; skip_comments() normally eats them.
        assert_eq!(env("  A=b"), LoadEnv::NotEnv);
        assert_eq!(env("\"A B\"='x y'  "), LoadEnv::Env(b"A B=x y".to_vec()));
        assert_eq!(env("A="), LoadEnv::Env(b"A=".to_vec()));
        assert_eq!(env("* * * * * cmd"), LoadEnv::NotEnv);
        assert_eq!(env("A='unterminated"), LoadEnv::NotEnv);
    }

    #[test]
    fn glue() {
        assert_eq!(glue_strings(100, b"/a/", b"b", b'/').unwrap(), b"/a/b");
        assert_eq!(glue_strings(100, b"HOME", b"/x", b'=').unwrap(), b"HOME=/x");
        assert!(glue_strings(6, b"HOME", b"/x", b'=').is_none());
        assert_eq!(glue_strings(8, b"HOME", b"/x", b'=').unwrap(), b"HOME=/x");
    }
}
