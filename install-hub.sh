#!/bin/sh
# monitor hub installer.
#
#   curl -fsSL https://raw.githubusercontent.com/stqfdyr/monitor/main/install-hub.sh -o install-hub.sh
#   sudo sh install-hub.sh
#
# Menu-driven when it has a terminal. A plain `curl ... | sh` has no terminal to
# read an answer from, so it installs with the defaults instead of hanging on a
# prompt nobody can see.
set -eu

REPO="stqfdyr/monitor"
SERVICE="monitor-hub"
BIN="/usr/local/bin/monitor-hub"
UNIT="/etc/systemd/system/monitor-hub.service"
# StateDirectory= below puts the database here and hands it to the unit's
# dynamic user, which is why nothing in this script ever chowns anything. Under
# DynamicUser= systemd keeps the real directory at /var/lib/private/monitor and
# leaves this path as a symlink into it for root.
DATA="/var/lib/monitor"
# Stamped into the unit this script writes, and checked before it overwrites
# one. A hub someone set up by hand is a different deployment: replacing its
# unit would swap a loopback listener and a --site for 0.0.0.0 and no TLS,
# and --purge would delete a database this script never created.
MARKER="# managed-by: install-hub.sh"
PORT="28080"
PORT_SET=""
SITE=""
SITE_SET=""
YES=""
PURGE=""
ACTION=""

# ---- ui ----
# Colour only into a terminal, and never against NO_COLOR: the output of a
# piped run belongs in a log, not in escape sequences.
if [ -t 1 ] && [ -z "${NO_COLOR-}" ]; then
	B="$(printf '\033[1m')" D="$(printf '\033[2m')" N="$(printf '\033[0m')"
	G="$(printf '\033[32m')" R="$(printf '\033[31m')" Y="$(printf '\033[33m')"
else
	B="" D="" N="" G="" R="" Y=""
fi

rule() { printf '  %s────────────────────────────────────────────%s\n' "$D" "$N"; }

banner() {
	if [ -t 1 ]; then printf '\033[H\033[2J'; fi
	printf '\n  %smonitor hub%s  %s·%s  安装器\n' "$B" "$N" "$D" "$N"
	rule
	printf '\n'
}

# Every label below is two CJK characters wide on purpose: printf pads by byte
# count, so a label of any other width would break the column.
ok() { printf '  %s✓%s  %s    %s%s%s\n' "$G" "$N" "$1" "$D" "${2-}" "$N"; }
field() { printf '  %s%s%s    %s\n' "$D" "$1" "$N" "$2"; }
warn() { printf '  %s!%s  %s\n' "$Y" "$N" "$1"; }
die() { printf '  %s✗%s  %s\n' "$R" "$N" "$1" >&2; exit 1; }

# A default answer on Enter, and the default itself when there is no terminal.
ask() {
	if [ ! -t 0 ]; then printf '%s' "$2"; return; fi
	printf '  %s?%s  %s %s[%s]%s ' "$Y" "$N" "$1" "$D" "$2" "$N" >&2
	read -r reply || reply=""
	printf '%s' "${reply:-$2}"
}

confirm() {
	if [ -n "$YES" ]; then return 0; fi
	if [ ! -t 0 ]; then die "$1（非交互运行时加 --yes 确认）"; fi
	printf '  %s?%s  %s  %s[y/N]%s ' "$Y" "$N" "$1" "$D" "$N"
	read -r reply || reply=""
	case "$reply" in y | Y | yes) return 0 ;; *) printf '  已取消\n'; return 1 ;; esac
}

press() {
	if [ ! -t 0 ]; then return 0; fi
	printf '\n  %s回车返回菜单%s ' "$D" "$N"
	read -r _ || true
}

check_port() {
	case "$1" in "" | *[!0-9]*) die "端口必须是 1-65535 的整数：$1" ;; esac
	[ "$1" -ge 1 ] && [ "$1" -le 65535 ] || die "端口必须是 1-65535 的整数：$1"
}

# The hub cannot know its own public address behind NAT, so ask the internet.
# A private address is still a better thing to print than nothing, and an
# IPv6-only box gets its literal bracketed so the result is a usable URL.
foreign_unit() {
	[ -f "$UNIT" ] || return 1
	! grep -qF "$MARKER" "$UNIT"
}

