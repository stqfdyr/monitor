# monitor

轻量的服务器探针 hub。Rust + SQLite，编译为单个二进制。

节点状态、流量统计、TCP 延迟、续费成本，仅此四项。

## 特性

- 单二进制，零配置启动，`http://<IP>:28080` 直接用，全部设置存在 SQLite 里
- 内存与磁盘口径对齐 `free(1)` / `df(1)`
- 总流量跨 VPS 重启、hub 重启、agent 掉线持续累加
- 在线节点过期后，到期日按付款周期自动顺延
- 公开状态页可换主题，主题在运行时从磁盘加载
- 数据库一键导出备份、导入恢复、回收空间
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
装完打印一次性应急密码，**记下来**——hub 只监听 `127.0.0.1`，得先配好反向代理才能打开面板，
脚本最后会把 nginx / caddy / CF 隧道三种配法打出来。密码事后也能找回：
`journalctl -u monitor-hub | grep Emergency`。

脚本做的事：核对 release 的 `sha256sums.txt` 之后才把二进制放进 `/usr/local/bin`，写一个
`DynamicUser=yes` 的 systemd 单元，数据固定在 `/var/lib/monitor`。**重跑一次就是升级**——校验通过才
替换，起不来自动回滚到上一版，没写的参数沿用上次的。

| 参数 | 说明 |
|---|---|
| `--port <n>` | **本机**监听端口，默认 `28080` |
| `--site <url>` | 一般不用填，见「反向代理」 |
| `--uninstall` | 卸载，数据保留在 `/var/lib/monitor` |
| `--purge` | 卸载并删除数据库 |

## 运行

不用脚本的话，二进制自己就能跑：

```bash
monitor-hub                    # 0.0.0.0:28080，数据库 ./monitor.db
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--listen` | `[::]:28080` | 监听地址。一个 socket 同时收 IPv6 与 IPv4 |
| `--db` | `monitor.db` | SQLite 路径 |
| `--site` | 空 | 对外地址，只有反向代理场景需要，见下 |
| `--themes` | 数据库同级 `themes/` | 外部主题目录 |

不带 `--site` 时，hub 不假设自己的地址：面板用浏览器地址栏里的地址拼安装命令，cookie 的 `Secure`
标志看请求的 `X-Forwarded-Proto`。**裸 ip:port 部署是明文 HTTP**，会话与节点凭证在链路上都是明文，
生产环境请放到 TLS 反向代理后面。

默认监听 `[::]:28080`，Linux 上一个 v6 socket 通过 v4-mapped 地址同时收两族，所以双栈机器不用配
任何东西；内核禁用了 IPv6 或 `bindv6only=1` 时自动退回 `0.0.0.0:28080`。**这件事只影响谁连得上**：
纯 v4 的 hub 收不到只有 IPv6 的节点，反之亦然。

## 接入节点

首次启动打印一次性应急密码，用它登录 `/admin`：

1. **安全** 配置 GitHub OAuth（回调 `<面板地址>/api/auth/github/callback`）与允许登录的用户名，
   修改应急密码
2. **节点** 添加节点，点下载按钮生成安装命令，在目标主机执行
3. 拖动手柄排序；展示与流量设置、续费设置分别编辑

```bash
curl -fsSL https://monitor.example.com/install.sh | sh -s -- \
  --server https://monitor.example.com --token <token>
```

机器多的时候不必一台一台添加：**节点 → 批量添加**开一个一小时的注册窗口，期间下面这条命令在
任意机器上跑一次，那台机器就会带着自己的 hostname 出现在列表里，各自拿到各自的 token。

```bash
curl -fsSL https://monitor.example.com/install.sh | sh -s -- \
  --server https://monitor.example.com --register <key>
```

这条命令里没有任何一台机器的凭证，所以可以直接进 `for` 循环、ansible 或者开机脚本。窗口到点自
动失效，一个窗口最多注册 100 台；重跑同一条命令不会重复添加——机器上已经有 token 就直接沿用。

agent 二进制由 hub 转发，节点无需直连 GitHub。hub 自身访问不了 GitHub 时，在**设置 → 站点**里填一个
GitHub 代理（如 `https://ghfast.top`），hub 拉 release 时会用它，节点侧不用改任何东西。

hub 只有明文 HTTP 时，命令里会多一个 `--insecure`——agent 和 `install.sh` 默认拒绝明文连远程 hub，
因为凭证会明文传输，装 agent 下载的二进制也走同一条未验证的通道。面板会把这件事标出来。上了 TLS
之后命令自动不再带它。

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

**`install-hub.sh` 装出来的 hub 只监听 `127.0.0.1`，公网访问不到**——凭证不会在链路上明文传输，
也没有端口需要防火墙。把域名指过来是反向代理的活。

