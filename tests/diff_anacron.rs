//! anacron: C vs Rust.  Runs in the foreground (`-d`) as PID 1 with
//! virtual time (the shim's alarm() moves the clock), from 2025-11-11
//! 14:00:30 UTC; timestamps are in `T/anacron` (bound over
//! /var/spool/anacron), the anacrontab in `T/etc/anacrontab`.

mod common;
use common::{Case, check};

const START: i64 = common::DEFAULT_TIME.0 + 30;

fn anacron(name: &str, args: &[&str], tab: &str) -> Case {
    Case::new(name, "anacron", args).time(START).virtual_pid1().file("etc/anacrontab", tab.as_bytes(), 0o644)
}

const TAB: &str = "\
SHELL=/bin/sh
PATH=/usr/bin:/bin
MAILTO=root
1 5 daily echo daily job; echo $FOO
7 10 weekly echo weekly >> @T@/out/weekly
@monthly 15 monthly echo monthly
FOO=set-later
30 2 thirty true
";

#[test]
fn options() {
    for (n, a) in [("version", &["-V"][..]), ("help", &["-h"][..]), ("bad", &["-Z"][..]), ("missing_arg", &["-t"][..])]
    {
        check(anacron(&format!("ana_{n}"), a, TAB));
    }
    check(anacron("ana_test_ok", &["-T"], TAB));
    check(anacron(
        "ana_test_bad",
        &["-T"],
        "1 x bad\nnot a line\n99999999999 1 big true\n@fortnightly 1 odd true\nSTART_HOURS_RANGE=9-3\nPREFERRED_HOUR=30\nRANDOM_DELAY=x\n",
    ));
    check(anacron("ana_missing_tab", &["-d", "-t", "@T@/nope"], TAB));
    check(anacron("ana_missing_spool", &["-d", "-S", "@T@/nope"], TAB));
}

#[test]
fn runs() {
    check(anacron("ana_first_run", &["-d"], TAB));
    check(anacron("ana_now", &["-d", "-n"], TAB));
    check(anacron("ana_serial", &["-d", "-s"], TAB));
    check(anacron("ana_quiet", &["-d", "-q"], TAB));
    check(anacron("ana_update", &["-u"], TAB));
    check(anacron("ana_force", &["-d", "-f", "-n"], TAB).file("anacron/daily", b"20251111\n", 0o600));
    check(anacron("ana_select", &["-d", "-n", "week*", "nomatch"], TAB));
    // Timestamps: done today, yesterday, long ago, in the future, garbage.
    let stamps = |c: Case| {
        c.file("anacron/daily", b"20251111\n", 0o600)
            .file("anacron/weekly", b"20251106\n", 0o600)
            .file("anacron/monthly", b"20251011\n", 0o600)
            .file("anacron/thirty", b"20991231\n", 0o600)
    };
    check(stamps(anacron("ana_stamps", &["-d", "-n"], TAB)));
    check(anacron("ana_stamp_garbage", &["-d", "-n"], TAB).file("anacron/daily", b"garbage!\n", 0o600).file(
        "anacron/weekly",
        b"2025",
        0o600,
    ));
}

#[test]
fn env_and_mail() {
    let tab = "\
MAILTO=
1 1 quiet echo not mailed
MAILTO=bob
1 2 tobob echo to bob; exit 4
NO_MAIL_OUTPUT=1
1 3 nomail echo no mail at all
NO_MAIL_OUTPUT=
MAILFROM=anacron@example.com
1 4 from echo from someone
1 5 silent true
1 6 killed kill -9 $$
";
    check(anacron("ana_mail", &["-d"], tab));
    check(anacron("ana_mail_serial", &["-d", "-s"], tab));
}

#[test]
fn ranges_and_delays() {
    check(anacron("ana_range_in", &["-d"], "START_HOURS_RANGE=10-20\n1 5 inrange true\n"));
    check(anacron("ana_range_out", &["-d"], "START_HOURS_RANGE=3-9\n1 5 outrange true\n"));
    check(anacron("ana_pref_hour", &["-d"], "PREFERRED_HOUR=14\n1 5 at14 true\nPREFERRED_HOUR=3\n1 6 at3 true\n"));
    check(anacron("ana_random", &["-d"], "RANDOM_DELAY=45\n1 5 a true\n1 10 b true\n"));
    check(anacron("ana_long_delay", &["-d"], "1 2000 late true\n"));
    // Same delay: without -s the jobs would finish in any order.
    check(
        anacron(
            "ana_named",
            &["-d", "-s"],
            "@daily 1 d true\n@weekly 1 w true\n@monthly 1 m true\n@yearly 1 y true\n@annually 1 a true\n",
        )
        .file("anacron/w", b"20251104\n", 0o600)
        .file("anacron/m", b"20251011\n", 0o600),
    );
}
