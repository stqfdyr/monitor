# monitor

一个轻量的服务器探针：一个 hub，多个 agent。Rust 编写，前端 React + shadcn/ui。

只做四件事：看状态、看流量、看延迟、算成本。没有通知、没有远程 SSH、没有插件系统。

## 为什么不是 komari

三处数据口径上的差异：

| | komari / Scout | monitor |
|---|---|---|
| 内存 | `total - free`，把 page cache 算成已用 | `total - MemAvailable`，与 `free -h` 的 used 列一致 |
| 硬盘 | `total - available`，把 ext4 的 5% 保留块算成已用 | `total - f_bfree`，与 `df` 完全一致 |
| 总流量 | agent 上报网卡计数器，VPS 一重启就归零 | hub 侧累加，用 `boot_id` 识别重启，一直往上加 |

另外多了一个「本月流量」：按商家的重置日单独计一份，用圆环显示还剩多少额度。这和总流量是两回事。

## 架构

```
agent (Linux)  ──WebSocket + JSON-RPC 2.0──▶  hub (axum + SQLite)  ──▶  面板 / 公开状态页
   读 /proc                Bearer token              内嵌前端，单文件
```

- **agent** 只支持 Linux，直接读 `/proc` 和 `statvfs`，不依赖 sysinfo。静态链接后是一个几 MB 的单文件。
- **hub** 零配置启动。除了监听地址和数据库路径，其它全在面板里配、存 SQLite。前端构建产物嵌进二进制。
- **通信** WebSocket 承载 JSON-RPC 2.0 通知，token 走 `Authorization` 头（不进反代日志）。

## 跑起来

```bash
# 前端
cd web && npm ci && npm run build && cd ..

# hub
cargo build --release
./target/release/monitor-hub --listen 0.0.0.0:8080 --site https://hub.example.com
```

首次启动会打印一次性应急密码。用它登录 `/admin`，然后：

1. **设置** 里配好 GitHub OAuth（回调填 `https://hub.example.com/api/auth/github/callback`）和允许登录的用户名，改掉应急密码
2. **节点** 里添加一台机器，填价格、到期、每月流量额度和重置日
3. 复制弹出的安装命令，粘到目标 VPS 上执行

```bash
curl -fsSL https://hub.example.com/install.sh | sh -s -- --server https://hub.example.com --token xxx
```

装完是一个 systemd 服务，token 存在 `/etc/monitor/agent.env`（0600，不进 journal）。

`--site` 请在上了 TLS 之后设成 https 地址：它决定安装命令里的地址，也决定 session cookie 要不要带 `Secure`。

## 延迟监控

只有 TCP ping（砍掉了 ICMP 和 HTTP）。在 **延迟监控** 里加一个目标 `host:port`，勾选要跑的节点，改动会立刻下发到在线 agent，不用等重连。

## 安全

- GitHub SSO 为主，本地 argon2 密码为辅——GitHub 挂了不至于把自己锁在外面
- 密码登录按来源地址限流（15 分钟 5 次）
- 节点 token 只存 sha256，明文只在创建时显示一次，可随时重新生成
- 公开状态页可按节点开关，且永远不会输出 IP、主机名和备注
- OAuth 回调校验 state；session cookie 是 HttpOnly + SameSite=Lax + Secure

## 开发

```bash
cargo test                      # 38 个测试
cd web && npm run dev           # 前端热更新，API 代理到 127.0.0.1:9911
```

## 许可

MIT
