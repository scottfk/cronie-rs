//! crond: C vs Rust, in virtual time (see the harness's daemon mode).
//!
//! crond runs as PID 1 of a PID namespace with `-n`, from 14:00:30 UTC.
//! Jobs write to `@T@/out` (one file per job: jobs of the same minute run
//! concurrently), and mail goes to the fake sendmail.

mod common;
use common::{Case, PAM_DENY, check};

const START: i64 = common::DEFAULT_TIME.0 + 30;

fn crond(name: &str, minutes: i64) -> Case {
    Case::new(name, "crond", &["-n"]).time(START).run_for(minutes)
}

fn with_args(name: &str, args: &[&str], minutes: i64) -> Case {
    let mut a = vec!["-n"];
    a.extend_from_slice(args);
    Case::new(name, "crond", &a).time(START).run_for(minutes)
}

#[test]
fn command_line() {
    for (n, a) in [
        ("version", &["-V"][..]),
        ("help", &["-h"][..]),
        ("bad_option", &["-Z"][..]),
        ("bad_debug", &["-x", "nope"][..]),
    ] {
        check(Case::new(&format!("crond_{n}"), "crond", a));
    }
}

#[test]
fn schedules() {
    check(crond("smoke", 60).spool("root", "*/15 * * * * echo tick >> @T@/out/ticks\n"));
    check(
        crond("users", 120)
            .spool("root", "*/20 * * * * echo root $LOGNAME $HOME $SHELL $PATH >> @T@/out/root\n@hourly id -un >> @T@/out/root-hourly\n")
            .spool("alice", "SHELL=/bin/bash\n5,35 * * * * echo alice $USER $HOME $SHELL $(pwd) >> @T@/out/alice\n")
            .spool("bob", "0 15 * * * echo bob at three >> @T@/out/bob\n0 9 * * * echo never >> @T@/out/bob-never\n"),
    );
    check(
        crond("system_crontab", 90)
            .file("etc/crontab", b"SHELL=/bin/sh\n*/30 * * * * root echo sys-root >> @T@/out/sys-root\n10 * * * * alice echo sys-alice >> @T@/out/sys-alice\n20 * * * * nosuch echo unknown user\n", 0o644)
            .file("etc/cron.d/extra", b"45 * * * * bob echo cron.d >> @T@/out/crond-d\n", 0o644)
            .spool("ghost", "* * * * * echo orphan >> @T@/out/orphan\n"),
    );
    check(
        crond("ranges", 180).spool(
            "root",
            "1-10/3 14,16 * * * echo stepped >> @T@/out/stepped\n0 */2 * * * echo even >> @T@/out/even\n",
        ),
    );
}

#[test]
fn output_and_mail() {
    let tab = "\
MAILTO=alice
* * * * * echo to-alice; echo second line
MAILTO=
1 * * * * echo silent
MAILTO=\"bob@example.com\"
2 * * * * printf 'crlf\\r\\nend\\r\\n'
MAILTO=-bad
3 * * * * echo unsafe recipient
MAILTO=root
MAILFROM=cron-sender@example.com
4 * * * * echo with sender
CONTENT_TYPE=text/html
CONTENT_TRANSFER_ENCODING=base64
5 * * * * echo typed
";
    check(crond("mail", 6).spool("root", tab));
    check(
        crond("mail_n", 3)
            .spool("root", "* * * * * -n echo fine\n1 * * * * -n echo failing; exit 3\n2 * * * * -n -n bad\n"),
    );
    check(crond("mail_long", 2).spool("root", "1 * * * * seq 1 3000\n"));
    check(crond("mail_noshell", 2).spool("root", "SHELL=/nonexistent/sh\n1 * * * * echo x\n"));
    check(with_args("mail_off", &["-m", "off"], 2).spool("root", "1 * * * * echo not mailed\n"));
    check(
        with_args("mail_custom", &["-m", "/bin/sh -c cat>>@T@/out/custom"], 2)
            .spool("root", "1 * * * * echo custom mailer\n"),
    );
    check(
        with_args("syslog_output", &["-s"], 3).spool(
            "root",
            &format!("1 * * * * echo line one; echo line two\n2 * * * * printf '{}'\n", "y".repeat(1500)),
        ),
    );
    check(with_args("mail_cmd_missing", &["-m", "/nonexistent/mailer"], 2).spool("root", "1 * * * * echo x\n"));
}