refuse_foreign() {
	printf '  %s✗%s  %s 不是这个脚本装的\n' "$R" "$N" "$UNIT" >&2
	printf '     先看一眼它的参数：systemctl cat %s\n' "$D$SERVICE$N" >&2
	printf '     确认可以替换之后删掉那个文件再重跑，否则现有部署的监听地址\n' >&2
	printf '     和 --site 会被这里的默认值覆盖\n' >&2
	exit 1
}

# ---- install ----
install_hub() {
	if foreign_unit; then refuse_foreign; fi
	case "$(uname -m)" in
	x86_64 | amd64) arch=x86_64 ;;
	aarch64 | arm64) arch=aarch64 ;;
	*) die "不支持的架构：$(uname -m)（发布的是 x86_64 与 aarch64）" ;;
	esac
	asset="monitor-hub-$arch-unknown-linux-musl"
	base="https://github.com/$REPO/releases/latest/download"
	ok "架构" "$arch"

	# An upgrade rewrites the unit, so anything not given on the command line
	# has to come back out of the old one. Without this, re-running to upgrade
	# silently resets the port and drops --site, and the reverse proxy in front
	# is then pointing at nothing.
	if [ -z "$PORT_SET" ] || [ -z "$SITE_SET" ]; then
		old_exec="$(sed -n 's/^ExecStart=.*--listen //p' "$UNIT" 2>/dev/null || true)"
		[ -n "$PORT_SET" ] || case "$old_exec" in
		*:[0-9]*) PORT="${old_exec%% *}"; PORT="${PORT##*:}" ;;
		esac
		[ -n "$SITE_SET" ] || case "$old_exec" in
		*--site\ *) SITE="${old_exec##*--site }"; SITE="${SITE%% *}" ;;
		esac
		check_port "$PORT"
	fi

	# A first run is what prints the one-time password, and only a missing
	# database makes one. Checked before anything is installed.
	first=""
	[ -f "$DATA/monitor.db" ] || [ -f /var/lib/private/monitor/monitor.db ] || first=1

	# The tag comes out of GitHub's own redirect for "latest", so there is no
	# API call to be rate-limited and no JSON to parse. Only the FIRST hop
	# carries it -- the chain now ends on release-assets.githubusercontent.com,
	# whose URL has no tag anywhere in it -- so this must not follow redirects.
	# A missing asset still redirects, so it is the download below that catches
	# an architecture that was never published.
	tag="$(curl -fsSI -o /dev/null -w '%{redirect_url}' "$base/$asset" 2>/dev/null |
		sed -n 's#.*/download/\([^/]*\)/.*#\1#p')" || true
	[ -n "$tag" ] || die "查不到最新发布版；GitHub 不可达，或还没有任何发布"
	ok "版本" "$tag"

	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' EXIT
	curl -fsSL --max-time 300 "$base/$asset" -o "$tmp/$asset" || die "二进制下载失败：$base/$asset"
	ok "下载" "$(du -h "$tmp/$asset" | cut -f1)"

	# Verified against the release's own checksum file, so a truncated transfer
	# or a swapped asset is caught before anything lands in /usr/local/bin.
	curl -fsSL --max-time 30 "$base/sha256sums.txt" -o "$tmp/sums" ||
		die "校验文件下载失败；这个版本可能早于 sha256sums.txt，请改用手动安装"
	want="$(sed -n "s/^\([0-9a-f]\{64\}\)  *$asset\$/\1/p" "$tmp/sums")"
	[ -n "$want" ] || die "sha256sums.txt 里没有 $asset 这一项"
	got="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
	[ "$got" = "$want" ] || die "校验不通过，已丢弃下载的文件。期望 $want，实得 $got"
	ok "校验" "sha256 一致"

	# Keep the old binary until the new one has proved it starts: a failed
	# upgrade has to leave a running hub behind, not a dead service.
	backup=""
	if [ -f "$BIN" ]; then
		backup="$BIN.old"
		cp -f "$BIN" "$backup"
	fi
	# Stopped first so the copy does not have to land underneath a live
	# process, and so the port check below is not tripped by the hub itself.
	systemctl stop "$SERVICE" 2>/dev/null || true
	install -m 0755 "$tmp/$asset" "$BIN"

	if command -v ss >/dev/null 2>&1 && ss -ltnH "sport = :$PORT" 2>/dev/null | grep -q .; then
		die "端口 $PORT 已被其它程序占用，换一个：--port <n>"
	fi

	# Loopback only: the panel and the agent tokens never travel a network in
	# the clear, and there is no port to firewall. Reaching it is the reverse
	# proxy's job, and 127.0.0.1 rather than [::1] because that is what every
	# proxy's default upstream is -- the hub binds one address, not both.
	args="--listen 127.0.0.1:$PORT --db $DATA/monitor.db"
	[ -z "$SITE" ] || args="$args --site $SITE"
	cat >"$UNIT" <<UNIT
