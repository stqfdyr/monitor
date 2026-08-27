#!/bin/sh
# Installs monitor-agent as a systemd service.
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --server URL --token TOKEN
set -eu

REPO="@@REPO@@"
SERVER=""
TOKEN=""
INTERVAL=2

while [ $# -gt 0 ]; do
	case "$1" in
	--server) SERVER="$2"; shift 2 ;;
	--token) TOKEN="$2"; shift 2 ;;
	--interval) INTERVAL="$2"; shift 2 ;;
	*) echo "unknown option: $1" >&2; exit 2 ;;
	esac
done

[ -n "$SERVER" ] && [ -n "$TOKEN" ] || { echo "usage: install.sh --server URL --token TOKEN" >&2; exit 2; }
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
command -v systemctl >/dev/null || { echo "this installer needs systemd" >&2; exit 1; }

case "$(uname -m)" in
x86_64 | amd64) ARCH=x86_64 ;;
aarch64 | arm64) ARCH=aarch64 ;;
*) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

URL="https://github.com/$REPO/releases/latest/download/monitor-agent-$ARCH-unknown-linux-musl"
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
RestrictAddressFamilies=AF_INET AF_INET6
MemoryMax=64M

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now monitor-agent
echo "monitor-agent installed; follow it with: journalctl -u monitor-agent -f"
