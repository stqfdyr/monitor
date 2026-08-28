# 批量升级 agent

这份文档是给**执行者**看的：一个人，或者一个能 ssh 到这些机器的 AI。照着做就行，不需要读这个仓库的其它部分。

开始之前，把 `HUB` 换成面板地址（例如 `https://monitor.example.com`），后面每段命令开头都有这一行。

## 结论先行

**升级一台已经装好的 agent，只需要换掉那个二进制再重启。**

- **不需要 token**。节点上 `/etc/monitor/agent.env` 里已经有了，换二进制不碰它
- **不需要重跑 `install.sh`**，也就不需要去面板一台台复制安装命令
- **不需要节点能访问 GitHub**。二进制由 hub 转发：`GET <HUB>/agent/x86_64`（或 `/agent/aarch64`）

例外情况见最后一节「什么时候必须重跑 install.sh」。

## 前提

- 能用 root ssh 到每台节点（本机的 `~/.ssh/config` 里通常已经配好别名）
- 知道要升哪些机器。面板的「节点」页是权威列表；`~/.ssh/config` 里的别名不一定和节点名一一对应（比如节点叫 `Zouter`，ssh 别名可能是 `zt`）
- 节点上有 `curl`

## 升一台

把 `<HOST>` 换成 ssh 别名或 `root@地址`：

```bash
ssh <HOST> 'sh -s' <<'REMOTE'
set -eu
HUB=https://monitor.example.com

[ -x /usr/local/bin/monitor-agent ] || { echo "$(hostname): 没装 agent，跳过"; exit 0; }
case "$(uname -m)" in aarch64 | arm64) A=aarch64 ;; *) A=x86_64 ;; esac

NEW=/usr/local/bin/.monitor-agent.new
# 走哪条路径退出都清掉它，包括 curl 中途失败、ssh 断开这种半路退出。
# 成功那条路径上它已经被 mv 走了，这里的 rm 是空操作。
trap 'rm -f "$NEW"' EXIT

curl -fsSL --max-time 120 "$HUB/agent/$A" -o "$NEW"
chmod 0755 "$NEW"

# 先证明下载到的是这台机器能跑的 agent，再动正在用的那个。
# 截断的文件、代理返回的 HTML 错误页、错的架构，都会停在这一步。
# 不要省掉这三行。
"$NEW" --help 2>&1 | head -1 | grep -q '^monitor-agent ' || {
	echo "$(hostname): 下载到的不是 agent，没动"; exit 1
}

OLD=$(/usr/local/bin/monitor-agent --help 2>&1 | head -1)
if cmp -s "$NEW" /usr/local/bin/monitor-agent; then
	echo "$(hostname): 已是最新 ($OLD)"; exit 0
fi

if command -v systemctl >/dev/null; then
	systemctl stop monitor-agent
	mv "$NEW" /usr/local/bin/monitor-agent
	systemctl start monitor-agent
else
	rc-service monitor-agent stop
	mv "$NEW" /usr/local/bin/monitor-agent
	rc-service monitor-agent start
fi
echo "$(hostname): $OLD -> $(/usr/local/bin/monitor-agent --help 2>&1 | head -1)"
REMOTE
```

这段是幂等的：版本没变就什么都不做，不会白重启一次。

**不留旧版本备份。** `mv` 直接把新的盖上去，旧二进制随之消失；节点上升级前后都只有
`/usr/local/bin/monitor-agent` 一个文件。要退回旧版本，从 GitHub 下指定 tag，见下面
[「升完之后节点在面板上一直不上线」](#升完之后节点在面板上一直不上线)。

**临时文件放在 `/usr/local/bin/` 下而不是 `/tmp`**，因为上面那步冒烟测试要执行它，而加固过的机器 `/tmp` 是 `noexec`。

## 升全部

同一段脚本，并发跑。总耗时等于最慢的一台，七台大约三秒：

```bash
HOSTS="zt ccs cc shuo han"        # 换成你的 ssh 别名

for h in $HOSTS; do
	ssh -o ConnectTimeout=10 "$h" 'sh -s' <<'REMOTE' &
# ↑ 把上一节 REMOTE 里的内容原样贴进来
REMOTE
done
wait
```

每台只输出一行，不会互相插花。预期输出形如：

```
ccs: 已是最新 (monitor-agent 0.1.4)
cc: monitor-agent 0.1.3 -> monitor-agent 0.1.4
han: 已是最新 (monitor-agent 0.1.4)
```

## 验证

agent 掉线到重连大约 10 秒。等一会儿再查：

```bash
curl -s <HUB>/api/nodes | grep -o '"name":"[^"]*"\|"agent_version":"[^"]*"\|"online":[a-z]*'
```

装了 `jq` 的话更清楚：

```bash
curl -s <HUB>/api/nodes | jq -r '.nodes[] | "\(.name)\t\(.online)\t\(.agent_version)"'
```

**完成的标准**：每台都 `online: true`，`agent_version` 都是新版本。这个接口不需要登录。

## 出问题了

### ssh 连不上：`Network is unreachable`

`~/.ssh/config` 里那台的 `HostName` 是 IPv6 地址，而**你现在这台机器没有 IPv6 出网**。两个办法：

- 换一台有 IPv6 的机器来跑
- 用它的 IPv4 地址：面板「节点」页的 IP 列同时列出了 v4 和 v6（agent 自己上报的），改用 `root@<IPv4>` 加对应端口即可

### `下载到的不是 agent，没动`

节点上的 agent **没有被动过**，还在跑原来的版本。去查下载源：

```bash
curl -sI <HUB>/agent/x86_64          # 期望 200 与 application/octet-stream
```

通常是 hub 拉不到 GitHub release（hub 是从 `releases/latest` 转发的），或者这个版本压根没发布对应架构的产物。

### 升完之后节点在面板上一直不上线

先看日志：

```bash
ssh <HOST> 'journalctl -u monitor-agent -n 50 --no-pager'
```

要**退回上一个版本**（把 `v0.1.3` 换成想要的 tag）：

```bash
ssh <HOST> 'set -eu
curl -fsSL https://github.com/stqfdyr/agent/releases/download/v0.1.3/monitor-agent-$(uname -m)-unknown-linux-musl -o /tmp/old
systemctl stop monitor-agent
install -m 0755 /tmp/old /usr/local/bin/monitor-agent
systemctl start monitor-agent
rm -f /tmp/old
/usr/local/bin/monitor-agent --help 2>&1 | head -1'
```

节点连不上 GitHub 的话，把上面的地址换成 `<HUB>/agent/<arch>`——但那个转发的永远是**最新版**，退不了版。

## 什么时候必须重跑 install.sh

只有这三种情况，换二进制不够：

1. **全新的机器**，还没装过
2. **要改上报间隔**（`--interval`）——它写在 systemd unit 的 `ExecStart` 里
3. **unit 文件本身变了**（本仓库改了 `install.sh` 里的服务定义，比如加了新的沙箱选项）

这时候去面板点节点的下载按钮，复制那条命令原样执行。重装是幂等的：服务名固定，不会跑出第二个进程，token 也不会因为重装而改变。

## 明确不要做的

- **不要为了升级去换发凭证**。换发会让旧凭证立刻作废，正在跑的 agent 立刻掉线，只有在凭证可能泄露时才需要
- **不要在节点上装定时器做自动更新**。这个项目明确不做自动更新，理由见 [decisions.md](decisions.md)
- **不要省掉冒烟测试那几行**。它是"下错东西不会把正在跑的 agent 换掉"的唯一保障
