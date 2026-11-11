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

## Intentional differences

These are the only places where cronie-rs knowingly behaves differently. Each
replaces undefined behavior in C:

- **An environment line ending in an opening quote** (`A='`): C copies the
  terminating NUL and keeps scanning the reused line buffer past the end of
  the line, so the outcome depends on what earlier, longer lines left there.
  cronie-rs stops at the end of the line, which matches C whenever that stale
  data starts with a NUL. Either way, the line is almost always rejected as
  not being an environment setting.
- **`cronnext -i ""` / `-e ""`** with an empty user name would make C's
  `matchuser()` loop forever; cronie-rs treats it as "no match".
