# Contributing

Thanks for stopping by. This is a personal project, a Rust port of
[cronie](https://github.com/cronie-crond/cronie) that I tinker with in my spare time, but
contributions are welcome. Here's how things work.

## What this project is

cronie-rs aims to be a **drop-in replacement** for C cronie. Given the same arguments,
crontabs, environment and clock, it should produce the same output, exit statuses, spool
files and job runs, down to the byte and the minute. It is not a redesign: if upstream
has an odd edge case, we copy the odd edge case and list it in QUIRKS.md.

The upstream release we track is pinned in `tools/REFERENCE_TAG`. Behavior is checked
against a build of that tag, not against the man pages or against what seems sensible.

The layout follows upstream's sources (`src/` for crond, crontab and cronnext;
`anacron/` for anacron); the README describes it as it grows.

## What's useful to contribute

- **Behavior differences.** If cronie-rs does something different from C cronie at the
  pinned tag, that's a bug. Please send the crontab, the command, the time it happened
  (or the clock setting), and both outputs. A new differential test case is even better.
- **Quirk fixes, as separate changes.** Fixing something listed in QUIRKS.md is welcome,
  but as its own PR that says which quirk it fixes. Ideally it gets fixed upstream too.
- **Reference bumps.** Moving `tools/REFERENCE_TAG` to a newer upstream release, plus the
  fixes needed so the differential tests pass again.
- **Documentation.** Corrections or additions, especially install notes for different
  distros and init systems.

## What doesn't fit

- **"Improvements" that change behavior quietly.** New crontab syntax, changed defaults
  and nicer messages belong upstream in cronie first. Once they ship in a tagged release,
  we'll match them.
- **New dependencies** without a strong reason. The only crate is `libc`, and I'd like to
  keep it that way. PAM is the same system library upstream links.
- **`unsafe` outside the `sys` module.** If you need a new syscall, add a safe wrapper
  there.

## Building and testing

You need Rust 1.88 or newer (edition 2024), a C compiler, and PAM headers and
libraries. For the differential tests you also need the tools to build upstream cronie
(`git`, `autoconf`, `automake`, `pkg-config`), unprivileged user namespaces, and a
subordinate id range of at least 65536 for your user in `/etc/subuid` and
`/etc/subgid` (with `newuidmap`/`newgidmap` from shadow).

```sh
make                # release build
make reference      # build the C reference at the pinned tag into ../cronie
make check          # build the reference if needed and run everything
make clippy         # cargo clippy --all-targets -D warnings
make fmt            # cargo fmt
```

**Before sending a PR, please make sure `make check`, `make clippy` and `make fmt` are
clean.**

### Differential tests

Most testing is done by running the C reference and cronie-rs side by side on the same
input, the way [metalog-rs](https://codeberg.org/scottfk/metalog-rs) does it:

- Each run is in a private user and mount namespace with real users mapped from your
  subordinate ids, and its own directory under `target/difftest/<case>/{c,rust}/`.
  `/etc` there is the harness's (test users, PAM config, cron.allow/deny).
- The clock comes from an LD_PRELOAD shim, so "when does this job run" is deterministic,
  across DST changes and clock jumps.
- The exit status, stdout, stderr, every file written and the jobs that ran are compared.

Cases can run as root or as test users, and `crontab` cases through a really setgid copy,
so permission checks are tested without real root.

When you fix a difference or change behavior, add cases that cover it, including error
paths and odd input. Unit tests are fine for internal helpers, but a differential test is
what shows the behavior matches.

## Code conventions

- **Port, don't paraphrase.** Each function's doc comment names the upstream function it
  ports (e.g. `` `load_entry()` ``). Keep the structure close enough to the C that
  someone can read the two side by side.
- **Bytes, not strings.** Crontab lines, paths, user names and environment values are
  `Vec<u8>`/`&[u8]`, as they are in C. Don't round-trip through `String` where non-UTF-8
  input could show up.
- **Mark quirks.** Reproduced upstream bugs get a `QUIRK:` comment in the code and a row
  in QUIRKS.md.
- **Every `unsafe` block gets a `// SAFETY:` comment.** The lints enforce this
  (`undocumented_unsafe_blocks`, `unsafe_op_in_unsafe_fn`).
- **US-English spelling** in comments, docs and identifiers, except where a name or
  string must match upstream.
- **Script headers.** Shell scripts, C shims and the Makefile have a changelog comment at
  the top, one line per change: `YYYYMMDD - <description> - <author>`. Please add a line
  when you change one.

## How to submit changes

The repo lives at <https://codeberg.org/scottfk/cronie-rs>.

1. Fork the repo and create a branch.
2. Make your change, with tests.
3. In the PR description, explain **what** changed and **why**. For a behavior fix,
   include the input that showed the difference.
4. If you bumped or tested against a different upstream tag, say which one.

There's no CLA. Contributions are accepted under the project's licenses (see
[LICENSE](LICENSE) and [COPYING.anacron](COPYING.anacron)), the same as upstream
cronie's: a ported file keeps the license of the upstream file it ports.

## Issues

You can open issues for questions, not just bugs. If something about the build or the
reference setup is confusing, ask. I can't promise a fast response, but I do read them.

## Code of conduct

Be straightforward and respectful. That's the whole policy. See
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
