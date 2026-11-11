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
    static PAM_SESSION_OPENED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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

/// `PAM_FAIL_CHECK`'s cleanup: log, close an open session, end.
fn pam_fail(pw: &Passwd, code: i32) -> i32 {
    let p = PAMH.with(|h| h.borrow_mut().take());
    let text = p.as_ref().map_or_else(|| Pam::strerror_static(code), |p| p.strerror(code));
    log_it(&pw.name, sys::getpid(), b"PAM ERROR", &text, 0);
    if let Some(p) = p {
        if PAM_SESSION_OPENED.with(std::cell::Cell::get) {
            p.close_session(PAM_SILENT);
        }
        p.end(code);
    }
    code
}

/// `cron_open_pam_session()`.
fn cron_open_pam_session(pw: &Passwd) -> i32 {
    let code = PAMH.with(|h| h.borrow().as_ref().map_or(PAM_SUCCESS, |p| p.open_session(PAM_SILENT)));
    if code != PAM_SUCCESS {
        return pam_fail(pw, code);
    }
    PAM_SESSION_OPENED.with(|c| c.set(true));
    code
}

/// `cron_close_pam()`.
pub fn cron_close_pam() {
    let p = PAMH.with(|h| h.borrow_mut().take());
    if PAM_SESSION_OPENED.with(std::cell::Cell::get)
        && let Some(p) = &p
    {
        p.setcred(sys::PAM_DELETE_CRED | PAM_SILENT);
        p.close_session(PAM_SILENT);
    }
    if let Some(p) = p {
        p.end(PAM_SUCCESS);
    }
}

/// `cron_change_groups()`: setgid, initgroups, and PAM's group credentials.
pub fn cron_change_groups(pw: &Passwd) -> i32 {
    let pid = sys::getpid();
    if let Err(e) = sys::setgid(pw.gid) {
        log_it(b"CRON", pid, b"ERROR", b"setgid failed", e);
        return -1;
    }
    if let Err(e) = sys::initgroups(&pw.name, pw.gid) {
        log_it(b"CRON", pid, b"ERROR", b"initgroups failed", e);
        return -1;
    }
    PAMH.with(|h| {
        if let Some(p) = h.borrow().as_ref() {
            p.setcred(sys::PAM_REINITIALIZE_CRED | PAM_SILENT);
        }
    });
    0
}

/// `cron_change_user_permanently()`: setreuid to the user, chdir home.
pub fn cron_change_user_permanently(pw: &Passwd, homedir: Option<&[u8]>) -> i32 {
    if let Err(e) = sys::setreuid(pw.uid, pw.uid) {
        log_it(b"CRON", sys::getpid(), b"ERROR", b"setreuid failed", e);
        return -1;
    }
    if let Err(e) = sys::chdir(homedir) {
        log_it(b"CRON", sys::getpid(), b"ERROR chdir failed", homedir.unwrap_or(b"(null)"), e);
        return -1;
    }
    crate::globals::log_close();
    0
}

/// `build_env()`: PAM's environment with the crontab's on top, or just a
/// copy of the crontab's.
fn build_env(cronenv: &crate::env::Env) -> crate::env::Env {
    let pam_env = PAMH.with(|h| h.borrow().as_ref().and_then(Pam::getenvlist));
    match pam_env {
        None => cronenv.clone(),
        Some(mut env) => {
            for v in cronenv {
                crate::env::env_set(&mut env, v);
            }
            env
        }
    }
}

/// `cron_set_job_security_context()`: PAM, groups, and the job's
/// environment.  `None` means the job must not run.
pub fn cron_set_job_security_context(e: &crate::entry::Entry, system: bool) -> Option<crate::env::Env> {
    let mut minutely_time = 0;
    if e.flags & crate::entry::MIN_STAR == crate::entry::MIN_STAR {
        minutely_time = sys::time_now();
        crate::globals::debug(crate::globals::DSCH, || {
            format!("Minute-ly job. Recording time {minutely_time}\n").into_bytes()
        });
    }
    if !system || e.pwd.uid != 0 {
        let ret = cron_start_pam(&e.pwd);
        if ret != 0 {
            log_it(&e.pwd.name, sys::getpid(), b"FAILED to authorize user with PAM", &Pam::strerror_static(ret), 0);
            return None;
        }
    }
    if PAMH.with(|h| h.borrow().is_some()) {
        let ret = cron_open_pam_session(&e.pwd);
        if ret != 0 {
            log_it(&e.pwd.name, sys::getpid(), b"FAILED to open PAM security session", &Pam::strerror_static(ret), 0);
            return None;
        }
    }
    if cron_change_groups(&e.pwd) != 0 {
        return None;
    }
    let jobenv = build_env(&e.envp);
    let job_run_time = sys::time_now();
    if minutely_time > 0 && job_run_time / 60 != minutely_time / 60 {
        let n = sys::localtime_r(job_run_time);
        let s = sys::localtime_r(minutely_time);
        let msg = format!(
            "Job execution of per-minute job scheduled for {:02}:{:02} delayed into subsequent minute {:02}:{:02}. Skipping job run.",
            s.tm_hour, s.tm_min, n.tm_hour, n.tm_min
        );
        log_it(&e.pwd.name, sys::getpid(), b"INFO", msg.as_bytes(), 0);
        return None;
    }
    Some(jobenv)
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
