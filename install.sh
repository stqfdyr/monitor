#!/bin/sh
# Installs monitor-agent as a systemd or OpenRC service.
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --server URL --token TOKEN [options]
set -eu

REPO="@@REPO@@"
SERVER=""
TOKEN=""
INTERVAL=1
GITHUB_PROXY=""

while [ $# -gt 0 ]; do
	case "$1" in
	--server) SERVER="$2"; shift 2 ;;
	--token) TOKEN="$2"; shift 2 ;;
	--interval) INTERVAL="$2"; shift 2 ;;
	--github-proxy) GITHUB_PROXY="$2"; shift 2 ;;
	*) echo "unknown option: $1" >&2; exit 2 ;;
	esac
done

[ -n "$SERVER" ] && [ -n "$TOKEN" ] || {
	echo "usage: install.sh --server URL --token TOKEN [--interval SECONDS] [--github-proxy URL]" >&2
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
install -m 0755 "$TMP" /usr/local/bin/monitor-agent

# The token lives in a root-only environment file rather than the unit, so it
# stays out of `systemctl cat` and the world-readable journal.
install -d -m 0700 /etc/monitor
umask 077
cat >/etc/monitor/agent.env <<ENV
MONITOR_SERVER=$SERVER
MONITOR_TOKEN=$TOKEN
ENV

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

systemctl daemon-reload
systemctl enable monitor-agent >/dev/null
# restart, not `enable --now`: --now leaves an already-running service alone,
# so reinstalling on top of a live agent would keep the old binary running.
systemctl restart monitor-agent
echo "monitor-agent installed; follow it with: journalctl -u monitor-agent -f"