[Unit]
Description=monitor hub
$MARKER
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$BIN $args
Restart=always
RestartSec=5
# The database and the themes/ directory beside it live here. systemd creates
# it and hands it to the dynamic user, so no chown is needed anywhere.
StateDirectory=monitor
WorkingDirectory=$DATA
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
RestrictAddressFamilies=AF_INET AF_INET6
MemoryMax=256M

[Install]
WantedBy=multi-user.target
UNIT

	systemctl daemon-reload
	systemctl enable "$SERVICE" >/dev/null 2>&1 || true
	systemctl restart "$SERVICE"
	# is-active answers before a unit that exits immediately has exited, and
	# the first run also has an argon2 hash to compute. Settle, then ask.
	sleep 3
	if ! systemctl is-active --quiet "$SERVICE"; then
		if [ -n "$backup" ]; then
			install -m 0755 "$backup" "$BIN"
			rm -f "$backup"
			systemctl restart "$SERVICE" 2>/dev/null || true
			die "新版本没能启动，已回滚到上一版。日志：journalctl -u $SERVICE -n 50"
		fi
		die "服务启动失败。日志：journalctl -u $SERVICE -n 50"
	fi
	rm -f "$BIN.old"
	ok "服务" "已启动并开机自启"

	if [ -n "$first" ]; then done_title="安装完成"; else done_title="升级完成"; fi
	printf '\n  %s%s%s\n' "$B" "$done_title" "$N"
	rule
	printf '\n'
	field "面板" "${SITE:-http://127.0.0.1:$PORT}/admin"
	if [ -n "$first" ]; then
		pw="$(journalctl -u "$SERVICE" --since '-2 min' --no-pager 2>/dev/null |
			sed -n 's/.*Emergency password: //p' | tail -1)"
		if [ -n "$pw" ]; then
			field "密码" "$pw"
			field "    " "${D}只显示这一次，登录后到「设置」里改掉${N}"
		else
			field "密码" "journalctl -u $SERVICE | grep Emergency"
		fi
	fi
	field "数据" "$DATA/monitor.db"
	field "服务" "systemctl status $SERVICE"
	field "日志" "journalctl -u $SERVICE -f"
	printf '\n'

	# The hub is on loopback, so this is not optional advice -- it is the
	# remaining half of the install. Deliberately does not mention --site: the
	# panel builds install commands from the browser's own address, so once the
	# domain works, everything downstream is already right.
	if [ -z "$SITE" ]; then
		printf '  %s还差一步：配个反向代理%s\n' "$B" "$N"
		printf '     面板只监听本机，公网访问不到——这是故意的，凭证不会在链路上裸奔。\n'
		printf '     用 nginx / caddy / cf tunnel 等配置完反向代理后，用域名访问面板，\n'
		printf '     我相信这难不倒你。\n\n'
		printf '     %s完整配置和注意事项见 README 的「反向代理」一节。%s\n' "$D" "$N"
	fi
}

# ---- uninstall ----
uninstall_hub() {
	if [ ! -f "$BIN" ] && [ ! -f "$UNIT" ]; then
		# The data outlives the unit, so --purge still has a job here. The
		# marker went with the unit, so ownership is inferred from the shape
		# this script leaves: StateDirectory= under DynamicUser= puts the real
		# directory in /var/lib/private and makes $DATA a symlink to it. A
		# hand-rolled deployment has a real directory at $DATA and no twin,
		# and that is someone else's database.
		if [ -n "$PURGE" ] && [ -e "$DATA" ]; then
			[ -L "$DATA" ] && [ -d /var/lib/private/monitor ] ||
				die "$DATA 不是这个脚本留下的形状，不敢删；请自己确认后手动删除"
			confirm "服务已经卸载了。删除 $DATA 下的数据库？不可撤销" || return 0
			rm -rf "$DATA" /var/lib/private/monitor
			ok "数据" "已删除"
			return 0
		fi
		[ ! -e "$DATA" ] || die "服务已经卸载了，数据还留在 $DATA；要一并删掉就加 --purge"
		die "这台机器上没有装 monitor hub"
	fi
	if foreign_unit; then refuse_foreign; fi
	# Deleting a database is the one step nothing here can undo, so it is
	# allowed only against a deployment this script owns.
	if [ -n "$PURGE" ] && ! grep -qsF "$MARKER" "$UNIT"; then
		die "只有这个脚本装的部署才能 --purge；别处的数据请自己确认后手动删"
	fi
	if [ -n "$PURGE" ]; then
		confirm "卸载 monitor hub，并删除 $DATA 下的数据库？不可撤销" || return 0
	else
		confirm "卸载 monitor hub？数据保留在 $DATA" || return 0
	fi
	systemctl disable --now "$SERVICE" 2>/dev/null || true
	rm -f "$UNIT" "$BIN" "$BIN.old"
	systemctl daemon-reload
	ok "服务" "已移除"
	if [ -n "$PURGE" ]; then
		# Both paths: $DATA is the symlink systemd leaves behind, and the real
		# directory under DynamicUser= is the private one.
		rm -rf "$DATA" /var/lib/private/monitor
		ok "数据" "已删除"
	else
		field "数据" "保留在 $DATA，重新安装会直接接着用"
	fi
}

