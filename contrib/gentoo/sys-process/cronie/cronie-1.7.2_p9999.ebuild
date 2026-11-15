# Copyright 1999-2026 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2
# 20261111 - Created by Claude (claude.ai, Anthropic) - scottfk
# 20261115 - Fetch from GitHub; the repo moved off Codeberg - Claude - scottfk

# cronie 1.7.2 with its programs (crond, crontab, cronnext, anacron) from
# cronie-rs (a strict-compatibility Rust port, built live from git).
# Upstream's release is built and installed exactly as
# sys-process/cronie-1.7.2-r1 does it, which supplies the configuration,
# init script, PAM file, man pages and directories; then each program is
# replaced by cronie-rs's, with the same path, owner and mode.  Pin the
# release with a package.mask of >sys-process/cronie-1.7.2_p9999:
# cronie-rs ports exactly 1.7.2.

EAPI=8

CRONIE_PV="1.7.2"
# cronie-rs's Cargo.lock: update together.
CRATES="libc@0.2.191"
RUST_MIN_VER="1.88.0"

# cronie supports /etc/crontab
CRON_SYSTEM_CRONTAB="yes"

inherit cargo cron flag-o-matic git-r3 pam systemd

DESCRIPTION="Cronie is a standard UNIX daemon cron based on the original vixie-cron (cronie-rs programs)"
HOMEPAGE="https://github.com/scottfk/cronie-rs https://github.com/cronie-crond/cronie"
EGIT_REPO_URI="https://github.com/scottfk/cronie-rs.git"
SRC_URI="
	https://github.com/cronie-crond/cronie/releases/download/cronie-${CRONIE_PV}/cronie-${CRONIE_PV}.tar.gz
	${CARGO_CRATE_URIS}
"
S="${WORKDIR}/cronie-${CRONIE_PV}"
# The cronie-rs checkout.
RS="${WORKDIR}/cronie-rs"
EGIT_CHECKOUT_DIR="${RS}"

LICENSE="ISC BSD BSD-2 GPL-2+"
# Dependent crate licenses
LICENSE+=" || ( Apache-2.0 MIT )"
SLOT="0"
# cronie-rs ports the build with inotify and PAM (and without audit and
# SELinux), so those are fixed.
IUSE="+anacron"
# The differential tests need upstream's C build and subordinate ids.
RESTRICT="test"

DEPEND="
	sys-libs/pam
	anacron? (
		!sys-process/anacron
		!sys-process/systemd-cron
	)
"
BDEPEND="acct-group/crontab"
RDEPEND="
	${BDEPEND}
	${DEPEND}
	sys-apps/debianutils
"

PATCHES=(
	"${FILESDIR}"/cronie-${CRONIE_PV}-c23.patch
)

src_unpack() {
	cargo_src_unpack
	git-r3_src_unpack
}

src_configure() {
	local myeconfargs=(
		--without-audit
		--with-inotify
		--with-pam
		--without-selinux
		$(use_enable anacron)
		--enable-syscrontab
		# Required for correct pidfile location, bug #835814
		--runstatedir="${EPREFIX}/run"
		--with-daemon_username=cron
		--with-daemon_groupname=cron
	)

	SPOOL_DIR="/var/spool/cron/crontabs" \
	ANACRON_SPOOL_DIR="/var/spool/anacron" \
	econf "${myeconfargs[@]}"

	pushd "${RS}" >/dev/null || die
	cargo_src_configure
	popd >/dev/null || die
}

src_compile() {
	default

	pushd "${RS}" >/dev/null || die
	cargo_src_compile
	popd >/dev/null || die
}

src_install() {
	default

	# Replace upstream's programs with cronie-rs's: same paths.
	local bin="${RS}/$(cargo_target_dir)"
	exeinto /usr/sbin
	doexe "${bin}"/crond
	use anacron && doexe "${bin}"/anacron
	exeinto /usr/bin
	doexe "${bin}"/crontab "${bin}"/cronnext

	docrondir -m 1730 -o root -g crontab
	fowners root:crontab /usr/bin/crontab
	fperms 2751 /usr/bin/crontab

	newconfd "${S}"/crond.sysconfig ${PN}

	insinto /etc
	newins "${FILESDIR}/${PN}-crontab" crontab
	newins "${FILESDIR}/${PN}-1.2-cron.deny" cron.deny

	insinto /etc/cron.d
	doins contrib/{0hourly,dailyjobs}

	newinitd "${FILESDIR}/${PN}-1.3-initd" ${PN}

	newpamd "${FILESDIR}/${PN}-1.4.3-pamd" crond

	systemd_newunit contrib/cronie.systemd cronie.service

	if use anacron ; then
		local anacrondir="/var/spool/anacron"
		keepdir ${anacrondir}
		fowners root:cron ${anacrondir}
		fperms 0750 ${anacrondir}

		insinto /etc
		doins contrib/anacrontab

		insinto /etc/cron.hourly
		doins contrib/0anacron
		fperms 0750 /etc/cron.hourly/0anacron
	fi

	einstalldocs
	docinto cronie-rs
	dodoc "${RS}"/{README,QUIRKS}.md
}

pkg_postinst() {
	cron_pkg_postinst

	if [[ -n "${REPLACING_VERSIONS}" ]] ; then
		ewarn "You should restart ${PN} daemon or else you might experience segfaults"
		ewarn "or ${PN} not working reliably anymore."
		einfo "(see https://bugs.gentoo.org/557406 for details.)"
	fi

	elog "cronie's programs are now cronie-rs's (crond, crontab, cronnext, anacron)."
	elog "Its known differences from C are in ${EROOT}/usr/share/doc/${PF}/cronie-rs/QUIRKS.md*."
	elog "The running daemon is still the old one: rc-service cronie restart"
	elog "To go back, emerge the C release: emerge -1 =sys-process/cronie-${CRONIE_PV}-r1"
}