#[test]
fn input_and_escapes() {
    check(crond("percent", 3).spool(
        "root",
        "1 * * * * cat > @T@/out/input%line one%line two\n2 * * * * cat > @T@/out/escaped%a \\\\% b%\\\\x\n3 * * * * echo 100\\\\% > @T@/out/literal\n",
    ));
    check(crond("dont_log", 2).spool("root", "-1 * * * * echo quiet >> @T@/out/quiet\n"));
}

#[test]
fn reboot_and_delay() {
    check(crond("reboot", 3).spool("root", "@reboot echo booted >> @T@/out/boot\n"));
    check(crond("reboot_locked", 3).spool("root", "@reboot echo booted >> @T@/out/boot\n").file(
        "run/cron.reboot",
        b"",
        0o644,
    ));
    check(crond("random_delay", 90).spool(
        "root",
        "RANDOM_DELAY=30\n0 15 * * * echo delayed >> @T@/out/delayed\nRANDOM_DELAY=5000\n10 14 * * * echo bad delay\n",
    ));
}

#[test]
fn time_zones_and_dst() {
    let tab = "30 2 * * * echo at-0230 >> @T@/out/0230\n*/30 * * * * echo wild >> @T@/out/wild\n15 1 * * * echo at-0115 >> @T@/out/0115\n";
    // Europe/Berlin: spring forward 2026-03-29 02:00 -> 03:00 (01:00 UTC),
    // fall back 2025-10-26 03:00 -> 02:00 (01:00 UTC).
    check(crond("dst_spring", 240).time(1_774_738_830).run_for(240).env("TZ", "Europe/Berlin").spool("root", tab));
    check(crond("dst_fall", 240).time(1_761_433_230).run_for(240).env("TZ", "Europe/Berlin").spool("root", tab));
    check(crond("cron_tz", 120).spool("root", "CRON_TZ=America/New_York\n0 10 * * * echo ten-in-new-york >> @T@/out/ny\nCRON_TZ=Asia/Tokyo\n30 23 * * * echo tokyo >> @T@/out/tokyo\n"));
}

#[test]
fn clock_jumps() {
    // A root job moves the fake clock: crond sees the jump next minute.
    let jump = |secs: i64| {
        format!(
            "2 14 * * * echo $(( $(cut -d' ' -f1 @T@/.faketime) {} {} )) 0 > @T@/.faketime\n",
            if secs < 0 { '-' } else { '+' },
            secs.abs()
        )
    };
    let jobs = "*/5 * * * * echo wild >> @T@/out/wild\n7,13,40 * * * * echo fixed >> @T@/out/fixed\n";
    // small: up to 5 min; medium: up to 3 h (DST-like); large: more.
    for (n, secs) in [("small", 180), ("medium", 1800), ("large", 4 * 3600 + 600), ("back", -3600)] {
        check(crond(&format!("jump_{n}"), 7 * 60).spool("root", &format!("{}{jobs}", jump(secs))));
    }
}

#[test]
fn reloads_and_signals() {
    // A job edits crontabs; crond rereads them (inotify, or mtimes with -i).
    let tab = "3 * * * * echo '* * * * * root echo added >> @T@/out/added' > @T@/etc/cron.d/new\n6 * * * * rm @T@/etc/cron.d/new\n";
    check(crond("reload_inotify", 10).spool("root", tab));
    check(with_args("reload_noinotify", &["-i"], 10).spool("root", tab));
    check(crond("sighup", 5).spool("root", "2 * * * * kill -HUP 1\n* * * * * echo tick >> @T@/out/tick\n"));
    check(with_args("permit_any", &["-p"], 3).file(
        "spool/crontabs/root",
        b"* * * * * echo loose >> @T@/out/loose\n",
        0o666,
    ));
    check(crond("strict_mode", 3).file("spool/crontabs/root", b"* * * * * echo loose >> @T@/out/loose\n", 0o666));
    check(
        with_args("keep_path", &["-P"], 2)
            .env("PATH", "/custom/bin:/usr/bin:/bin")
            .spool("root", "1 * * * * echo $PATH > @T@/out/path\n"),
    );
    check(crond("pam_deny", 3).spool("alice", "* * * * * echo denied >> @T@/out/alice\n").file(
        "etc/pam.d/crond",
        PAM_DENY.as_bytes(),
        0o644,
    ));
    check(with_args("debug_sch", &["-x", "sch,load,misc"], 3).spool("root", "* * * * * true\n"));
}