**配好之后不用改 hub 的任何参数，安装命令会自己变。** 面板用浏览器地址栏的地址拼命令，所以你改用
`https://hub.example.com` 进后台，命令立刻变成 `--server https://hub.example.com` 并去掉
`--insecure`；会话 cookie 的 `Secure` 跟着请求的 `X-Forwarded-Proto` 走。hub 也不用重启。

### caddy

证书、`X-Forwarded-Proto`、WebSocket 都自动处理，一行就够：

```caddyfile
hub.example.com {
    reverse_proxy 127.0.0.1:28080
}
```

### nginx

```nginx
map $http_upgrade $connection_upgrade { default upgrade; '' close; }

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    server_name hub.example.com;
    ssl_certificate     /etc/letsencrypt/live/hub.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/hub.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:28080;
        proxy_http_version 1.1;
        # 导入备份和上传主题是分片传的，单片 4 MiB，所以这个数只跟分片
        # 大小有关，跟数据库多大无关。其余路径 hub 自己卡在 64 KiB。
        client_max_body_size 8m;
        proxy_set_header Host              $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        # WebSocket：/api/agent/ws 与 /api/ws 是长连接
        proxy_set_header Upgrade    $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_buffering off;
        proxy_read_timeout  1h;
        proxy_send_timeout  1h;
    }
}
```

### Cloudflare 隧道

不用开任何入站端口，纯 IPv4 的机器也能拿到双栈入口（`cloudflared` 的隧道是出站建立的）：

```yaml
ingress:
  - hostname: hub.example.com
    service: http://127.0.0.1:28080
  - service: http_status:404
```

**在边缘做了路径白名单的话，升级 hub 时记得同步**：新版本加的路由会被上一版的名单挡在外面，
而且 403 来自边缘，hub 侧一行日志都没有。面板用到的全部路径见
[docs/architecture.md](docs/architecture.md) 的路由表；这一版新增的是 `POST /api/themes`（上传主题）
和 `DELETE /api/themes/{short}`。

### 几条通用注意

- 转发 `Upgrade` / `Connection` 头，关闭缓冲，读写超时远大于 60 秒，否则节点会周期性掉线
- **请求体上限只需要 8 MiB**：导入备份和上传主题都是分片传的（单片 4 MiB，hub 侧单请求硬上限
  8 MiB），所以这个数不随数据库增长——256 MiB 的备份也是 64 个 4 MiB 的请求。nginx 默认
  `client_max_body_size 1m` 仍然拦得住一片，要改成 `8m`（caddy 默认不限）；Cloudflare 免费版
  100 MB 的上传上限则不再是天花板，单个请求离它差一个数量级
- 放行 `POST` / `PUT` / `DELETE`
- 透传 `X-Forwarded-Proto`，否则会话 cookie 拿不到 `Secure`
- 透传 `X-Forwarded-For`，否则登录限流会按代理地址计数

手工部署（不走 `install-hub.sh`）时记得自己加 `--listen 127.0.0.1:28080`，否则明文端口对外可达，
绕过反代就能明文访问。

### 什么时候才需要 `--site`

只有这两种情况，常规反代都不属于：

- **你进后台的地址不是节点该用的地址**。比如监听回环之后又通过 SSH 隧道进面板，浏览器地址栏是
  `127.0.0.1`，节点却得连公网域名——这时命令是错的，得用 `--site` 钉死。
- **反代不发 `X-Forwarded-Proto`**。cookie 拿不到 `Secure`，用 `--site https://...` 强制。

## 主题

主题目录复制到 `--themes` 指向的位置后，在后台「主题」页切换，无需重启。选中的主题缺失或损坏时回落到内置默认主题。主题包格式见
[monitor-theme-default](https://github.com/stqfdyr/monitor-theme-default)。

主题的 `theme.json` 里 `url` 指向 GitHub 仓库时，卡片上的 ⟳ 从该仓库最新的 release 取 `theme.tar.gz`
装上；tag 和已装版本相同就不下载。走「设置」里的 GitHub 代理。

## 安全

- 节点 token 只在登录后的面板视图里出现，公开页拿不到；可随时换发，换发即踢掉旧 agent
- 密码登录按来源地址限流，15 分钟 5 次，另有一道并发闸门
- 公开状态页按节点开关，且不输出 IP、主机名与备注
- OAuth 回调校验 state；session cookie 为 HttpOnly + SameSite=Lax + Secure
- 匿名可达的接口都有明确上界：历史窗口最宽 7 天，agent 二进制转发最多 4 个并发
- 导入的备份先整份校验（完整性、表结构、无视图/触发器、schema 版本）才允许覆盖；恢复后所有会话作废

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
