# Upstream quirks, kept on purpose

cronie-rs reproduces cronie **1.7.2** exactly, including behavior that looks
like a bug. Each item here is marked `QUIRK` in the source. Fixing one is a
deliberate, separate change (ideally upstream as well), not something to slip
into the port.

Severity: **high** means it can run the wrong thing, run it at the wrong time,
or miss a job; **medium** means it's surprising and user-visible; **low**
means it's cosmetic or hard to hit.

## Crontab parsing

| Severity | Quirk | Where |
|---|---|---|
| low | A bad word in a time field (e.g. `* * * foo *`) pushes the character after it back twice, so it is read twice. The line is rejected anyway, but the extra character can change which error is reported or where the skip to the next line starts. | `entry.rs` (`get_number`) |
| low | The file name in some load errors is stale. `FileName` is a static, set only just before an entry is parsed or a missing final newline is reported. So "too many garbage characters", "too many entries" and "too many environment variables" name the *previous* crontab, or `(null)` if none was read yet. | `user.rs` (`load_user`) |

## crontab

| Severity | Quirk | Where |
|---|---|---|
| **high** | In `cron.allow` and `cron.deny`, a last line without a trailing newline makes `in_file()` give up, as if the name weren't there. So a user listed on the unterminated last line of **`cron.deny` is allowed in**, and one listed on the unterminated last line of `cron.allow` is kept out. Lines after an unterminated one are never reached. | `security.rs` (`in_file`) |
| medium | `crontab -e`: if the edited crontab has errors and stdin is at end-of-file, "Do you want to retry the same edit? (Y/N)" is asked forever (`fgets` fails, and the loop just continues). | `bin/crontab.rs` (`edit_cmd`) |
| low | When the backup directory can't be created, `backup_crontab()` "restores" an umask it never saved (an uninitialized variable). cronie-rs leaves the umask alone. Nothing is created afterwards, so the difference isn't observable. | `bin/crontab.rs` (`backup_crontab`) |
| low | `crontab -r` deletes the crontab even when the backup fails (only a missing crontab stops it). | `bin/crontab.rs` (`delete_cmd`) |

## crond

| Severity | Quirk | Where |
|---|---|---|
| medium | `$VAR` and `${VAR}` in `MAILTO` and `MAILFROM` are expanded from **crond's own environment**, not the job's. So `MAILTO=$USER` means the user crond runs as, unset variables expand to nothing, and a `$` that doesn't start a valid name (`$1`, `${x`) stops expansion: the rest of the value is used as is. | `misc.rs` (`expand_envvar`) |
| low | The `@reboot` lock file is created with mode `S_IRUSR & S_IWUSR` (`&`, not `\|`), which is 0. It only needs to exist, so nothing breaks. | `bin/crond.rs` (`run_reboot_jobs`) |
| low | Carriage returns are dropped from mailed job output, except a `\r` that is the very first byte. | `bin/crond.rs` (`child_process`) |

## anacron

| Severity | Quirk | Where |
|---|---|---|
| medium | `START_HOURS_RANGE` and `PREFERRED_HOUR` are only checked for jobs that already have an 8-byte timestamp file. A job's first run, or a run after its timestamp is lost, ignores them and runs at any hour. | `bin/anacron.rs` (`consider_job`) |
| medium | The anacrontab's environment assignments are applied with `setenv()` in anacron's own process and never undone. So a job also sees assignments that were applied for jobs that started before it, including ones written *after* it in the anacrontab (jobs run in order of delay, not of lines). | `bin/anacron.rs` (`setup_env`) |
| low | A `RANDOM_DELAY` larger than RAND_MAX/2 divides by zero, and anacron dies of SIGFPE while reading the anacrontab. | `bin/anacron.rs` (`unbiased_rand`) |
| low | A job with an unknown named period (`@fortnightly`) is reported as "skipping", but is registered anyway. C leaves its period type uninitialized; cronie-rs makes it 0, so the job is due on every run. | `bin/anacron.rs` (`register_period_job`) |
| low | Checking `@monthly`/`@weekly`-style jobs prints a leftover debugging line, `Checking against N with M`, to stdout. | `bin/anacron.rs` (`consider_job`) |
| low | Messages of exactly 150 characters are marked " (truncated)", although they fit. | `bin/anacron.rs` (`make_msg`) |

## Intentional differences

These are the only places where cronie-rs knowingly behaves differently. Each
replaces undefined behavior in C:

- **An environment line ending in an opening quote** (`A='`): C copies the
  terminating NUL and keeps scanning the reused line buffer past the end of
  the line, so the outcome depends on what earlier, longer lines left there.
  cronie-rs stops at the end of the line, which matches C whenever that stale
  data starts with a NUL. Either way, the line is almost always rejected as
  not being an environment setting.
- **Closing fds before a job runs:** C closes each fd while it lists
  `/proc/self/fd`, including the directory's own fd partway through. cronie-rs
  lists them first, then closes them. Either way, everything above stderr
  ends up closed.
- **`crontab` when `stat()` of the spool fails with an error other than
  ENOENT**: C goes on to test an uninitialized `struct stat`; cronie-rs
  reports that the spool is not a directory and exits.
- **`cronnext -i ""` / `-e ""`** with an empty user name would make C's
  `matchuser()` loop forever; cronie-rs treats it as "no match".
