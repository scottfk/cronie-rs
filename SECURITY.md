# Security

cronie-rs is a reimplementation of a cron daemon. `crond` runs as root and starts jobs as
every user on the system. `crontab` is setgid `crontab` and reads input from any local
user. Security bugs here matter, even though this is a hobby project.

## Status and supported versions

cronie-rs is pre-release (0.x). There are no maintained release branches: only the
latest `main` gets fixes.

## What counts as a security issue

These are the kinds of bugs I most want to hear about:

- **Privilege boundaries.** A job running as the wrong user, with the wrong groups, or
  with root's environment, limits or file descriptors. Ways around `cron.allow` and
  `cron.deny`, or the PAM account checks.
- **`crontab` as an unprivileged user.** Reading or writing other users' crontabs,
  writing outside the spool, races on the temporary file or the editor, or anything the
  setgid `crontab` group can be made to do on someone's behalf.
- **Untrusted crontab input.** Crashes, hangs or memory growth from crontabs (any user
  can install one), environment lines, or `/etc/cron.d` files.
- **Memory safety.** Unsoundness in the `unsafe` code, or anything reachable from safe
  code that causes undefined behavior.
- **Divergence from upstream that weakens security.** For example, a permission check,
  an ownership check on a crontab file, or a privilege drop that C cronie does and
  cronie-rs skips.

Upstream behavior that is reproduced on purpose will be listed in QUIRKS.md. Reports
about it are still welcome, especially if the impact is worse than described. It will be
fixed as deliberate changes.

If the same bug is also present in **upstream C cronie**, please report it to the
[cronie project](https://github.com/cronie-crond/cronie/security) as well. Upstream's fix
is what cronie-rs will follow.

## How to report

For anything that could be exploited, **please don't put the details in a public issue.**
Email them to me instead:

    scone-void-baggage (at) duck (dot) com

(Replace `(at)` with `@` and `(dot)` with `.`, and remove the spaces.)

If you don't get a reply, open an issue on <https://codeberg.org/scottfk/cronie-rs/issues>
that only says you've sent a security report, without any details.

For low-risk problems, such as hardening suggestions or issues that need root already
(a malicious `/etc/crontab`, for instance), a normal public issue is fine.

Useful things to include:

- The commit you tested.
- The crontab or command involved, the user it ran as, and the exact steps.
- What happened, and what C cronie does at the pinned tag (`tools/REFERENCE_TAG`).

## What to expect

This is a spare-time project, so I can't promise a response time, but I'll acknowledge
security reports as soon as I can, fix confirmed bugs on `main`, and credit you in the
commit unless you'd rather I didn't.
