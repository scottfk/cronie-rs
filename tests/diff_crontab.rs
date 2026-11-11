//! crontab: C vs Rust, as root and as a user through a setgid crontab copy.

mod common;
use common::{ALICE, BOB, Case, PAM_DENY, check};

const GOOD: &str = "# my jobs\nMAILTO=me\n0 5 * * * echo hi\n@daily echo daily\n";
const BAD: &str = "0 5 * * * fine\n61 * * * * bad minute\n0 0 * * * never reached\n";

fn root(name: &str, args: &[&str]) -> Case {
    Case::new(name, "crontab", args).env("HOME", "@T@/home/root")
}

fn alice(name: &str, args: &[&str]) -> Case {
    Case::new(name, "crontab", args).setgid_crontab(ALICE).env("HOME", "@T@/home/alice").env("USER", "alice").file(
        "etc/cron.deny",
        b"bob\n",
        0o644,
    )
}

#[test]
fn options_and_usage() {
    for (n, a) in [
        ("version", &["-V"][..]),
        ("bad_option", &["-Z"][..]),
        ("two_ops", &["-l", "-r"][..]),
        ("extra_arg", &["-l", "x"][..]),
        ("u_unknown", &["-u", "nosuch", "-l"][..]),
        ("u_after_T", &["-T", "-u", "alice"][..]),
        ("u_then_n", &["-u", "alice", "-n"][..]),
        ("u_then_c", &["-u", "alice", "-c"][..]),
        ("bad_debug", &["-x", "bogus", "-l"][..]),
        ("debug_misc", &["-x", "misc,load", "-l"][..]),
        ("missing_file", &["@T@/nope"][..]),
        ("dir_file", &["@T@/home"][..]),
    ] {
        check(root(&format!("opt_{n}"), a));
    }
    let long = "x".repeat(5000);
    check(root("opt_filename_too_long", &[long.as_str()]));
}

#[test]
fn replace_list_remove_as_root() {
    check(root("root_replace_file", &["@T@/new"]).file("new", GOOD.as_bytes(), 0o644));
    check(root("root_replace_stdin", &["-"]).stdin(GOOD.as_bytes()));
    check(root("root_replace_default_stdin", &[]).stdin(GOOD.as_bytes()));
    check(root("root_replace_bad", &["@T@/new"]).file("new", BAD.as_bytes(), 0o644));
    check(root("root_replace_noeol", &["@T@/new"]).file("new", b"0 5 * * * no newline", 0o644));
    check(root("root_replace_garbage", &["@T@/new"]).file("new", format!("{}\n", "#".repeat(40000)).as_bytes(), 0o644));
    check(root("root_replace_options", &["@T@/new"]).file(
        "new",
        b"-0 1 * * * quiet\n0 2 * * * -n mail-on-error\n0 3 * * * -x bad\n",
        0o644,
    ));
    check(root("root_replace_empty", &["@T@/new"]).file("new", b"", 0o644));
    // With an existing crontab: a backup, in ~/.cache/crontab if ~/.cache exists.
    let existing = |c: Case| c.spool("root", "0 1 * * * old job\n");
    check(existing(root("root_backup_nocache", &["@T@/new"]).file("new", GOOD.as_bytes(), 0o644)));
    check(existing(root("root_backup", &["@T@/new"]).file("new", GOOD.as_bytes(), 0o644).file(
        "home/root/.cache/keep",
        b"",
        0o644,
    )));
    check(existing(
        root("root_backup_xdg", &["@T@/new"])
            .file("new", GOOD.as_bytes(), 0o644)
            .env("XDG_CACHE_HOME", "@T@/xdg")
            .file("xdg/keep", b"", 0o644),
    ));
    check(existing(Case::new("root_backup_nohome", "crontab", &["@T@/new"]).file("new", GOOD.as_bytes(), 0o644)));
    check(existing(root("root_list", &["-l"])));
    check(root("root_list_none", &["-l"]));
    check(existing(root("root_remove", &["-r"]).file("home/root/.cache/keep", b"", 0o644)));
    check(root("root_remove_none", &["-r"]));
    for (n, answer) in [("yes", "y\n"), ("Yes", "Yes please\n"), ("no", "n\n"), ("eof", "")] {
        check(existing(root(&format!("root_remove_ask_{n}"), &["-i", "-r"]).stdin(answer.as_bytes())));
    }
    check(root("root_test_good", &["-T", "@T@/new"]).file("new", GOOD.as_bytes(), 0o644));
    check(root("root_test_bad", &["-T", "@T@/new"]).file("new", BAD.as_bytes(), 0o644));
}

#[test]
fn other_users_as_root() {
    let alice_tab = |c: Case| c.spool("alice", "0 1 * * * alice's job\n");
    check(alice_tab(root("u_list", &["-u", "alice", "-l"])));
    check(alice_tab(root("u_replace", &["-u", "alice", "@T@/new"]).file("new", GOOD.as_bytes(), 0o644).file(
        "home/root/.cache/keep",
        b"",
        0o644,
    )));
    check(alice_tab(root("u_remove", &["-u", "alice", "-r"]).file("home/root/.cache/keep", b"", 0o644)));
    // A non-root user's crontab may not disable logging.
    check(root("u_dont_log", &["-u", "alice", "@T@/new"]).file("new", b"-0 1 * * * quiet\n", 0o644));
}

