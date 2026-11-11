# Gentoo: sys-process/cronie from cronie-rs

`sys-process/cronie/cronie-1.7.2_p9999.ebuild` is a live ebuild. It installs
cronie 1.7.2 with its four programs (`crond`, `crontab`, `cronnext` and
`anacron`) replaced by cronie-rs's, built from this repository's `main`.
Everything else is the same as Gentoo's `sys-process/cronie-1.7.2-r1`:

- Upstream's release is built and installed exactly as Gentoo does it: the
  same distfile and hashes, the same `econf` arguments, and Gentoo's c23
  patch. That supplies the man pages, `/etc/anacrontab`, `/etc/cron.d/0hourly`,
  `/etc/cron.hourly/0anacron`, `conf.d/cronie`, the systemd unit and the
  directories.
- Then each program is replaced by cronie-rs's, with the same path, owner
  and mode. `crontab` is still `root:crontab`, mode 2751.
- The init script, the PAM file, `/etc/crontab` and `/etc/cron.deny` are
  Gentoo's, copied into `files/`.

Notes:

- It keeps the package name `sys-process/cronie`, so `virtual/cron` and
  anything else that depends on cronie is satisfied as before.
- `anacron` is the only USE flag. cronie-rs ports the build with inotify and
  PAM, and without audit and SELinux, so those flags are gone.
- The only crate is `libc`, fetched like a normal distfile (`CRATES`), so no
  `-network-sandbox` is needed. If cronie-rs's `Cargo.lock` changes, update
  `CRATES` and regenerate the Manifest.

It was rehearsed on the test VM (snapshot `cronie-base`, with metalog-rs
logging). [tools/vm-cron-check.sh](../../tools/vm-cron-check.sh) installs
the test crontabs for root and a user. After a boot, it waits two minutes,
then records the spool, the jobs' output, `crontab -l`, `cronnext -c`, an
`anacron -d -n -s` run and the cron lines in syslog. The rehearsal went:

1. Boot on C cronie and capture.
2. Switch to this ebuild and restart the service.
3. Boot and capture.
4. Roll back to C from the binary package only.
5. Reinstall this ebuild.

The two captures were the same apart from the line that says which crond
ran. That covers the `@reboot` job, the per-minute jobs for root and for the
user (with PAM sessions), the spool files' owners and modes, and the
anacron run with its syslog lines.

## Setup (once)

Put the package in a local overlay (here `localrepo`), then generate the
Manifest:

```sh
cp -r contrib/gentoo/sys-process/cronie /var/db/repos/localrepo/sys-process/
ebuild /var/db/repos/localrepo/sys-process/cronie/cronie-1.7.2_p9999.ebuild manifest
```

Then, in `/etc/portage`, *add* these lines and keep the ones you already
have:

```sh
# package.accept_keywords: it's a live ebuild
=sys-process/cronie-1.7.2_p9999 **
# package.mask: cronie-rs ports exactly 1.7.2, so nothing newer may replace it
>sys-process/cronie-1.7.2_p9999
```

## Before switching: a binary package of the C cronie

```sh
quickpkg --include-config=y sys-process/cronie
```

If quickpkg fails with "binpkg appears unsigned", Portage is set to require
signed binary packages (`binpkg-request-signature`, common with Gentoo's
binhost). Run it as `FEATURES=-binpkg-request-signature quickpkg ...`, and
do the same for the `--usepkgonly` emerge below. hydra doesn't set that
feature.

## Switching

```sh
emerge -1 =sys-process/cronie-1.7.2_p9999
rc-service cronie restart
ls -l /proc/$(pgrep -xo crond)/exe     # not "(deleted)": the new binary runs
```

Until the restart, the old C daemon keeps running from its deleted file.
It still runs jobs, but they come from C. The restart doesn't re-run
`@reboot` jobs: `/run/cron.reboot` is still there, as with C.

Your crontabs, `/etc/crontab`, `/etc/cron.d` and the anacron timestamps are
read as they are. cronie-rs uses the same file formats.

## Going back

```sh
emerge -1 --usepkgonly =sys-process/cronie-1.7.2-r1   # or without --usepkgonly, to build it
rc-service cronie restart
```

The differences from C that cronie-rs keeps on purpose (upstream bugs ported
as they are) are in QUIRKS.md, installed in
`/usr/share/doc/cronie-1.7.2_p9999/cronie-rs/`.

A Gentoo detail that looks like a bug but is the same in C:
`/var/spool/cron` is `root:cron` 0750, so a user's own `crontab` (setgid
`crontab`, not `cron`) can't reach the spool. It stops with
"'/var/spool/cron/crontabs' is not a directory, bailing out." root can
still manage users' crontabs with `crontab -u USER`.
