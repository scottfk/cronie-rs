# cronie-rs

 A drop-in port/reimplementation of cronie in rust. Currently just something fun for me to tinker with in Claude.

## Goals

- **Drop-in compatible with cronie 1.7.2** (Gentoo's `sys-process/cronie`):
  `crond`, `crontab`, `cronnext` and `anacron`, with the same options,
  messages and exit statuses, the same crontab and anacrontab syntax, the
  same spool files, and jobs run at the same times with the same
  environment. Upstream quirks are reproduced on purpose and listed in
  [QUIRKS.md](QUIRKS.md). Improvements come later, as deliberate changes.
- **Linux only.**
- **Small and boring:** `libc` as the only crate dependency, the same system
  libraries upstream links (PAM), no async, and `unsafe` confined to one
  `sys` module.

## Status

Phase 1 is done: the crontab parser, the database loader and `cronnext`.
The reference is upstream's tag `cronie-1.7.2`, built as Gentoo builds it
([tools/build-reference.sh](tools/build-reference.sh)):

- USE="anacron inotify pam" and `--enable-syscrontab`;
- the spool in `/var/spool/cron/crontabs`;
- the daemon user and group `cron`.

## Layout

One library crate, with what upstream's programs share (upstream links the
same object files into each of them), and a binary per program in
`src/bin/`:

| Path | What |
|---|---|
| `src/entry.rs` | `entry.c`: one crontab line (time fields, `@` specials, `~` random ranges, options) |
| `src/env.rs` | `env.c`: `NAME=value` lines and job environments |
| `src/user.rs` | `user.c` and `database.c`'s loader: crontab files, `/etc/crontab`, `/etc/cron.d`, the spool |
| `src/reader.rs` | stdio input with glibc's `ungetc`/`ftell`/EOF rules, and misc.c's `get_char()` family |
| `src/globals.rs` | globals.h, `log_it()` (syslog), debug flags |
| `src/random.rs` | glibc's `random()`, for `~` ranges and `RANDOM_DELAY` |
| `src/sys.rs` | every libc call: the only `unsafe` code |
| `src/bin/cronnext.rs` | `cronnext.c` |
| `src/stdio.rs`, `getopt.rs`, `num.rs`, `bytes.rs` | glibc behavior the output depends on, copied from metalog-rs/openrc-rs |
| `tests/` | differential tests and the harness ([tests/common/mod.rs](tests/common/mod.rs)) |

## Testing

```sh
make check
```

This builds the C reference into `../cronie` if it isn't there, then runs the
unit tests and the differential suite. Each differential case runs a C program
and its port on the same input and compares the exit status, stdout, stderr,
the syslog messages and every file left behind.

- Each run is "root" in an unprivileged user and mount namespace.
- Test directories are bound over `/var/spool/cron`, `/etc/cron.d`,
  `/etc/crontab` and `/dev/log`, so the installed-crontab code reads test
  data at the real paths.
- The clock is an LD_PRELOAD shim, so schedules are deterministic, DST
  changes included.

To see what each case produced:
`CRONIE_DIFF_SHOW=1 cargo test -- --nocapture`.

## Roadmap

1. **Crontab parsing and `cronnext` (done, branch `phase1-parse`):** the
   entry parser (`entry.c`), the environment lines, the database loader, and
   the next-run calculation.
2. **`crontab`:** list, edit, replace, remove and check (`-l -e -r -T`), the
   `cron.allow`/`cron.deny` rules, and the spool file format. It's setgid
   `crontab`, so this is where memory safety matters most.
3. **`crond`:** the database (`database.c`, inotify), the minute loop and
   clock jumps, running jobs (`do_command.c`, PAM, mail), and `@reboot`.
   Job execution needs real root, so it's tested in the shared VM.
4. **`anacron`:** anacrontab, timestamps, delays and serialized runs.
5. **Packaging:** a Gentoo ebuild (`sys-process/cronie` drop-in), rehearsed
   in the VM.

The testing approach comes from
[metalog-rs](https://codeberg.org/scottfk/metalog-rs): the C reference and
the port run side by side on the same input, in unprivileged user
namespaces, with an LD_PRELOAD clock, and the harness compares everything
they do.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md) and
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## License

The same licenses as upstream cronie, whose files these are:

- **[LICENSE](LICENSE):** upstream's `COPYING`. It covers the cron parts,
  which are ISC (Paul Vixie, ISC) and BSD (University of California).
- **[COPYING.anacron](COPYING.anacron):** GPL-2.0-or-later, for anacron and
  `cronnext`.

Each ported file keeps the license of the upstream file it ports. Taken as a
whole, the project is GPL-2.0-or-later.
