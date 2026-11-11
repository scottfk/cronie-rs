//! Port of the parts of src/security.c that crontab uses: `allowed()`
//! (cron.allow / cron.deny) and the PAM account check.
//!
//! Upstream license: ISC (see LICENSE).

use std::cell::RefCell;

use crate::globals::log_it;
use crate::sys::{self, PAM_ESTABLISH_CRED, PAM_SILENT, PAM_SUCCESS, Pam, Passwd};

pub const CRON_ALLOW: &[u8] = b"/etc/cron.allow";
pub const CRON_DENY: &[u8] = b"/etc/cron.deny";
const MAX_TEMPSTR: usize = 131_072;

/// `in_file(string, file, error)`: is `string` a line of the file?  Any
/// line that doesn't end in a newline (including the last one) makes the
/// answer `error`.
///
/// QUIRK: so a name on the final, unterminated line is never found, and
/// with `error = FALSE` a cron.deny listing a user on that line lets them
/// in (while cron.allow keeps them out).
fn in_file(string: &[u8], data: &[u8], error: bool) -> bool {
    // fgets(line, MAX_TEMPSTR, file): lines of at most MAX_TEMPSTR-1 bytes.
    let mut rest = data;
    while !rest.is_empty() {
        let take = match rest[..rest.len().min(MAX_TEMPSTR - 1)].iter().position(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => rest.len().min(MAX_TEMPSTR - 1),
        };
        let line = crate::bytes::c_str_prefix(&rest[..take]);
        rest = &rest[take..];
        if !line.is_empty() {
            if line[line.len() - 1] != b'\n' {
                return error;
            }
            if &line[..line.len() - 1] == string {
                return true;
            }
        }
    }
    false
}

/// `allowed(username, allow_file, deny_file)`: root always; otherwise the
/// user must be in cron.allow, or (without one) not in cron.deny.  With
/// neither file, only root is allowed.
pub fn allowed(username: &[u8], allow_file: &[u8], deny_file: &[u8]) -> bool {
    if sys::getuid() == 0 {
        return true;
    }
    let read = |p: &[u8]| {
        sys::open(p, libc::O_RDONLY, 0).ok().map(|fd| {
            let d = sys::read_to_end(fd);
            sys::close(fd);
            d
        })
    };
    if let Some(data) = read(allow_file) {
        in_file(username, &data, false)
    } else if let Some(data) = read(deny_file) {
        !in_file(username, &data, false)
    } else {
        false
    }
}

thread_local! {
    static PAMH: RefCell<Option<Pam>> = const { RefCell::new(None) };
}

fn pam_message(msg: &[u8]) {
    log_it(b"CRON", sys::getpid(), b"pam_message", msg, 0);
}

/// `cron_start_pam(pw)`: the PAM account check for service "crond".
pub fn cron_start_pam(pw: &Passwd) -> i32 {
    sys::set_pam_log(pam_message);
    let fail = |pam: Option<Pam>, code: i32| {
        let text = pam.as_ref().map(|p| p.strerror(code)).unwrap_or_default();
        log_it(&pw.name, sys::getpid(), b"PAM ERROR", &text, 0);
        if let Some(p) = pam {
            p.end(code);
        }
        code
    };
    let pam = match Pam::start(b"crond", &pw.name) {
        Ok(p) => p,
        Err((p, code)) => return fail(p, code),
    };
    for step in 0..3 {
        let code = match step {
            0 => pam.set_tty(c"cron"),
            1 => pam.acct_mgmt(PAM_SILENT),
            _ => pam.setcred(PAM_ESTABLISH_CRED | PAM_SILENT),
        };
        if code != PAM_SUCCESS {
            return fail(Some(pam), code);
        }
    }
    PAMH.with(|h| *h.borrow_mut() = Some(pam));
    PAM_SUCCESS
}

/// `cron_close_pam()` (crontab never opens a session).
pub fn cron_close_pam() {
    if let Some(p) = PAMH.with(|h| h.borrow_mut().take()) {
        p.end(PAM_SUCCESS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_file_rules() {
        assert!(in_file(b"bob", b"alice\nbob\n", false));
        assert!(!in_file(b"carol", b"alice\nbob\n", false));
        assert!(!in_file(b"bob", b"alice\nbob", false));
        assert!(in_file(b"x", b"alice\nbob", true));
        assert!(in_file(b"", b"\n", false));
    }
}
