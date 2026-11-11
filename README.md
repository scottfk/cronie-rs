# cronie-rs

 A drop-in port/reimplementation of cronie in rust. Currently just something fun for me to tinker with in Claude.

## Goals

- **Drop-in compatible with cronie 1.7.2** (Gentoo's `sys-process/cronie`):
  `crond`, `crontab`, `cronnext` and `anacron`, with the same options,
  messages and exit statuses, the same crontab and anacrontab syntax, the
  same spool files, and jobs run at the same times with the same
  environment. Upstream quirks are reproduced on purpose and will be listed
  in QUIRKS.md. Improvements come later, as deliberate changes.
- **Linux only.**
- **Small and boring:** `libc` as the only crate dependency, the same system
  libraries upstream links (PAM), no async, and `unsafe` confined to one
  `sys` module.

## Status

Just starting. The reference is upstream's tag `cronie-1.7.2`, built as
Gentoo builds it: USE="anacron inotify pam", `--enable-syscrontab`, the
spool in `/var/spool/cron/crontabs`, and the daemon user and group `cron`.

## Roadmap

1. **Crontab parsing and `cronnext`:** the entry parser (`entry.c`), the
   environment lines, and the next-run calculation. These are pure, so they
   are easy to diff against C.
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
