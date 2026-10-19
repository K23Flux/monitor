#!/bin/sh
# Runs upgrade-hub-docker.sh against a fake docker, systemctl and curl: the
# install, a good upgrade, an image that cannot be pulled, a hub that does not
# come up (rolled back), a request that is not a version, and the uninstall.
#   scripts/test-upgrade-hub-docker.sh
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/upgrade-hub-docker.sh"
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/app/data" "$T/backups"

# docker: `compose pull` fails for version 9.9.9; `compose up -d` makes the hub
# healthy unless the compose file names version 9.9.8.
cat >"$T/bin/docker" <<'SH'
#!/bin/sh
[ "$1" = compose ] || exit 1
shift
case "$1" in
version) exit 0 ;;
pull) grep -q 'monitor:9.9.9' "$FAKE_DIR/app/docker-compose.yml" && exit 1; exit 0 ;;
up)
	if grep -q 'monitor:9.9.8' "$FAKE_DIR/app/docker-compose.yml"; then echo down >"$FAKE_DIR/health"; else echo ok >"$FAKE_DIR/health"; fi
	echo "up $(sed -n 's/.*image:.*monitor:\([^ ]*\).*/\1/p' "$FAKE_DIR/app/docker-compose.yml")" >>"$FAKE_DIR/docker.log"
	exit 0 ;;
esac
SH
cat >"$T/bin/curl" <<'SH'
#!/bin/sh
[ "$(cat "$FAKE_DIR/health")" = ok ]
SH
printf '#!/bin/sh\nexit 0\n' >"$T/bin/systemctl"
printf '#!/bin/sh\nexit 0\n' >"$T/bin/sleep"
chmod +x "$T"/bin/*
echo ok >"$T/health"
printf 'services:\n  monitor:\n    image: ghcr.io/k23flux/monitor:1.4.1-sb.6\n    restart: always\n' >"$T/app/docker-compose.yml"
echo data >"$T/app/data/monitor.db"

export FAKE_DIR="$T" PATH="$T/bin:$PATH" MONITOR_UPGRADE_CONF="$T/upgrade.conf"
# Paths the script writes to by design; moved under the sandbox for the test.
sed -e "s#^BIN=.*#BIN=\"$T/helper\"#" \
	-e "s#^PATH_UNIT=.*#PATH_UNIT=\"$T/u.path\"#" \
	-e "s#^SERVICE_UNIT=.*#SERVICE_UNIT=\"$T/u.service\"#" \
	-e "s#^LOG=.*#LOG=\"$T/upgrade.log\"#" "$SCRIPT" >"$T/script.sh"
S="$T/script.sh"

fail() { echo "FAIL: $*" >&2; cat "$T/upgrade.log" 2>/dev/null >&2 || true; exit 1; }
tag() { sed -n 's/.*image:.*monitor:\([^ ]*\).*/\1/p' "$T/app/docker-compose.yml"; }
status() { cat "$T/app/data/upgrade.status"; }

# --- install
sh "$S" install --dir "$T/app" --backups "$T/backups" --health http://127.0.0.1:1/x >/dev/null
[ -f "$T/app/data/upgrade.helper" ] || fail "no marker left for the panel"
grep -q "PathExists=$T/app/data/upgrade.request" "$T/u.path" || fail "path unit does not watch the request"
[ -x "$T/helper" ] || fail "helper not installed"

# --- a good upgrade
echo 1.4.1-sb.7 >"$T/app/data/upgrade.request"
sh "$T/helper" run
[ "$(tag)" = 1.4.1-sb.7 ] || fail "compose still at $(tag)"
[ "$(status)" = ok ] || fail "status $(status)"
[ ! -e "$T/app/data/upgrade.request" ] || fail "request not consumed"
ls "$T"/backups/monitor-data-before-1.4.1-sb.7-*.tgz >/dev/null || fail "no data backup"
ls "$T"/app/docker-compose.yml.bak-* >/dev/null || fail "no compose backup"

# --- asking for the version it already runs
echo 1.4.1-sb.7 >"$T/app/data/upgrade.request"
sh "$T/helper" run
[ "$(status)" = ok ] || fail "same version: $(status)"

# --- an image that cannot be pulled leaves the running hub and file alone
echo 9.9.9 >"$T/app/data/upgrade.request"
if sh "$T/helper" run; then fail "a failed pull reported success"; fi
case "$(status)" in failed:*) ;; *) fail "status after failed pull: $(status)" ;; esac
[ "$(tag)" = 1.4.1-sb.7 ] || fail "compose not restored after failed pull: $(tag)"

# --- a hub that does not come up is rolled back
echo 9.9.8 >"$T/app/data/upgrade.request"
if sh "$T/helper" run; then fail "a dead hub reported success"; fi
case "$(status)" in "failed: 新版本"*"已回滚"*) ;; *) fail "status after rollback: $(status)" ;; esac
[ "$(tag)" = 1.4.1-sb.7 ] || fail "compose not rolled back: $(tag)"
[ "$(cat "$T/health")" = ok ] || fail "old hub not brought back"

# --- the request carries a version and nothing else
for bad in 'latest' '1.4.1; touch /tmp/pwned' '../../etc' '1.4.1-sb.7 && rm -rf /' ''; do
	printf '%s' "$bad" >"$T/app/data/upgrade.request"
	if sh "$T/helper" run; then fail "accepted request: $bad"; fi
	case "$(status)" in failed:*) ;; *) fail "status for '$bad': $(status)" ;; esac
	[ "$(tag)" = 1.4.1-sb.7 ] || fail "compose touched by '$bad'"
done
[ ! -e /tmp/pwned ] || fail "a request ran as a command"

# --- uninstall
sh "$S" uninstall >/dev/null
[ ! -e "$T/app/data/upgrade.helper" ] && [ ! -e "$T/helper" ] || fail "uninstall left files"

echo "upgrade-hub-docker: all checks passed"