#[test]
fn cluster_host() {
    check(root("host_set", &["-n", "otherhost"]));
    check(root("host_set_default", &["-n"]));
    check(root("host_get_none", &["-c"]));
    check(root("host_get", &["-c"]).file("spool/crontabs/.cron.hostname", b"somehost\n", 0o644));
    check(root("host_get_noeol", &["-c"]).file("spool/crontabs/.cron.hostname", b"somehost", 0o644));
    let long = "h".repeat(70);
    check(root("host_too_long", &["-n", long.as_str()]));
}

#[test]
fn edit() {
    let editor = |body: &str| format!("#!/bin/sh\n{body}\n");
    let ed = |c: Case, body: &str| c.file("ed.sh", editor(body).as_bytes(), 0o755).env("EDITOR", "@T@/ed.sh");
    check(ed(root("edit_unchanged", &["-e"]).spool("root", "0 1 * * * old\n"), "exit 0"));
    check(ed(root("edit_new", &["-e"]), "echo '0 2 * * * added' >> \"$1\""));
    check(ed(
        root("edit_append", &["-e"]).spool("root", "0 1 * * * old\n").file("home/root/.cache/keep", b"", 0o644),
        "echo '0 2 * * * added' >> \"$1\"",
    ));
    check(ed(root("edit_fails", &["-e"]), "exit 3"));
    check(ed(root("edit_killed", &["-e"]), "kill -TERM $$"));
    check(ed(root("edit_symlink", &["-e"]), "rm \"$1\"; ln -s /dev/null \"$1\""));
    check(ed(root("edit_bad_no", &["-e"]).stdin(b"n\n"), "echo '61 * * * * bad' >> \"$1\""));
    check(ed(root("edit_bad_retry", &["-e"]).stdin(b"x\ny\nn\n"), "echo '61 * * * * bad' >> \"$1\""));
    check(ed(
        root("edit_visual", &["-e"]).env("VISUAL", "@T@/ed.sh").env("EDITOR", "/nonexistent"),
        "echo '0 3 * * * visual' >> \"$1\"",
    ));
    check(ed(
        root("edit_tmpdir", &["-e"]).env("TMPDIR", "@T@/home"),
        "echo \"$1\" > @T@/home/edited-path; echo '0 3 * * * x' >> \"$1\"",
    ));
}

#[test]
fn setgid_users() {
    check(alice("user_replace", &["@T@/new"]).file("new", GOOD.as_bytes(), 0o644));
    check(alice("user_replace_stdin", &["-"]).stdin(GOOD.as_bytes()));
    check(alice("user_list", &["-l"]).file_owned("spool/crontabs/alice", b"0 1 * * * mine\n", 0o600, 1001));
    check(alice("user_remove", &["-r"]).file_owned("spool/crontabs/alice", b"0 1 * * * mine\n", 0o600, 1001));
    check(alice("user_dont_log", &["@T@/new"]).file("new", b"-0 1 * * * quiet\n", 0o644));
    check(alice("user_u", &["-u", "bob", "-l"]));
    check(alice("user_n", &["-n", "host"]));
    check(alice("user_c", &["-c"]));
    check(alice("user_unreadable_file", &["@T@/secret"]).file("secret", GOOD.as_bytes(), 0o600));
    check(
        alice("user_tmpdir_ignored", &["-e"])
            .env("TMPDIR", "@T@/home")
            .file("ed.sh", b"#!/bin/sh\necho \"$1\" > /tmp/edited-path\necho '0 2 * * * x' >> \"$1\"\n", 0o755)
            .env("EDITOR", "@T@/ed.sh"),
    );
    check(Case::new("bob_list", "crontab", &["-l"]).setgid_crontab(BOB).file("etc/cron.allow", b"bob\n", 0o644));
}

#[test]
fn allow_and_deny() {
    let base = |n: &str| Case::new(n, "crontab", &["-l"]).setgid_crontab(ALICE);
    check(base("access_no_files"));
    check(base("access_deny_other").file("etc/cron.deny", b"bob\n", 0o644));
    check(base("access_deny_alice").file("etc/cron.deny", b"bob\nalice\n", 0o644));
    check(base("access_deny_alice_noeol").file("etc/cron.deny", b"bob\nalice", 0o644));
    check(base("access_deny_noeol_first").file("etc/cron.deny", b"alice\nbob", 0o644));
    check(base("access_allow_alice").file("etc/cron.allow", b"alice\n", 0o644));
    check(base("access_allow_alice_noeol").file("etc/cron.allow", b"alice", 0o644));
    check(base("access_allow_other").file("etc/cron.allow", b"bob\n", 0o644).file("etc/cron.deny", b"", 0o644));
    check(base("access_allow_unreadable").file("etc/cron.allow", b"alice\n", 0o600));
    check(base("access_pam_deny").file("etc/cron.deny", b"", 0o644).file(
        "etc/pam.d/crond",
        PAM_DENY.as_bytes(),
        0o644,
    ));
}
