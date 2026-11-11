#!/bin/bash
# build-reference.sh - build upstream C cronie at the pinned tag, for the differential tests
#
# 20261111 - Created by Claude (claude.ai, Anthropic) - scottfk
#
# Usage: tools/build-reference.sh [DIR]
#
# DIR defaults to ../cronie (next to this checkout), which is where the
# tests look (override with CRONIE_REFERENCE=/path/to/cronie).  Builds the
# way Gentoo's sys-process/cronie does with USE="anacron inotify pam":
# in-tree, Gentoo's C23 prototype patch (no behavior change), and the
# ebuild's configure flags and spool paths.

set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
dir=${1:-$here/../cronie}
tag=$(cat "$here/tools/REFERENCE_TAG")
url=https://github.com/cronie-crond/cronie.git
patch=/var/db/repos/gentoo/sys-process/cronie/files/cronie-1.7.2-c23.patch

if [[ ! -d $dir/.git ]]; then
	git clone "$url" "$dir"
fi
cd "$dir"
if [[ $(git describe --tags --exact-match 2>/dev/null || true) != "$tag" ]]; then
	git fetch --tags origin
	git checkout -q "$tag"
fi
if [[ -f $patch ]] && patch -p1 -s -N --dry-run < "$patch" >/dev/null 2>&1; then
	patch -p1 -s -N < "$patch"
fi
if [[ ! -x configure ]]; then
	autoreconf -fi
fi
if [[ ! -f Makefile ]]; then
	SPOOL_DIR=/var/spool/cron/crontabs ANACRON_SPOOL_DIR=/var/spool/anacron \
	./configure -q --prefix=/usr --sysconfdir=/etc --localstatedir=/var \
		--without-audit --with-inotify --with-pam --without-selinux \
		--enable-anacron --enable-syscrontab --runstatedir=/run \
		--with-daemon_username=cron --with-daemon_groupname=cron
fi
make -s
echo "reference: $dir ($(./src/crond -V 2>&1))"
