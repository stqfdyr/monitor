#!/bin/sh
# Installs monitor-agent as a systemd or OpenRC service.
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --server URL --token TOKEN [options]
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --server URL --register KEY [options]
set -eu

# Binary and token in one directory, the same one the hub uses, giving a node a
# single path to inspect and a single path to remove.
ROOT="/opt/monitor"
BIN="$ROOT/monitor-agent"
ENV_FILE="$ROOT/agent.env"
SERVER=""
TOKEN=""
REGISTER=""
INTERVAL=1
INSECURE=""

while [ $# -gt 0 ]; do
	# A flag with no argument: under set -u, `$2` aborts with the shell's own
	# message rather than the usage below, and `shift 2` cannot proceed.
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
# A bare host implies TLS, matching the upgrade the agent's ws_url() performs,
# and the same reversal under --insecure where the hub has no TLS to upgrade to.
# Without this the two diverge: the agent would dial wss:// while curl below
# defaults a scheme-less URL to http://, fetching over plaintext the binary about
# to run as root.
if [ -n "$INSECURE" ]; then SCHEME=http; else SCHEME=https; fi
case "$SERVER" in *://*) ;; *) SERVER="$SCHEME://$SERVER" ;; esac
# The agent already refuses plaintext ws:// to a remote hub, since the token
# would travel in the clear. The same address fetches the binary about to run as
# root here, so the same rule applies: over plain HTTP anyone on the path can
# substitute a binary of their own.
#
# --insecure overrides both halves for a hub reached at ip:port with no TLS in
# front, and says so explicitly: this is the one step of the install that cannot
# be corrected afterwards, because a substituted binary is already running as
# root by then.
#
# The test applies to the host alone, with scheme, port and path removed, and
# matches an address rather than a prefix: `127.evil.com` is a registered name
# resolving wherever its owner points it, and reading it as loopback would hand
# this plaintext channel to that owner.
HOST="${SERVER#*://}"
HOST="${HOST%%/*}"
# RFC 3986 places userinfo before the host, so `127.0.0.1:28080@evil.example.com`
# leaves a loopback address where the test below looks while curl, which parses
# the URL correctly, fetches from the owner of that name -- over plain HTTP, with
# the bytes installed 0755 and started as root a few lines below. A hub address
# never requires userinfo; the agent's own ws_url rejects it as well.
case "$HOST" in
*@*) echo "server URL must not contain '@': the host is whatever follows it" >&2; exit 2 ;;
esac
case "$HOST" in
"["*) HOST="${HOST#\[}"; HOST="${HOST%%]*}" ;;
*) HOST="${HOST%%:*}" ;;
esac
# A full dotted quad in 127/8 and nothing shorter, matching what the agent's
# is_loopback() accepts, since Rust's IpAddr parser accepts nothing shorter
# either -- `127.1` is a name to it, not an address. The two must agree, or this
# installs over plaintext against a hub the agent then refuses to dial: the unit
# is written, the service started, and it crash-loops on RestartSec while this
# script has reported success.
#
# The first arm excludes anything containing a letter, which is a registered name
# however it begins, and anything with more than four components, which is not an
# address.
case "$HOST" in
localhost | ::1) LOCAL=1 ;;
*[!0-9.]* | *.*.*.*.*) LOCAL="" ;;
127.[0-9]*.[0-9]*.[0-9]*) LOCAL=1 ;;
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

# The hub relays the binary, so a node need only reach the hub it already talks
# to: an IPv6-only or blocked machine cannot resolve github.com. A hub unable to
# fetch releases itself is configured with a GitHub proxy in its own settings,
# which is why none is requested here.
URL="${SERVER%/}/agent/$ARCH"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

echo "downloading monitor-agent ($ARCH)"
curl -fsSL "$URL" -o "$TMP"

# Downloaded before the registration below, because that step spends a node: the
# key returns a token and the panel gains a row, while the env file recording it
# is only written once the binary is in place. A download that fails after
# registering therefore leaves an unusable node behind, and the rerun -- the
# documented way to recover -- registers a second one.
#
# --register exchanges a key for this node's own token, which is what allows one
# command to provision a batch of machines. The key is valid only within the
# window the panel opened and never becomes the credential the agent runs with.
if [ -z "$TOKEN" ]; then
	# Re-running the same command must not add a second node. This machine's
	# token is already present and outlives the window that issued it, so the env
	# file answers before the hub is consulted.
	#
	# Only for the same hub: a token issued by hub A means nothing to hub B, and
	# retaining it would leave the agent authenticating indefinitely against a
	# node that was never created, with this installer reporting success.
	CACHED=$(sed -n 's/^MONITOR_SERVER=//p' "$ENV_FILE" 2>/dev/null || true)
	if [ "${CACHED%/}" = "${SERVER%/}" ]; then
		TOKEN=$(sed -n 's/^MONITOR_TOKEN=//p' "$ENV_FILE" 2>/dev/null || true)
		if [ -n "$TOKEN" ]; then
			echo "this machine is already registered; keeping its token"
		fi
	fi
fi
if [ -z "$TOKEN" ]; then
	# The hub trims and bounds this as well; here it is restricted to characters
	# a hostname may contain, so nothing unexpected travels in the body.
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

# Stop an agent already running here before replacing its binary. The service
# name is fixed, so a reinstall could never start a second copy, but without this
# the new binary lands beneath a live process and only the restart at the end
# picks it up. Stopping first also means the copy does not depend on `install`
# unlinking rather than failing with ETXTBSY. Placed after the download, so a
# node that cannot fetch the binary keeps running.
if [ "$INIT" = openrc ]; then
	rc-service monitor-agent stop 2>/dev/null || true
else
	systemctl stop monitor-agent 2>/dev/null || true
fi
install -d -m 0755 "$ROOT"
install -m 0755 "$TMP" "$BIN"

# The token lives in a root-only environment file rather than the unit, keeping
# it out of `systemctl cat` and the world-readable journal. 0600 root is what
# keeps it private, since $ROOT itself is readable and holds the binaries. Set in
# a subshell, because the unit file written below is read by anyone debugging
# with `systemctl cat` and need not be 0600.
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

# The token stays in the root-only env file rather than the service script.
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
# AF_NETLINK is how getifaddrs(3) obtains this host's own addresses from the
# kernel; without it the agent reports none.
RestrictAddressFamilies=AF_INET AF_INET6 AF_NETLINK
MemoryMax=64M

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable monitor-agent >/dev/null
# restart rather than `enable --now`: --now leaves an already-running service
# untouched, so reinstalling over a live agent would keep the old binary
# running.
systemctl restart monitor-agent
echo "monitor-agent installed; follow it with: journalctl -u monitor-agent -f"
