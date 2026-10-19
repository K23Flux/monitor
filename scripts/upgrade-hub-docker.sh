#!/bin/sh
# Upgrade helper for a monitor hub run with docker compose, so that the panel's
# "升级 hub" button works.
#
# A hub in a container is unprivileged and cannot touch its own compose file or
# the docker daemon, so the button only makes it leave upgrade.request in its
# data directory. This script, installed once on the host, is what acts on it:
# a root systemd path unit watches that file and runs `run` below.
#
#   sudo sh upgrade-hub-docker.sh install [--dir /opt/apps/monitor] [--health URL]
#   sudo sh upgrade-hub-docker.sh uninstall
#
# What it will do is fixed here. The request file is read for one thing, a
# version number, and anything else in it is ignored: the image is always
# ghcr.io/k23flux/monitor, changed in the one compose file named at install.
# A new hub that does not answer afterwards is rolled back to the version it
# replaced.
set -eu
PATH="$PATH:/usr/sbin:/sbin:/usr/local/bin"

IMAGE="ghcr.io/k23flux/monitor"
CONF="${MONITOR_UPGRADE_CONF:-/etc/monitor-hub-upgrade.conf}"
BIN="/usr/local/sbin/monitor-hub-upgrade"
PATH_UNIT="/etc/systemd/system/monitor-hub-upgrade.path"
SERVICE_UNIT="/etc/systemd/system/monitor-hub-upgrade.service"
LOG="/var/log/monitor-hub-upgrade.log"
KEEP=3

die() { printf '%s\n' "$*" >&2; exit 1; }

# A value that ends up in the config file the root unit sources: kept to the
# characters a path or URL here needs, so nothing in it can be shell.
safe() {
	case "$2" in
	"" | *[!A-Za-z0-9_./:@%+=-]*) die "$1 里有不能用的字符：$2" ;;
	esac
}

compose_file() {
	for f in docker-compose.yml docker-compose.yaml compose.yml compose.yaml; do
		if [ -f "$1/$f" ]; then printf '%s' "$1/$f"; return 0; fi
	done
	return 1
}

# The tag the compose file runs now, empty unless exactly one line names the image.
current_tag() {
	n="$(grep -cE "^[[:space:]]*image:[[:space:]]*[\"']?$IMAGE:" "$1" || true)"
	[ "$n" = 1 ] || return 0
	sed -n "s#^[[:space:]]*image:[[:space:]]*[\"']\\{0,1\\}$IMAGE:\\([^\"'[:space:]]*\\).*#\\1#p" "$1"
}

install_helper() {
	DIR="/opt/apps/monitor"
	DATA=""
	HEALTH="http://127.0.0.1:28080/api/me"
	BACKUPS="/root"
	while [ $# -gt 0 ]; do
		[ $# -ge 2 ] || die "$1 后面要跟值"
		case "$1" in
		--dir) DIR="$2" ;;
		--data) DATA="$2" ;;
		--health) HEALTH="$2" ;;
		--backups) BACKUPS="$2" ;;
		*) die "未知参数：$1" ;;
		esac
		shift 2
	done
	[ -n "$DATA" ] || DATA="$DIR/data"
	safe --dir "$DIR"; safe --data "$DATA"; safe --health "$HEALTH"; safe --backups "$BACKUPS"

	[ "$(id -u)" = 0 ] || die "需要 root：sudo sh $0 install"
	command -v docker >/dev/null || die "没找到 docker"
	command -v curl >/dev/null || die "需要 curl（升级后靠它确认新 hub 起来了）"
	command -v systemctl >/dev/null || die "需要 systemd"
	[ -f "$0" ] || die "请把脚本存成文件再运行，不要用管道"
	file="$(compose_file "$DIR")" || die "$DIR 里没有 compose 文件"
	[ -n "$(current_tag "$file")" ] ||
		die "$file 里要正好有一行 image: $IMAGE:<版本>（升级靠改这一行的版本）"
	[ -d "$DATA" ] || die "数据目录 $DATA 不存在"

	# Replaced rather than written over: this may be run from the copy a
	# previous install left, and a shell reads its script as it goes.
	tmp="$(mktemp)"
	cp "$0" "$tmp"
	chmod 0755 "$tmp"
	mv -f "$tmp" "$BIN"
	printf 'DIR=%s\nDATA=%s\nHEALTH=%s\nBACKUPS=%s\n' "$DIR" "$DATA" "$HEALTH" "$BACKUPS" >"$CONF"
	chmod 0644 "$CONF"
	cat >"$PATH_UNIT" <<UNIT
[Unit]
Description=monitor hub upgrade request

[Path]
PathExists=$DATA/upgrade.request
Unit=monitor-hub-upgrade.service

[Install]
WantedBy=multi-user.target
UNIT
	cat >"$SERVICE_UNIT" <<UNIT
[Unit]
Description=monitor hub upgrade

[Service]
Type=oneshot
ExecStart=$BIN run
TimeoutStartSec=900
UNIT
	systemctl daemon-reload
	systemctl enable --now monitor-hub-upgrade.path >/dev/null 2>&1 || die "升级单元没能启动"
	# What the panel looks for before it offers the button.
	printf 'docker\n' >"$DATA/upgrade.helper"
	chmod 0644 "$DATA/upgrade.helper"
	echo "升级助手已装好：面板的「升级 hub」按钮会改 $file 里的镜像版本，备份放在 $BACKUPS，过程记在 $LOG。"
}

