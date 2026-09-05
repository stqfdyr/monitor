#!/bin/sh
# Installs monitor-agent as a systemd or OpenRC service.
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --server URL --token TOKEN [options]
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --server URL --register KEY [options]
set -eu

# Binary and token in one directory, the same one the hub uses, so a node has
# a single path to look at and a single path to clean up.
ROOT="/opt/monitor"
BIN="$ROOT/monitor-agent"
ENV_FILE="$ROOT/agent.env"
SERVER=""
TOKEN=""
REGISTER=""
INTERVAL=1
INSECURE=""

while [ $# -gt 0 ]; do
	# A flag with nothing after it: `$2` under set -u aborts with the shell's
	# own message instead of the usage below, and `shift 2` cannot shift.
	case "$1" in
	--server | --token | --register | --interval)
		[ $# -ge 2 ] || { echo "$1 needs a value" >&2; exit 2; } ;;
	esac
	case "$1" in
	--server) SERVER="$2"; shift 2 ;;
	--token) TOKEN="$2"; shift 2 ;;
	--register) REGISTER="$2"; shift 2 ;;
	--interval) INTERVAL="$2"; shift 2 ;;
	--insecure) INSECURE=1; shift ;;
	*) echo "unknown option: $1" >&2; exit 2 ;;
	esac
done

[ -n "$SERVER" ] && { [ -n "$TOKEN" ] || [ -n "$REGISTER" ]; } || {
	echo "usage: install.sh --server URL (--token TOKEN | --register KEY) [--interval SECONDS] [--insecure]" >&2
	exit 2
}
case "$INTERVAL" in "" | *[!0-9]*) echo "interval must be an integer from 1 to 3600" >&2; exit 2 ;; esac
[ "$INTERVAL" -ge 1 ] && [ "$INTERVAL" -le 3600 ] || { echo "interval must be from 1 to 3600" >&2; exit 2; }
# A bare host means TLS, which is the same upgrade the agent's ws_url() does
# with one -- and the same reversal under --insecure, where the hub has no TLS
# to upgrade to. Without this the two halves disagree: the agent would dial
# wss://, while curl below defaults a scheme-less URL to http:// and fetches
# the binary that is about to run as root over plaintext -- the worse half.
if [ -n "$INSECURE" ]; then SCHEME=http; else SCHEME=https; fi
case "$SERVER" in *://*) ;; *) SERVER="$SCHEME://$SERVER" ;; esac
# The agent already refuses plaintext ws:// to a remote hub, because the token
# would travel in the clear. The same address fetches the binary that is about
# to run as root here, so it gets the same rule: over plain HTTP anyone on the
# path can answer with a binary of their own.
#
# --insecure is the operator overriding both halves for a hub reached at
# ip:port with no TLS in front. It says so out loud rather than silently: this
# is the one step of the install that cannot be undone by fixing it later,
# because a MITM'd binary is already running as root by then.
#
# The test is on the host alone, with the scheme, the port and any path taken
# off, and it is an address match rather than a prefix: `127.evil.com` is a
# registered name whose owner resolves it wherever they like, and reading it as
# loopback would hand this exact plaintext channel to whoever owns it.
HOST="${SERVER#*://}"
HOST="${HOST%%/*}"
# RFC 3986 puts userinfo before the host, so `127.0.0.1:28080@evil.example.com`
# leaves a loopback address where the test below looks while curl -- which parses
# the URL properly -- fetches from whoever owns that name. Over plain HTTP, and
# the bytes are installed 0755 and started as root a few lines down. Nothing a
# hub is reached at needs userinfo; the agent's own ws_url refuses it too.
case "$HOST" in
*@*) echo "server URL must not contain '@': the host is whatever follows it" >&2; exit 2 ;;
esac
case "$HOST" in
"["*) HOST="${HOST#\[}"; HOST="${HOST%%]*}" ;;
*) HOST="${HOST%%:*}" ;;
esac
case "$HOST" in
127.*[!0-9.]*) LOCAL="" ;; # a name that merely starts 127.
127.* | localhost | ::1) LOCAL=1 ;;
*) LOCAL="" ;;
esac
case "$SERVER" in
http://*)
	if [ -z "$LOCAL" ]; then
		[ -n "$INSECURE" ] || {
			echo "refusing plaintext http:// to a remote hub; use https://, or --insecure if it has no TLS" >&2
			exit 2
		}
		echo "warning: --insecure over plain HTTP to $SERVER" >&2
		echo "         the token and every report travel in the clear, and the binary" >&2
		echo "         installed below is fetched over the same unverified channel" >&2
	fi
	;;
esac
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

# --register trades a key for this node's own token, which is what lets one
# command set up a batch of machines. The key is only good inside the window
# the panel opened, and never becomes the credential the agent runs with.
if [ -z "$TOKEN" ]; then
	# Re-running the same command must not add a second node. This machine's
	# token is already here, and it outlives the window that issued it, so the
	# env file is the answer before the hub is asked.
	#
	# Only for the same hub, though: a token hub A issued means nothing to hub
	# B, and keeping it would leave the agent authenticating forever against a
	# node that was never created -- with this installer reporting success.
	CACHED=$(sed -n 's/^MONITOR_SERVER=//p' "$ENV_FILE" 2>/dev/null || true)
	if [ "${CACHED%/}" = "${SERVER%/}" ]; then
		TOKEN=$(sed -n 's/^MONITOR_TOKEN=//p' "$ENV_FILE" 2>/dev/null || true)
		if [ -n "$TOKEN" ]; then
			echo "this machine is already registered; keeping its token"
		fi
	fi
fi
if [ -z "$TOKEN" ]; then
	# The hub trims and bounds this too; here it is kept to what a hostname is
	# allowed to contain, so nothing surprising travels in the body.
	NAME=$(hostname 2>/dev/null | tr -cd 'A-Za-z0-9._-' | cut -c1-64)
	echo "registering $NAME with the hub"
	TOKEN=$(curl -fsS --max-time 30 -H "Authorization: Bearer $REGISTER" \
		--data-binary "$NAME" "${SERVER%/}/api/agent/register") || {
		echo "the hub refused the registration key: the window may have closed," >&2
		echo "the key may be wrong, or it has registered enough nodes already." >&2
		echo "open a new one from the panel's node list." >&2
		exit 1
	}
fi

# The hub relays the binary, so a node only has to reach the hub it already
# talks to -- an IPv6-only or blocked machine never resolves github.com at all.
# A hub that cannot fetch releases itself points at a GitHub proxy in its own
# settings, which is why no proxy is asked for here.
URL="${SERVER%/}/agent/$ARCH"
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
install -d -m 0755 "$ROOT"
install -m 0755 "$TMP" "$BIN"

# The token lives in a root-only environment file rather than the unit, so it
# stays out of `systemctl cat` and the world-readable journal. 0600 root is
# what keeps it private -- $ROOT itself is readable, the binaries are in it.
# In a subshell: the unit file written further down is read by anyone
# debugging with `systemctl cat`, and a 0600 unit is a puzzle with no payoff.
(
	umask 077
	cat >"$ENV_FILE" <<ENV
MONITOR_SERVER=$SERVER
MONITOR_TOKEN=$TOKEN
ENV
)

if [ "$INIT" = openrc ]; then
	cat >/etc/init.d/monitor-agent <<RC
#!/sbin/openrc-run
description="monitor agent"
command="$BIN"
command_args="--interval $INTERVAL${INSECURE:+ --insecure}"
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
	. $ENV_FILE
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
EnvironmentFile=$ENV_FILE
ExecStart=$BIN --interval $INTERVAL${INSECURE:+ --insecure}
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