menu() {
	while :; do
		banner
		printf '    1  安装 / 升级\n'
		printf '    2  卸载\n'
		printf '    3  状态\n'
		printf '    4  日志\n'
		printf '    q  退出\n\n'
		printf '  %s›%s ' "$B" "$N"
		read -r choice || exit 0
		printf '\n'
		case "$choice" in
		1)
			PORT="$(ask "监听端口" "$PORT")"
			check_port "$PORT"
			printf '\n'
			install_hub
			press
			;;
		2) uninstall_hub; press ;;
		3) systemctl status "$SERVICE" --no-pager || true; press ;;
		4) journalctl -u "$SERVICE" -f --no-pager ;;
		q | Q | exit | "") exit 0 ;;
		*) ;;
		esac
	done
}

usage() {
	cat <<TXT
monitor hub 安装器

  sudo sh install-hub.sh                有终端时给菜单，否则按默认安装
  sudo sh install-hub.sh --port 8443    指定端口安装
  sudo sh install-hub.sh --uninstall    卸载，保留数据
  sudo sh install-hub.sh --purge        卸载并删除数据库

  --port <n>     本机监听端口，默认 $PORT
  --site <url>   一般不用填。面板拼安装命令用的是浏览器地址栏，配好反代
                 用域名访问就自动对了。只有两种情况要填：你进面板的地址
                 不是节点能用的地址（比如走 SSH 隧道），或反代不发
                 X-Forwarded-Proto
  --yes, -y      跳过确认
  --help, -h     显示这段

hub 只监听 127.0.0.1，公网访问不到，需要自己配 nginx / caddy / CF 隧道把
域名指过来。装完会打印具体怎么配。

重跑一次就是升级：校验通过后才替换二进制，起不来会自动回滚到上一版；
没写的参数沿用上次的，所以升级不会把端口和 --site 冲掉。
数据固定在 $DATA，卸载默认保留。

已经有一个手工部署的 monitor-hub 时，这个脚本会拒绝动它——它写的服务单元带
自己的标记，认不出标记就不覆盖，免得把你的监听地址和 --site 换成默认值。
TXT
}

while [ $# -gt 0 ]; do
	case "$1" in
	--port) PORT="${2-}"; PORT_SET=1; shift 2 ;;
	--site) SITE="${2-}"; SITE_SET=1; shift 2 ;;
	--uninstall) ACTION=uninstall; shift ;;
	--purge) ACTION=uninstall; PURGE=1; shift ;;
	--yes | -y) YES=1; shift ;;
	-h | --help) usage; exit 0 ;;
	*) die "未知参数：$1（--help 看用法）" ;;
	esac
done

check_port "$PORT"
case "$SITE" in "" | http://* | https://*) ;; *) die "--site 要以 http:// 或 https:// 开头" ;; esac
[ "$(id -u)" = 0 ] || die "需要 root：sudo sh $0"
command -v curl >/dev/null 2>&1 || die "需要 curl"
command -v sha256sum >/dev/null 2>&1 || die "需要 sha256sum（装 coreutils）"
command -v systemctl >/dev/null 2>&1 ||
	die "这个安装器只装 systemd 服务。手动运行：$BIN --listen 127.0.0.1:$PORT --db $DATA/monitor.db"

case "$ACTION" in
uninstall) banner; uninstall_hub ;;
*)
	if [ -t 0 ]; then
		menu
	else
		banner
		install_hub
	fi
	;;
esac