uninstall_helper() {
	[ "$(id -u)" = 0 ] || die "需要 root"
	if [ -f "$CONF" ]; then
		DATA="$(sed -n 's/^DATA=//p' "$CONF")"
		[ -z "$DATA" ] || rm -f "$DATA/upgrade.helper" "$DATA/upgrade.request"
	fi
	systemctl disable --now monitor-hub-upgrade.path 2>/dev/null || true
	rm -f "$PATH_UNIT" "$SERVICE_UNIT" "$BIN" "$CONF"
	systemctl daemon-reload 2>/dev/null || true
	echo "升级助手已卸载"
}

# Run by the path unit. Reports to upgrade.status in the data directory, which
# the panel reads, and keeps its own output in $LOG.
run_upgrade() {
	[ -f "$CONF" ] || die "没有 $CONF，先 install"
	# shellcheck disable=SC1090
	. "$CONF"
	STATUS="$DATA/upgrade.status"
	say() { printf '%s\n' "$1" >"$STATUS"; chmod 0644 "$STATUS"; }
	exec >"$LOG" 2>&1

	ask="$(head -c 64 "$DATA/upgrade.request" 2>/dev/null | tr -d '\n\r ' || true)"
	# Removed first, so one request is one run, however it ends.
	rm -f "$DATA/upgrade.request"
	say running
	fail() { echo "失败：$*"; say "failed: $*"; exit 1; }

	printf '%s\n' "$ask" | grep -qE '^[0-9]+(\.[0-9]+){1,3}(-sb\.[0-9]+)?$' ||
		fail "请求里的版本号不对：$ask"
	file="$(compose_file "$DIR")" || fail "$DIR 里没有 compose 文件"
	old="$(current_tag "$file")"
	[ -n "$old" ] || fail "$file 里找不到唯一的 $IMAGE 镜像行"
	if [ "$old" = "$ask" ]; then
		echo "已经是 $ask"
		say ok
		exit 0
	fi

	if docker compose version >/dev/null 2>&1; then
		dc() { docker compose "$@"; }
	elif command -v docker-compose >/dev/null 2>&1; then
		dc() { docker-compose "$@"; }
	else
		fail "没找到 docker compose"
	fi
	cd "$DIR"
	stamp="$(date +%Y%m%d-%H%M%S)"

	echo "升级 $old -> $ask"
	mkdir -p "$BACKUPS"
	# The hub is running and writing, so tar may report a file changed under it
	# (exit 1), which is a usable archive; only worse than that stops the upgrade.
	rc=0
	tar czf "$BACKUPS/monitor-data-before-$ask-$stamp.tgz" --exclude='upgrade.*' \
		-C "$(dirname "$DATA")" "$(basename "$DATA")" || rc=$?
	[ "$rc" -le 1 ] || fail "备份数据目录失败，没动任何东西"
	cp -p "$file" "$file.bak-$stamp"
	# Only the newest few of each kind stay.
	# shellcheck disable=SC2012
	ls -t "$BACKUPS"/monitor-data-before-*.tgz 2>/dev/null | tail -n +$((KEEP + 1)) | while read -r f; do rm -f "$f"; done
	# shellcheck disable=SC2012
	ls -t "$file".bak-* 2>/dev/null | tail -n +$((KEEP + 1)) | while read -r f; do rm -f "$f"; done

	sed -i "s#^\\([[:space:]]*image:[[:space:]]*[\"']\\{0,1\\}$IMAGE:\\)[^\"'[:space:]]*#\\1$ask#" "$file"
	[ "$(current_tag "$file")" = "$ask" ] || { cp -p "$file.bak-$stamp" "$file"; fail "改 compose 文件失败，已还原"; }

	if ! dc pull; then
		cp -p "$file.bak-$stamp" "$file"
		fail "拉取镜像 $IMAGE:$ask 失败，镜像发布了吗？没动正在跑的 hub"
	fi
	dc up -d || echo "up -d 没成功，下面看新 hub 有没有起来"

	up() {
		i=0
		while [ "$i" -lt 30 ]; do
			curl -fsS -m 3 -o /dev/null "$HEALTH" 2>/dev/null && return 0
			i=$((i + 1))
			sleep 2
		done
		return 1
	}
	if up; then
		echo "新 hub 已经起来"
		say ok
		exit 0
	fi

	echo "新 hub 一分钟内没起来，回滚到 $old"
	cp -p "$file.bak-$stamp" "$file"
	dc up -d || true
	if up; then
		fail "新版本 $ask 没起来，已回滚到 $old"
	fi
	fail "新版本 $ask 没起来，回滚后也没应答，请登录机器看 docker compose logs"
}

case "${1:-}" in
install) shift; install_helper "$@" ;;
uninstall) uninstall_helper ;;
run) run_upgrade ;;
*) die "用法：sh $0 install [--dir DIR] [--data DIR] [--health URL] [--backups DIR] | uninstall" ;;
esac
