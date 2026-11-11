//! cronnext, and with it the crontab parser and the database loader: C vs Rust.

mod common;
use common::{Case, check};

fn file_case(name: &str, tab: &str, args: &[&str]) -> Case {
    let mut a: Vec<&str> = args.to_vec();
    a.push("@T@/tab");
    Case::new(name, "cronnext", &a).file("tab", tab.as_bytes(), 0o644)
}

const BASIC: &str = "\
# comment
SHELL=/bin/bash
MAILTO=\"me@example.com\"
*/15 * * * * echo quarter
0 9-17/2 * Jan-Mar mon-fri echo work
@daily echo daily
30 4 1,15 * 5 echo dom-or-dow
0 0 * * 0 echo sunday
0 0 * * 7 echo sunday7
";

#[test]
fn options() {
    check(Case::new("version", "cronnext", &["-V"]));
    check(Case::new("help", "cronnext", &["-h"]));
    check(Case::new("bad_option", "cronnext", &["-x"]));
    check(Case::new("missing_arg", "cronnext", &["-t"]));
    check(Case::new("clustered_bad", "cronnext", &["-lZ"]));
    check(Case::new("f_without_q", "cronnext", &["-f", "x"]));
    check(Case::new("missing_file", "cronnext", &["@T@/nope"]));
    check(Case::new("directory_arg", "cronnext", &["@T@/etc/cron.d"]));
}

#[test]
fn listing_modes() {
    for (n, args) in [
        ("plain", &[][..]),
        ("c", &["-c"][..]),
        ("l", &["-l"][..]),
        ("cl", &["-c", "-l"][..]),
        ("f", &["-f", "-q", "1763000000"][..]),
        ("t", &["-t", "1700000000"][..]),
        ("t_odd", &["-t", "1762869601", "-c"][..]),
        ("q_past", &["-q", "1"][..]),
        ("j", &["-j", "echo d", "-c"][..]),
        ("j_none", &["-j", "nothing-matches"][..]),
    ] {
        check(file_case(&format!("mode_{n}"), BASIC, args));
    }
}

#[test]
fn syntax() {
    let tabs = [
        ("names", "0 0 * jan,MAR,dec SUN-wed x\n0 0 * Feb * y\n"),
        ("steps", "*/7 */5 */10 */3 */2 x\n1-50/9 * * * * y\n"),
        ("random", "~ ~ * * * a\n5~20 3~ * * * b\n~30 * * * * c\n10~5 * * * * d\n"),
        (
            "specials",
            "@reboot r\n@yearly y\n@annually a\n@monthly m\n@weekly w\n@midnight n\n@hourly h\n@bogus x\n@daily\n",
        ),
        ("bad_fields", "61 * * * * a\n* 24 * * * b\n* * 0 * * c\n* * * 13 * d\n* * * * 8 e\n* * * * x f\n"),
        ("bad_shapes", "1- * * * * a\n*/0 * * * * b\n1-5/ * * * * c\n1,,2 * * * * d\n* * * * *\n* * * * * *x\n"),
        ("step_warning", "*/90 * * * * a\n1-3/5 * * * * b\n"),
        ("options", "0 0 * * * -n x\n0 0 * * * -n -n y\n0 0 * * * -x z\n0 0 * * * -nfoo w\n0 0 * * * -n\n"),
        ("dont_log", "-0 0 * * * quiet\n-\n"),
        ("no_newline", "0 0 * * * last"),
        ("blank_and_comments", "\n\n   # indented comment\n\t\n0 0 * * * x\n#tail"),
        ("env_quirks", "A = b\n'B'=\"c d\"\nC=\nD='unterminated\n E=leading blank\n0 0 * * * x\n"),
        ("crlf", "0 0 * * * x\r\n1 1 * * * y\r\n"),
        ("long_cmd", &format!("0 0 * * * {}\n", "z".repeat(5000))),
        ("dom_dow", "0 12 13 * 5 friday-or-13th\n0 12 * * 5 every-friday\n0 12 13 * * every-13th\n"),
        ("leap", "0 0 29 2 * leap\n0 0 31 4 * never\n"),
        ("random_delay", "RANDOM_DELAY=30\n0 0 * * * x\nRANDOM_DELAY=bad\n1 1 * * * y\n"),
    ];
    for (name, tab) in tabs {
        check(file_case(&format!("syntax_{name}"), tab, &["-c"]));
        check(file_case(&format!("syntax_{name}_f"), tab, &["-f", "-q", "1765000000"]));
    }
}

#[test]
fn time_zones() {
    // DST changes: Europe/Berlin goes back on 2025-10-26 and forward on
    // 2026-03-29; America/St_Johns has a half-hour offset.
    let tab = "30 2 * * * at-0230\n0 * * * * hourly\n*/30 1-3 * * * night\n";
    for (n, tz, t) in [
        ("berlin_fall", "Europe/Berlin", "1761400000"),
        ("berlin_spring", "Europe/Berlin", "1774656000"),
        ("st_johns", "America/St_Johns", "1762869600"),
        ("posix_tz", "EST5EDT", "1762869600"),
    ] {
        check(
            file_case(
                &format!("tz_{n}"),
                tab,
                &["-f", "-t", t, "-q", &(t.parse::<i64>().unwrap() + 2 * 86400).to_string()],
            )
            .env("TZ", tz),
        );
    }
}

#[test]
fn installed_database() {
    let user_tab = "0 5 * * * user job\n";
    let sys_tab =
        "SHELL=/bin/sh\n0 6 * * * root system job\n0 7 * * * nosuchuser orphan job\n15 * * * * nobody nobody job\n";
    let base = || {
        Case::new("x", "cronnext", &["-c"])
            .spool("root", user_tab)
            .spool("nobody", "0 8 * * * nobody's job\n")
            .file("etc/crontab", sys_tab.as_bytes(), 0o644)
            .file("etc/cron.d/extra", b"0 9 * * * root cron.d job\n", 0o644)
    };
    check(Case { name: "db_all".into(), ..base() });
    check(Case { name: "db_no_system".into(), args: vec!["-c".into(), "-s".into()], ..base() });
    check(Case { name: "db_include".into(), args: vec!["-c".into(), "-i".into(), "nobody,*system*".into()], ..base() });
    check(Case { name: "db_exclude".into(), args: vec!["-c".into(), "-e".into(), "root".into()], ..base() });
    check(Case {
        name: "db_plus_file".into(),
        args: vec!["-c".into(), "-a".into(), "@T@/tab".into()],
        ..base().file("tab", b"0 10 * * * file job\n", 0o644)
    });
    // Files that load_database rejects or skips.
    check(
        Case::new("db_rejects", "cronnext", &["-c"])
            .file("spool/crontabs/root", b"0 1 * * * bad mode\n", 0o644)
            .spool("nosuchuser", "0 2 * * * orphan\n")
            .spool(".hidden", "0 3 * * * hidden\n")
            .spool("#temp", "0 3 * * * temp\n")
            .spool("nobody~", "0 3 * * * backup\n")
            .spool("x.rpmsave", "0 3 * * * rpm\n")
            .file("etc/cron.d/notregular", b"", 0o644)
            .file("etc/cron.d/exec", b"0 4 * * * root executable\n", 0o755)
            .file("etc/cron.d/.dot", b"0 4 * * * root dot\n", 0o644),
    );
    check(Case::new("db_garbage", "cronnext", &["-c"]).spool("root", &format!("{}\n0 0 * * * x\n", "#".repeat(40000))));
    check(Case::new("db_empty", "cronnext", &[]));
}
