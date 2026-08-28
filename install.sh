#!/bin/sh
# Installs monitor-agent as a systemd or OpenRC service.
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --server URL --token TOKEN [options]
set -eu

REPO="@@REPO@@"
SERVER=""
TOKEN=""
INTERVAL=1
GITHUB_PROXY=""
AUTO_UPDATE=1

while [ $# -gt 0 ]; do
	case "$1" in
	--server) SERVER="$2"; shift 2 ;;
	--token) TOKEN="$2"; shift 2 ;;
	--interval) INTERVAL="$2"; shift 2 ;;
	--github-proxy) GITHUB_PROXY="$2"; shift 2 ;;
	--auto-update) AUTO_UPDATE=1; shift ;;
	--no-auto-update) AUTO_UPDATE=0; shift ;;
	*) echo "unknown option: $1" >&2; exit 2 ;;
	esac
done

[ -n "$SERVER" ] && [ -n "$TOKEN" ] || {
	echo "usage: install.sh --server URL --token TOKEN [--interval SECONDS] [--github-proxy URL] [--no-auto-update]" >&2
	exit 2
}
case "$INTERVAL" in "" | *[!0-9]*) echo "interval must be an integer from 1 to 3600" >&2; exit 2 ;; esac
[ "$INTERVAL" -ge 1 ] && [ "$INTERVAL" -le 3600 ] || { echo "interval must be from 1 to 3600" >&2; exit 2; }
case "$GITHUB_PROXY" in "" | http://* | https://*) ;; *) echo "GitHub proxy must start with http:// or https://" >&2; exit 2 ;; esac
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
if command -v systemctl >/dev/null; then
	INIT=systemd
elif command -v rc-update >/dev/null; then
	INIT=openrc
else
	echo "this installer needs systemd or OpenRC" >&2
	exit 1
fi

case "$(uname -m)" in
x86_64 | amd64) ARCH=x86_64 ;;
aarch64 | arm64) ARCH=aarch64 ;;
*) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

# The hub relays the binary by default, so an IPv6-only or blocked node only
# needs to reach the hub it already talks to. A proxy overrides that and goes
# to GitHub directly, for when the hub itself cannot fetch releases.
if [ -n "$GITHUB_PROXY" ]; then
	URL="${GITHUB_PROXY%/}/https://github.com/$REPO/releases/latest/download/monitor-agent-$ARCH-unknown-linux-musl"
else
	URL="${SERVER%/}/agent/$ARCH"
fi
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

echo "downloading monitor-agent ($ARCH)"
curl -fsSL "$URL" -o "$TMP"

# Stop an agent already running here before replacing its binary. The service
# name is fixed, so a reinstall was never going to start a second copy, but
# without this the new binary lands underneath a live process and only the
# restart at the end picks it up. Stopping first also means the copy does not
# depend on `install` choosing to unlink rather than fail with ETXTBSY.
# After the download, so a node that cannot fetch the binary keeps running.
if [ "$INIT" = openrc ]; then
	rc-service monitor-agent stop 2>/dev/null || true
else
	systemctl stop monitor-agent 2>/dev/null || true
fi
install -m 0755 "$TMP" /usr/local/bin/monitor-agent

# The token lives in a root-only environment file rather than the unit, so it
# stays out of `systemctl cat` and the world-readable journal.
install -d -m 0700 /etc/monitor
umask 077
cat >/etc/monitor/agent.env <<ENV
MONITOR_SERVER=$SERVER
MONITOR_TOKEN=$TOKEN
ENV

# ---- automatic updates ----
#
# A root oneshot on a timer, not something the agent does to itself. Self-update
# was turned down once for a good reason (see docs/decisions.md): it means the
# long-lived, network-facing process can write its own binary, so it has to run
# with the privileges to do that. This keeps the agent under DynamicUser with a
# read-only filesystem, and the thing that can replace a binary runs for two
# seconds a day.
UPDATER=/usr/local/bin/monitor-agent-update

# The updater re-downloads from this URL unattended, forever. Plaintext would
# hand anyone on the path a root binary on every node, which is the one way an
# auto-updater is worse than no auto-updater. Loopback is the dev case.
case "$URL" in
https://* | http://127.0.0.1[:/]* | http://localhost[:/]* | "http://[::1]"[:/]*) ;;
*)
	[ "$AUTO_UPDATE" = 1 ] && echo "auto-update needs an https URL; installing without it" >&2
	AUTO_UPDATE=0
	;;
esac

if [ "$AUTO_UPDATE" = 1 ]; then
	printf '%s\n' "$URL" >/etc/monitor/update.url
	chmod 0600 /etc/monitor/update.url
	cat >"$UPDATER" <<'UPD'
#!/bin/sh
# Written by monitor's install.sh. Replaces the agent binary only when the
# published one differs, runs, and leaves the service up.
set -eu
BIN=/usr/local/bin/monitor-agent
URL="$(cat /etc/monitor/update.url)"
# Beside the binary rather than in /tmp: the smoke test below has to execute
# this file, and a hardened box mounts /tmp noexec. Same filesystem too, so
# the install at the end is a rename rather than a copy across devices.
TMP="$(mktemp "$BIN.new.XXXXXX")"
trap 'rm -f "$TMP"' EXIT

