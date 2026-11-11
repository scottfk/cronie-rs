//! Port of the misc.c and cronie_common.c helpers shared by the programs
//! (beyond the parsing ones in [`crate::reader`]).
//!
//! Upstream license: ISC (see LICENSE).

use crate::bcat;
use crate::stdio::{self, err};
use crate::sys;
use crate::user::SPOOL_DIR;

/// `check_spool_dir()` (built without CRON_GROUP: no chown/chmod).
pub fn check_spool_dir() {
    let st = match sys::stat(SPOOL_DIR) {
        Err(e) if e == libc::ENOENT => {
            stdio::perror(SPOOL_DIR);
            if sys::mkdir(SPOOL_DIR, 0o700).is_ok() {
                err(&bcat!(SPOOL_DIR, ": created\n"));
                match sys::stat(SPOOL_DIR) {
                    Ok(st) => Some(st),
                    Err(_) => {
                        stdio::perror(b"stat retry");
                        stdio::exit(1);
                    }
                }
            } else {
                err(&bcat!(SPOOL_DIR, ": "));
                stdio::perror(b"mkdir");
                stdio::exit(1);
            }
        }
        // C then tests an uninitialized struct stat; treat it as "not a
        // directory".
        Err(_) => None,
        Ok(st) => Some(st),
    };
    if !st.is_some_and(|s| sys::is_dir(s.mode)) {
        err(&bcat!("'", SPOOL_DIR, "' is not a directory, bailing out.\n"));
        stdio::exit(1);
    }
}

/// `mkprints()`: control characters as `^X`, DEL as `^?`, high bytes as
/// `\ooo`.
pub fn mkprints(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    for &ch in src {
        if ch < b' ' {
            out.push(b'^');
            out.push(ch + b'@');
        } else if ch < 0o177 {
            out.push(ch);
        } else if ch == 0o177 {
            out.extend_from_slice(b"^?");
        } else {
            out.extend_from_slice(format!("\\{ch:03o}").as_bytes());
        }
    }
    out
}

/// `first_word(s, t)`: the first run of characters not in `t`.
pub fn first_word(s: &[u8], t: &[u8]) -> Vec<u8> {
    const MAX_TEMPSTR: usize = 131_072;
    let start = s.iter().position(|b| !t.contains(b)).unwrap_or(s.len());
    s[start..].iter().take_while(|b| !t.contains(b)).take(MAX_TEMPSTR).copied().collect()
}

/// `find_envvar()`: where the first `$NAME` / `${NAME}` is, and its length
/// including `$` and braces.
fn find_envvar(source: &[u8]) -> Option<(usize, usize)> {
    let start = source.iter().position(|&b| b == b'$')?;
    let mut size = 1usize;
    let mut waiting_close = false;
    let mut i = start + 1;
    while i < source.len() {
        let c = source[i];
        if c == b'_' || c.is_ascii_alphanumeric() {
            if size <= 2 && c.is_ascii_digit() {
                return None;
            }
            size += 1;
        } else if c == b'{' {
            if size != 1 {
                return None;
            }
            size += 1;
            waiting_close = true;
        } else if c == b'}' {
            if (waiting_close && size == 2) || size == 1 {
                return None;
            }
            if waiting_close {
                size += 1;
            }
            waiting_close = false;
            break;
        } else {
            break;
        }
        i += 1;
    }
    if waiting_close {
        return None;
    }
    Some((start, size))
}

/// `expand_envvar()`: expand `$NAME` and `${NAME}` from the process
/// environment.  `None` if the result needs `max_size` bytes or more.
///
/// QUIRK: the values come from the calling process (crond), not the job;
/// an unset variable expands to nothing, and a `$` that doesn't
/// start a valid name (`$1`, `$`, `${`) stops expansion: the rest is copied
/// as is.
pub fn expand_envvar(source: &[u8], max_size: usize) -> Option<Vec<u8>> {
    let mut result: Vec<u8> = Vec::new();
    let mut src = source;
    while let Some((start, size)) = (!src.is_empty()).then(|| find_envvar(src)).flatten() {
        if start > 0 {
            if result.len() + start + 1 > max_size {
                return None;
            }
            result.extend_from_slice(&src[..start]);
        }
        let mut name = &src[start + 1..start + size];
        if name.first() == Some(&b'{') {
            name = &name[1..name.len() - 1];
        }
        src = &src[start + size..];
        if let Some(v) = sys::getenv(name) {
            if result.len() + v.len() + 1 > max_size {
                return None;
            }
            result.extend_from_slice(&v);
        }
    }
    if !src.is_empty() {
        if result.len() + src.len() + 1 > max_size {
            return None;
        }
        result.extend_from_slice(src);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints() {
        assert_eq!(mkprints(b"a\tb\x7f\xe9"), b"a^Ib^?\\351");
        assert_eq!(first_word(b"..host.example", b"."), b"host");
    }

    #[test]
    fn envvars() {
        assert_eq!(find_envvar(b"x$HOME/y"), Some((1, 5)));
        assert_eq!(find_envvar(b"${HOME}"), Some((0, 7)));
        assert_eq!(find_envvar(b"$1x"), None);
        assert_eq!(find_envvar(b"${HOME"), None);
    }
}
