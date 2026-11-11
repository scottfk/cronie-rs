#!/bin/bash
# vm-cron-check.sh - exercise cronie on a TEST VM and record what it did
#
# 20261111 - Created by Claude (claude.ai, Anthropic) - scottfk
# 20261111 - Install the user crontab with crontab -u (users can't reach the spool on Gentoo) - Claude - scottfk
#
# Usage (as root, in the test VM, with cronie and metalog in a runlevel):
#   tools/vm-cron-check.sh setup        install the test crontabs, clear the logs
#   tools/vm-cron-check.sh capture LABEL   after a boot: record into /root/cron-check/LABEL
#   tools/vm-cron-check.sh compare A B  diff two captures
#
# `setup` installs, through crontab itself:
# - a root crontab with an @reboot job and a per-minute job;
# - a crontab for user scottfk, with root's `crontab -u` (on Gentoo the
#   crontab group can't search /var/spool/cron, so a user's own crontab
#   command stops at "is not a directory"; `capture` records that too).
# `capture` waits for two minute boundaries, then records:
# - the crond processes (and whether the binary is C or cronie-rs);
# - the spool files, with owners and modes;
# - the jobs' output;
# - `crontab -l` for both users, and `cronnext -c`;
# - an `anacron -d -n -s` run;
# - the cron lines from metalog's log, without timestamps and pids.

set -euo pipefail

user=scottfk
out=/root/cron-out

case ${1:-} in
setup)
	rm -rf "$out" /tmp/user-tick /var/log/everything /var/spool/anacron/cron.*
	mkdir -p "$out"
	printf '%s\n' "@reboot echo reboot >> $out/reboot" "* * * * * echo tick >> $out/root-tick" | crontab -
	printf '%s\n' 'MAILTO=' '* * * * * id -un >> /tmp/user-tick' | crontab -u "$user" -
	exit 0
	;;
compare)
	diff -r "/root/cron-check/$2" "/root/cron-check/$3" && echo "no differences"
	exit 0
	;;
capture) label=${2:?label} ;;
*)
	echo "usage: $0 setup | capture LABEL | compare A B" >&2
	exit 2
	;;
esac

dir=/root/cron-check/$label
rm -rf "$dir"
mkdir -p "$dir"
for i in $(seq 60); do pgrep -x crond >/dev/null && break; sleep 1; done
pid=$(pgrep -xo crond)
{
	echo "crond: $(grep -qa RUST_BACKTRACE "/proc/$pid/exe" && echo cronie-rs || echo C)"
	echo "titles: $(pgrep -a crond | sed 's/^[0-9]* //' | sort | tr '\n' '|')"
	ls -ln /var/spool/cron/crontabs | sed 1d | awk '{print $1, $3, $4, $NF}'
	stat -c '%A %U:%G %n' /var/spool/cron/crontabs /usr/bin/crontab /run/cron.reboot 2>&1
} > "$dir/info"

# Two minute boundaries, then a few seconds for the jobs.
now=$(date +%s)
sleep $(( 120 - now % 60 + 5 ))

{
	echo "== root-tick: $(wc -l < $out/root-tick 2>/dev/null || echo 0) lines"
	echo "== reboot:"; cat "$out/reboot" 2>/dev/null || true
	echo "== user-tick:"; sort -u /tmp/user-tick 2>/dev/null || true
} > "$dir/jobs"
crontab -l > "$dir/crontab-root" 2>&1 || true
crontab -u "$user" -l > "$dir/crontab-u-user" 2>&1 || true
su "$user" -c 'crontab -l' > "$dir/crontab-user" 2>&1 || true
cronnext -c 2>&1 | sed -E 's/^( *next: ).*/\1N/; s/^( *nextstring: ).*/\1S/' > "$dir/cronnext"
anacron -d -n -s > "$dir/anacron.out" 2>&1 || echo "exit $?" >> "$dir/anacron.out"
ls -ln /var/spool/anacron | sed 1d | awk '{print $1, $3, $4, $NF}' > "$dir/anacron.spool"
sleep 2
grep -ahE 'crond|CROND|crontab|anacron' /var/log/everything/current 2>/dev/null |
	sed -E 's/^[A-Z][a-z]{2} [ 0-9][0-9] [0-9:]{8} //; s/\[[0-9]+\]/[PID]/g; s/(factor) [0-9]+%/\1 N%/' |
	sort > "$dir/syslog"
cat "$dir/info"
