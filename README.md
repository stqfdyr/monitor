# monitor

轻量的服务器探针 hub。Rust + SQLite，编译为单个二进制。

节点状态、流量统计、TCP 延迟、续费成本，仅此四项。

## 特性

- 单二进制，零配置启动，全部设置存在 SQLite 里
- 内存与磁盘口径对齐 `free(1)` / `df(1)`
- 总流量跨 VPS 重启、hub 重启、agent 掉线持续累加
- 在线节点过期后，到期日按付款周期自动顺延
- 公开状态页可换主题，主题在运行时从磁盘加载
- GitHub SSO 登录，本地应急密码作为备用入口
- agent 静态链接单文件，支持 systemd 与 OpenRC

不做：通知、远程 SSH、插件系统、ICMP / HTTP 探测。

## 组成

| 仓库 | 说明 |
|---|---|
| [monitor](https://github.com/stqfdyr/monitor) | hub：后台、API、公开页宿主 |
| [agent](https://github.com/stqfdyr/agent) | Linux agent |
| [monitor-theme-default](https://github.com/stqfdyr/monitor-theme-default) | 内置默认主题 |

```
agent (Linux)  ──WebSocket / JSON-RPC 2.0──▶  hub (axum + SQLite)  ──▶  后台 + 状态页
```

## 构建

需要 Rust stable 与 Node.js（Node 只用于构建后台）。

```bash
cd web-admin && npm ci && npm run build && cd ..
cargo build --release
```

后台与默认主题在编译期嵌入二进制。默认主题不需要 clone：`cargo build` 按 `web-theme.pin`
里的 `<tag> <sha256>` 下载主题仓库发布的 `theme.tar.gz`，校验后解到 `target/theme/`。

## 运行

```bash
monitor-hub --site https://monitor.example.com
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--listen` | `0.0.0.0:8080` | 监听地址 |
| `--db` | `monitor.db` | SQLite 路径 |
| `--site` | 由 `--listen` 推导 | 对外地址，决定安装命令与 cookie 的 `Secure` 标志 |
| `--themes` | 数据库同级 `themes/` | 外部主题目录 |

首次启动打印一次性应急密码，用它登录 `/admin`：

1. **设置** 配置 GitHub OAuth（回调 `<site>/api/auth/github/callback`）与允许登录的用户名，修改
   应急密码
2. **节点** 添加节点，点下载按钮生成安装命令，在目标主机执行
3. 拖动手柄排序；展示与流量设置、续费设置分别编辑

```bash
curl -fsSL https://monitor.example.com/install.sh | sh -s -- \
  --server https://monitor.example.com --token <token>
```

agent 二进制由 hub 转发，节点无需直连 GitHub。hub 自身无法访问 GitHub 时，可给安装命令加
`--github-proxy https://ghfast.top`，由节点直连镜像下载。

## 反向代理

置于反向代理之后时，用 `--listen 127.0.0.1:8080` 限制监听，并注意：

- 转发 `/api/agent/ws` 与 `/api/ws` 的 `Upgrade` / `Connection` 头，关闭缓冲，读写超时远大于 60 秒
- 放行 `POST` / `PUT` / `DELETE`
- 透传 `X-Forwarded-For`，否则登录限流会按代理地址计数
- `--site` 填写对外地址，与 `--listen` 无关

## 主题

主题目录复制到 `--themes` 指向的位置后，在后台「主题」页切换，无需重启。选中的主题缺失或损坏时回落到内置默认主题。主题包格式见
[monitor-theme-default](https://github.com/stqfdyr/monitor-theme-default)。

## 安全

- 节点 token 只在登录后的面板视图里出现，公开页拿不到；可随时换发，换发即踢掉旧 agent
- 密码登录按来源地址限流，15 分钟 5 次，另有一道并发闸门
- 公开状态页按节点开关，且不输出 IP、主机名与备注
- OAuth 回调校验 state；session cookie 为 HttpOnly + SameSite=Lax + Secure
- 匿名可达的接口都有明确上界：历史窗口最宽 7 天，agent 二进制转发最多 4 个并发

详见 [docs/security.md](docs/security.md)。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/architecture.md](docs/architecture.md) | 路由表、协议、数据表 |
| [docs/traffic.md](docs/traffic.md) | 流量累加与周期重置 |
| [docs/security.md](docs/security.md) | 鉴权、API 边界、公开页 |
| [docs/decisions.md](docs/decisions.md) | 技术选型与被否决的方案 |
| [docs/development.md](docs/development.md) | 开发流程 |

## 开发

```bash
cargo test
cargo clippy --all-targets
cd web-admin && npm run dev
```

## 许可

MIT