if command -v systemctl >/dev/null; then
	stop() { systemctl stop monitor-agent >/dev/null 2>&1 || true; }
	start() { systemctl start monitor-agent; }
	alive() { systemctl is-active --quiet monitor-agent; }
else
	stop() { rc-service monitor-agent stop >/dev/null 2>&1 || true; }
	start() { rc-service monitor-agent start >/dev/null; }
	alive() { rc-service monitor-agent status >/dev/null 2>&1; }
fi

# A hub that is down or a network that is out is not worth an alert: the timer
# comes back tomorrow and the agent keeps running in the meantime.
curl -fsSL --max-time 120 "$URL" -o "$TMP" || exit 0
[ -s "$TMP" ] || exit 0
cmp -s "$TMP" "$BIN" && exit 0

chmod 0755 "$TMP"
# Proves the download is a binary this machine can execute before it replaces
# one that already works: a truncated file, an HTML error page from a proxy, or
# the wrong architecture all stop here rather than at the restart.
"$TMP" --help 2>&1 | head -1 | grep -q '^monitor-agent ' || {
	echo "downloaded file is not a monitor-agent binary; keeping the current one" >&2
	exit 1
}

cp -p "$BIN" "$BIN.prev"
stop
install -m 0755 "$TMP" "$BIN"
start

# A binary that starts and dies leaves the unit restarting rather than active.
# Putting the old one back beats leaving the node dark until someone notices.
sleep 5
if ! alive; then
	install -m 0755 "$BIN.prev" "$BIN"
	stop
	start
	echo "the new agent did not stay up; rolled back to the previous binary" >&2
	exit 1
fi
echo "monitor-agent updated"
UPD
	chmod 0755 "$UPDATER"
fi

if [ "$INIT" = openrc ]; then
	cat >/etc/init.d/monitor-agent <<RC
#!/sbin/openrc-run
description="monitor agent"
command="/usr/local/bin/monitor-agent"
command_args="--interval $INTERVAL"
supervisor="supervise-daemon"
respawn_delay=5
output_log="/var/log/monitor-agent.log"
error_log="/var/log/monitor-agent.log"

depend() {
	need net
}

# The token stays in the root-only env file instead of the service script.
start_pre() {
	set -a
	. /etc/monitor/agent.env
	set +a
}
RC
	chmod 0755 /etc/init.d/monitor-agent
	rc-update add monitor-agent default >/dev/null
	# No timers here; Alpine's busybox crond runs whatever is in this directory.
	if [ "$AUTO_UPDATE" = 1 ] && [ -d /etc/periodic/daily ]; then
		ln -sf "$UPDATER" /etc/periodic/daily/monitor-agent-update
	else
		rm -f /etc/periodic/daily/monitor-agent-update
		[ "$AUTO_UPDATE" = 1 ] && echo "no /etc/periodic/daily; skipping auto-update" >&2
	fi
	rc-service monitor-agent restart
	echo "monitor-agent installed; follow it with: tail -f /var/log/monitor-agent.log"
	exit 0
fi

cat >/etc/systemd/system/monitor-agent.service <<UNIT
[Unit]
Description=monitor agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=/etc/monitor/agent.env
ExecStart=/usr/local/bin/monitor-agent --interval $INTERVAL
Restart=always
RestartSec=5
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
# AF_NETLINK is how getifaddrs(3) asks the kernel for this box's own
# addresses; without it the agent reports none at all.
RestrictAddressFamilies=AF_INET AF_INET6 AF_NETLINK
MemoryMax=64M

[Install]
WantedBy=multi-user.target
UNIT

if [ "$AUTO_UPDATE" = 1 ]; then
	cat >/etc/systemd/system/monitor-agent-update.service <<UNIT
[Unit]
Description=update monitor-agent
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
# Longer than the download's own 120s cap, so a slow link fails on curl's
# terms rather than being killed halfway by the default 90s start timeout.
TimeoutStartSec=300
ExecStart=$UPDATER
UNIT
	# Persistent catches a machine that was off at the scheduled time; the
	# random delay keeps a fleet from asking the hub for the same 1.6 MB in
	# the same second.
	cat >/etc/systemd/system/monitor-agent-update.timer <<'UNIT'
[Unit]
Description=daily monitor-agent update check

[Timer]
OnCalendar=daily
RandomizedDelaySec=6h
Persistent=true

[Install]
WantedBy=timers.target
UNIT
else
	# Reinstalling with --no-auto-update has to actually turn it off.
	systemctl disable --now monitor-agent-update.timer >/dev/null 2>&1 || true
	rm -f /etc/systemd/system/monitor-agent-update.timer \
		/etc/systemd/system/monitor-agent-update.service \
		/etc/monitor/update.url "$UPDATER"
fi

systemctl daemon-reload
systemctl enable monitor-agent >/dev/null
[ "$AUTO_UPDATE" = 1 ] && systemctl enable --now monitor-agent-update.timer >/dev/null
# restart, not `enable --now`: --now leaves an already-running service alone,
# so reinstalling on top of a live agent would keep the old binary running.
systemctl restart monitor-agent
echo "monitor-agent installed; follow it with: journalctl -u monitor-agent -f"
# An `A && B` here would make the script exit 1 whenever auto-update is off,
# which is a failed install as far as `curl | sh` is concerned.
if [ "$AUTO_UPDATE" = 1 ]; then
	echo "auto-update is on; it checks once a day and only restarts when the binary changes"
fi
