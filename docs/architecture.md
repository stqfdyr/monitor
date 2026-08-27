# 架构

```
┌──────────────┐   WebSocket + JSON-RPC 2.0    ┌─────────────────────┐   HTTP + WS   ┌─────────┐
│    agent     │ ─── Authorization: Bearer ──▶ │        hub          │ ◀──────────── │ 浏览器  │
│  (Linux VPS) │ ◀──── ping.tasks 下发 ─────── │  axum + SQLite      │               │ React   │
└──────────────┘                               │  前端嵌在二进制里   │               └─────────┘
   读 /proc                                    └─────────────────────┘
   无状态                                          monitor.db
```

## 源码地图

| 文件 | 规模 | 职责 |
|---|---|---|
| `agent/src/collect.rs` | ~520 | **全部采集逻辑。** 读 `/proc` 和 `statvfs`。三个数据 bug 的修复都在这里 |
| `agent/src/main.rs` | ~260 | CLI 参数、WebSocket 会话、重连退避、TCP ping |
| `hub/src/db.rs` | ~810 | schema + 所有 SQL。**流量累加 `accumulate()` 在这里** |
| `hub/src/agent_ws.rs` | ~275 | agent 侧 WebSocket、RPC 分发、实时状态 |
| `hub/src/api.rs` | ~405 | 面板和公开页的 HTTP 接口、`Admin` 提取器 |
| `hub/src/auth.rs` | ~330 | session、GitHub OAuth、argon2 密码、登录限流 |
| `hub/src/main.rs` | ~285 | 启动、路由表、静态资源、首次运行、定时清理 |
| `web/src/` | ~1370 | 前端。`components/ui/` 下是 shadcn 生成的，不手改 |

## 线上协议

WebSocket 承载 **JSON-RPC 2.0 通知**（只有 `method` + `params`，没有 `id`，不需要响应）。komari 和 NodeGet 都是这个方案。

### agent → hub

连上之后先发一次 `hello`，之后按 `--interval`（默认 2 秒）持续发 `report`。

| method | params | 何时发 |
|---|---|---|
| `hello` | `Facts`：hostname / os / kernel / arch / virt / cpu_name / cpu_cores / mem_total / swap_total / disk_total / agent_version | 每次连接建立后一次 |
| `report` | `Metrics`：见下 | 每 `interval` 秒 |
| `ping.result` | `{task_id, latency_ms}`，`latency_ms` 为 `-1` 表示连不上 | 每个探测任务按自己的间隔 |

`Metrics` 的字段（`agent/src/collect.rs` 的 `Metrics` struct 就是权威定义）：

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

八张表，`hub/src/db.rs` 顶部的 `SCHEMA` 常量是权威定义。

| 表 | 作用 | 注意 |
|---|---|---|
| `setting` | key/value 配置 | 替代配置文件。见下方设置键列表 |
| `node` | 节点配置 + agent 上报的静态信息 | `token_hash` 是 sha256，明文不存 |
| `traffic` | **单调递增的流量累计** | 1:1 于 node，但生命周期完全不同（每次上报都写） |
| `metric` | 历史明细，**每节点每分钟一行** | `WITHOUT ROWID`，按保留天数定期删 |
| `ping_task` / `ping_node` | 探测任务及其节点分配 | 多对多 |
| `ping_record` | 探测结果 | 同样按保留天数删 |
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
| `release_repo` | `stqfdyr/monitor` | `install.sh` 从哪个仓库拉二进制。目前没有 UI，只能改 DB |

## 请求路径

`hub/src/main.rs` 的路由表是权威定义。

**agent**：`GET /api/agent/ws`（Bearer token）、`GET /install.sh`（公开，不含密钥）

**读取**（登录了看全部，没登录且公开页开着只看公开节点）：
`GET /api/me`、`GET /api/nodes`、`GET /api/nodes/{id}/metrics?hours=N`、`GET /api/ws`（每 2 秒推一次快照）

**登录**：`POST /api/auth/login`、`POST /api/auth/logout`、`GET /api/auth/github`、`GET /api/auth/github/callback`

**面板**（全部要 `Admin` 提取器）：
`POST /api/nodes`、`PUT|DELETE /api/nodes/{id}`、`POST /api/nodes/{id}/token`、`PUT /api/nodes/{id}/traffic`、`GET|POST /api/ping-tasks`、`DELETE /api/ping-tasks/{id}`、`GET|PUT /api/settings`

其余路径 fallback 到嵌入的前端，找不到就返回 `index.html`，让前端路由在刷新时也能工作。

## 实时状态放在内存里

`App.live: RwLock<HashMap<i64, Live>>` 存每个节点的当前指标。hub 重启后会在一个上报周期内重建，所以不落盘。

**在线判定就是 WebSocket 连着**（`App.agents` 里有没有这个 node_id），不做超时心跳推断。断开时同时从 `live` 和 `agents` 里删掉。

## 已知的简化上限

源码里用 `ponytail:` 注释标出来了：

- `hub/src/db.rs` — 单个 SQLite 写连接加互斥锁。几十个节点每 2 秒上报完全够用；真堵了再拆读连接池
- `hub/src/api.rs` — 浏览器实时推送是每个连接自己跑 2 秒定时器，不是广播 fan-out。自用面板没必要
