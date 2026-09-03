# monitor

轻量的服务器探针 hub。Rust + SQLite，编译为单个二进制。

节点状态、流量统计、TCP 延迟、续费成本，仅此四项。

## 特性

- 单二进制，零配置启动，`http://<IP>:28080` 直接用，全部设置存在 SQLite 里
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

## 安装

```bash
curl -fsSL https://raw.githubusercontent.com/stqfdyr/monitor/main/install-hub.sh -o install-hub.sh
sudo sh install-hub.sh
```

有终端时给一个菜单（安装 / 升级、卸载、状态、日志）；`curl ... | sh` 没有终端可读答案，直接按默认装。
装完打印面板地址和一次性应急密码，浏览器打开 `http://<服务器 IP>:28080/admin` 即可登录。

脚本做的事：核对 release 的 `sha256sums.txt` 之后才把二进制放进 `/usr/local/bin`，写一个
`DynamicUser=yes` 的 systemd 单元，数据固定在 `/var/lib/monitor`。**重跑一次就是升级**——校验通过才
替换，起不来自动回滚到上一版。

| 参数 | 说明 |
|---|---|
| `--port <n>` | 监听端口，默认 `28080` |
| `--site <url>` | 反向代理后的对外地址，直接用 ip:port 时不填 |
| `--uninstall` | 卸载，数据保留在 `/var/lib/monitor` |
| `--purge` | 卸载并删除数据库 |

## 运行

不用脚本的话，二进制自己就能跑：

```bash
monitor-hub                    # 0.0.0.0:28080，数据库 ./monitor.db
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--listen` | `0.0.0.0:28080` | 监听地址 |
| `--db` | `monitor.db` | SQLite 路径 |
| `--site` | 空 | 对外地址，只有反向代理场景需要，见下 |
| `--themes` | 数据库同级 `themes/` | 外部主题目录 |

不带 `--site` 时，hub 不假设自己的地址：面板用浏览器地址栏里的地址拼安装命令，cookie 的 `Secure`
标志看请求的 `X-Forwarded-Proto`。**裸 ip:port 部署是明文 HTTP**，会话与节点凭证在链路上都是明文，
生产环境请放到 TLS 反向代理后面。

## 接入节点

首次启动打印一次性应急密码，用它登录 `/admin`：

1. **设置** 配置 GitHub OAuth（回调 `<面板地址>/api/auth/github/callback`）与允许登录的用户名，
   修改应急密码
2. **节点** 添加节点，点下载按钮生成安装命令，在目标主机执行
3. 拖动手柄排序；展示与流量设置、续费设置分别编辑

```bash
curl -fsSL https://monitor.example.com/install.sh | sh -s -- \
  --server https://monitor.example.com --token <token>
```

agent 二进制由 hub 转发，节点无需直连 GitHub。hub 自身访问不了 GitHub 时，在**设置 → 站点**里填一个
GitHub 代理（如 `https://ghfast.top`），hub 拉 release 时会用它，节点侧不用改任何东西。

hub 只有明文 HTTP 时，命令里会多一个 `--insecure`——agent 和 `install.sh` 默认拒绝明文连远程 hub，
因为凭证会明文传输，装 agent 下载的二进制也走同一条未验证的通道。面板会把这件事标出来。上了 TLS
之后（`--site https://...`）命令自动不再带它。

## Docker

```bash
docker run -d --name monitor -p 28080:28080 \
  -v monitor-data:/data -e TZ=Asia/Shanghai \
  ghcr.io/stqfdyr/monitor
```

`FROM scratch` 里放同一个 musl 二进制加一份 zoneinfo，约 10 MB，以 uid 65534 运行，数据库和主题
目录都在 `/data`。首次启动的应急密码在 `docker logs monitor` 里。

**`TZ` 必须设成 hub 所在的时区。** 日流量和账单周期按本地日期切换，不设按 UTC 算——到点不归零，
也不会有任何报错。

挂主机目录代替 named volume 时，先 `chown 65534:65534`。

## 反向代理

置于反向代理之后时，用 `--listen 127.0.0.1:28080` 限制监听，并注意：

- 转发 `/api/agent/ws` 与 `/api/ws` 的 `Upgrade` / `Connection` 头，关闭缓冲，读写超时远大于 60 秒
- 放行 `POST` / `PUT` / `DELETE`
- 透传 `X-Forwarded-For`，否则登录限流会按代理地址计数
- **填 `--site`**：反代后面板常常是通过回环端口访问的，不填会让安装命令指向 `127.0.0.1`

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
