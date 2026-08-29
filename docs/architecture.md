# 架构

```
┌──────────────┐   WebSocket + JSON-RPC 2.0    ┌─────────────────────┐   HTTP + WS   ┌─────────┐
│    agent     │ ─── Authorization: Bearer ──▶ │        hub          │ ◀──────────── │ 浏览器  │
│  (Linux VPS) │ ◀──── ping.tasks 下发 ─────── │  axum + SQLite      │               │ React   │
└──────────────┘                               │ 后台内置 / 主题可换 │               └─────────┘
   读 /proc                                    └─────────────────────┘
   无状态                                          monitor.db
```

## 三个仓库

| 仓库 | 内容 |
|---|---|
| **monitor**（本仓库） | hub + 内置后台 + `install.sh` |
| **[agent](https://github.com/stqfdyr/agent)** | Linux agent。发布自己的 musl 二进制，`install.sh` 从那边的 release 拉 |
| **[monitor-theme-default](https://github.com/stqfdyr/monitor-theme-default)** | 默认公开页主题。**发布构建产物**（`theme.tar.gz` = `dist/` + `theme.json`），hub 按 `web-theme.pin` 下载校验后嵌入 |

agent 拆开是因为部署机器和发布节奏不同。默认主题拆开是为了让主题拥有独立契约、版本和开发流程；代价是多一个跨仓库依赖——主题得先发布，`web-theme.pin` 才钉得上去。

**hub 消费的是主题的构建产物，不编译主题源码。** 发布的 `theme.tar.gz` 里就是一个可安装的主题目录——
hub 嵌进去的那个文件，和用户解到 `<themes>/<short>/` 的那个是同一个，所以默认主题和第三方主题走
同一套契约。`web-theme.pin` 钉 `<tag> <sha256>`，对不上就构建失败。见 [decisions.md](decisions.md)。

后台不属于主题。`/admin/*` 和登录页始终由 hub 内置的 `web-admin` 提供；主题只负责公开状态页。这样第三方主题不需要重做节点 CRUD、OAuth 和密码设置。

## 源码地图

| 文件 | 规模（不含测试） | 职责 |
|---|---|---|
| `src/db.rs` | ~1060 | schema + 所有 SQL。**流量累加 `accumulate()` 在这里** |
| `src/api.rs` | ~540 | 面板和公开页的 HTTP 接口、`Admin` 提取器 |
| `src/auth.rs` | ~360 | session、GitHub OAuth、argon2 密码、登录限流 |
| `src/main.rs` | ~365 | 启动、路由表、首次运行、定时清理 |
| `src/agent_ws.rs` | ~365 | agent 侧 WebSocket、RPC 分发、实时状态 |
| `src/frontend.rs` | ~180 | 双 SPA、主题扫描、磁盘安全读取与 fallback |
| `web-admin/src/` | ~2320 | 内置后台。`components/ui/` 下是 shadcn 生成的，不手改 |
| `scripts/theme.sh` | ~35 | 按 `web-theme.pin` 下载、校验、解出默认主题到 `target/theme/`。build.rs 和 CI 都调它 |

agent 的采集代码在 [另一个仓库](https://github.com/stqfdyr/agent)。改了它的上报字段就是改了协议，两边要同步。

## 线上协议

WebSocket 承载 **JSON-RPC 2.0 通知**（只有 `method` + `params`，没有 `id`，不需要响应）：一条长连接双向都能主动发，报文自带方法名，用 `curl` 和浏览器控制台就能读。

### agent → hub

连上之后先发一次 `hello`，之后按 `--interval` 持续发 `report`；面板生成的安装命令默认 1 秒。

| method | params | 何时发 |
|---|---|---|
| `hello` | `Facts`：hostname / os / kernel / arch / virt / cpu_name / cpu_cores / mem_total / swap_total / disk_total / agent_version | 每次连接建立后一次 |
| `report` | `Metrics`：见下 | 每 `interval` 秒 |
| `ping.result` | `{task_id, latency_ms}`，`latency_ms` 为 `-1` 表示连不上 | 每个探测任务按自己的间隔 |

`Metrics` 的字段（[agent 仓库](https://github.com/stqfdyr/agent) 里 `src/collect.rs` 的 `Metrics` struct 就是权威定义）：

```
boot_id  uptime  cpu  load[3]
mem_total  mem_used  swap_total  swap_used  disk_total  disk_used
net_rx_total  net_tx_total    ← 内核 lifetime 计数器，hub 负责累加
net_rx  net_tx                ← 瞬时速率 B/s，agent 自己算差值
tcp  udp  procs
```

`boot_id` 来自 `/proc/sys/kernel/random/boot_id`，是 hub 识别 VPS 重启的唯一依据。**不要删。**

### hub → agent

| method | params | 何时发 |
|---|---|---|
| `ping.tasks` | `[{id, target, interval}]` | 连接建立时；面板增删改探测任务时立刻下发 |

agent 收到后会**保留没变化的任务**（同 id + 同 target + 同 interval），只重启变了的，避免每次下发都把所有计时器清零。

## 数据模型

八张表，`src/db.rs` 顶部的 `SCHEMA` 常量是权威定义。库的版本记在 SQLite 自带的
`PRAGMA user_version` 里，当前是 `SCHEMA_VERSION`；迁移只在版本落后时跑一次，见 `db::migrate_to_1`。

| 表 | 作用 | 注意 |
|---|---|---|
| `setting` | key/value 配置 | 替代配置文件。见下方设置键列表 |
| `node` | 节点配置 + agent 上报的静态信息 | `token` 存明文，面板要能重新显示安装命令；只在 `full` 视图输出 |
| `traffic` | **单调递增的流量累计** | 1:1 于 node，但生命周期完全不同（每次上报都写） |
| `metric` | 历史明细，**每节点每分钟一行** | `WITHOUT ROWID`，按保留天数定期删。**一行描述它前面那一分钟，不是它那一瞬**：`net_rx/net_tx` 从累计器差值算出，`cpu`/`mem_used`/`disk_used`/`swap_used`/`tcp`/`udp`/`procs` 是分钟内均值。见 [decisions.md](decisions.md) |
| `ping_task` / `ping_node` | 探测任务及其节点分配 | 多对多 |
| `ping_record` | 探测结果 | 同样按保留天数删。主键是 `(node_id, ts, task_id)`——顺序跟着查询走，见 [benchmark.md](benchmark.md#6-下一轮一条会随时间变慢的查询) |
| `session` | 登录会话 | 存 sha256，14 天过期 |

### 为什么 traffic 单独一张表

因为它的生命周期和 `node` 完全不同：`node` 是用户偶尔改一次的配置，`traffic` 是每 2 秒写一次的热数据。分开还有一个更要紧的原因——**`metric` 可以随便清理而不影响累计流量**，因为累计值不是从明细算出来的。这是设计的一部分，见 [traffic.md](traffic.md)。

### 设置键

| key | 默认 | 说明 |
|---|---|---|
| `site_name` | `Monitor` | 页面标题 |
| `public_page` | 开 | 值为 `off` 时关闭公开状态页 |
| `retention_days` | `30` | 历史明细保留天数，限制在 1–3650 |
| `admin_password_hash` | 首次启动生成 | argon2id |
| `github_client_id` / `github_client_secret` | 空 | OAuth App |
| `github_allowed_users` | 空 | 逗号分隔的用户名白名单。**空 = 任何人都登不进来**（不是任何人都能进） |
| `theme` | `default` | 公开页主题短名；空、无效或已删除时使用内置默认主题 |

## 请求路径

`src/main.rs` 的路由表是权威定义。

**agent**：`GET /api/agent/ws`（Bearer token）、`GET /install.sh`（公开，不含密钥）、
`GET /agent/{arch}`（公开，把 release 二进制从 GitHub 转发给节点）

安装命令默认传 `--interval 1`，也可在 1..3600 内调整；另有 `--github-proxy URL`。

二进制默认走 `<hub>/agent/<arch>`，由 hub 从 GitHub Release 取回再转发——能连上 hub 就能装，
IPv6-only 或者出不去的机器不用再找加速站。`--github-proxy` 是 hub 自己拉不到 release 时的退路，
它让节点直连 GitHub 代理，只拼到下载地址前，不代理 agent 与 hub 的 WebSocket。

`install.sh` 认 systemd 和 OpenRC：前者写 unit（`DynamicUser` + `ProtectSystem` 等加固），
后者写 `/etc/init.d/monitor-agent`，用 `supervise-daemon` 拿到等价的自动重启。两边 token 都只在
root-only 的 `/etc/monitor/agent.env` 里。

**等价的只有自动重启。** OpenRC 那边的 agent 以 root 跑：`DynamicUser` 是 systemd 白送的降权，
OpenRC 没有对应开关，要降权得自己建用户再指过去。agent 并不需要 root（只读 `/proc` 和 `/sys`
里的公开文件，加出站 TCP），所以这是笔可以还的债，不是必须这样。没有顺手加 `command_user=nobody`：
那会把 token 从 root 独占挪进一个共享身份的 `environ`，拿凭证换降权不是明确的净收益。

**读取**（登录了看全部，没登录且公开页开着只看公开节点）：
`GET /api/me`、`GET /api/nodes`、`GET /api/nodes/{id}/metrics?hours=N`、`GET /api/ws`（每 2 秒推一次快照）

`hours` clamp 到 1–2160。**分辨率由屏幕决定**：调用方用 `points` 报上自己能画多少点（设备像素），
`api::sample_step` 只往下调，绝不往上——上限 `SAMPLES = 1440`（一天的分钟数）是 hub 的，不是调用方的。
样本装得下就一个不抽稀，装不下才聚合。`series=metrics|ping` 再决定只要哪一半。
这条路匿名可达，所以代价必须有上界。见 [security.md](security.md) 和
[decisions.md](decisions.md#分辨率由屏幕决定聚合是退让不是默认-用户)。

**登录**：`POST /api/auth/login`、`POST /api/auth/logout`、`GET /api/auth/github`、`GET /api/auth/github/callback`

**面板**（全部要 `Admin` 提取器）：
`POST /api/nodes`、`PUT /api/nodes/order`、`PUT|DELETE /api/nodes/{id}`、`POST /api/nodes/{id}/token`、`PUT /api/nodes/{id}/traffic`、`GET|POST /api/ping-tasks`、`DELETE /api/ping-tasks/{id}`、`GET|PUT /api/settings`、`GET /api/themes`

其余路径按下面顺序处理：

```text
/api/*           未匹配即 404，不回落 SPA
/admin, /admin/* 内置后台；资源位于 /admin/assets/
其它             当前磁盘主题；不可用时回落内置默认主题
```

主题内找不到的路径返回同一主题的 `index.html`，让客户端路由刷新可用。磁盘文件必须在规范化后仍位于 `<themes>/<short>/dist` 内，路径穿越和越界符号链接都会被拒绝。

## 实时状态放在内存里

`App.agents: RwLock<HashMap<i64, Agent>>` 存每条 agent 连接：出站通道、会话号、最新一次上报。
hub 重启后会在一个上报周期内重建，所以不落盘。

**在线判定就是 WebSocket 连着**——`App.agents` 里有没有这个 node_id。握手时写入，断开时删掉，
一张表一个真相。

原来是两张：连接进 `agents`、指标进 `live`，靠每个改动点手工同步。它们已经不一致过——连接在握手时
入表，指标在**首次上报**时才入表，而在线判定读的是 `live`，所以刚连上的节点会离线整整一个
`--interval`（最长 1 小时）。合成一张表以后这类不同步没有地方可以发生。

刚连上还没上报的节点是 `online: true` + `metrics: null`，主题和面板本来就要处理这个组合
（离线节点也是 `metrics: null`）。

连着不等于活着：机器掉进网络黑洞、内核卡死、NAT 表项超时，TCP 连接会停在半开状态，`recv()`
永远不返回，节点就一直是"在线"配一份冻在死亡瞬间的指标——直到内核几小时后放弃这条连接。
所以 `src/agent_ws.rs` 每 30 秒发一个 WebSocket Ping，**任何入站帧**（包括 pong）都算活着的
证据；连续 120 秒一帧不来就主动断开，agent 那边随即重连。判定离线最慢 150 秒。

## 已知的简化上限

源码里用 `ponytail:` 注释标出来了：

- `src/db.rs` — 单个 SQLite 写连接加互斥锁。几十个节点每 2 秒上报完全够用；真堵了再拆读连接池。
  **触发条件是可测的**：匿名可达的 `/api/nodes/{id}/metrics` 实测 24 小时窗口占锁 64 ms，这段时间
  agent 上报排队。面板真的开始堵上报时再拆——WAL 下加一条 `SQLITE_OPEN_READ_ONLY` 连接就够，
  但那会给 `:memory:` 的测试留一条和生产不同的路，现在这个规模不值得
- `src/api.rs` — 浏览器实时推送是每个连接自己跑 2 秒定时器，不是广播 fan-out。定时器背后的快照是共享的（`live_snapshot`，公开/后台各一份，缓存 1.9 秒），所以多开几个标签页只多几次 socket 写，不会把查询量乘上观众数
