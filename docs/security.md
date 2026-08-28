# 安全

用户把"安全"列为三条设计哲学的第一条。碰鉴权、API 边界、公开页之前读这篇。

## 三个信任边界

```
┌── 公网匿名 ──────────────────────────────────────┐
│  公开状态页：只看得到 public=1 的节点            │
│  且永远看不到 ip / hostname / remark             │
├── agent（持有节点 token）────────────────────────┤
│  只能上报自己那个节点的数据                      │
│  只能收到分配给自己的探测任务                    │
├── 管理员（持有 session cookie）──────────────────┤
│  全部                                            │
└──────────────────────────────────────────────────┘
```

## 管理员鉴权

### `Admin` 提取器

`src/api.rs` 里定义了一个 `Admin` 类型，实现了 `FromRequestParts`。任何写接口的 handler 签名里带 `_: Admin`，axum 就会在进入函数体之前先校验 session，不通过直接 401。

```rust
pub async fn delete_node(_: Admin, State(app): State<Shared>, ...) -> Response
```

**这是故意的设计：鉴权在类型签名里，忘不掉。** 新增面板接口时照抄这个模式，不要在函数体里手写检查。

### session

- 256 位随机 token，**数据库里只存 sha256**
- cookie：`HttpOnly`、`SameSite=Lax`、`Path=/`，`--site` 不是 `http://` 时加 `Secure`
- 14 天过期，每小时清一次过期记录
- 改密码时 `drop_all_sessions()`，所有登录立即失效

`SameSite=Lax` + 同源 API 就是 CSRF 防护，没有额外的 CSRF token。

## 两条登录通道

用户选的方案：GitHub SSO 为主，本地密码为辅。**不要删掉密码通道**——GitHub 挂了/被墙了/OAuth App 配错了的时候，那是唯一的入口。

### GitHub OAuth

`src/auth.rs` 里手写的，约 40 行：

1. `/api/auth/github` 生成随机 state，塞进一个 10 分钟的 HttpOnly cookie，重定向到 GitHub
2. `/api/auth/github/callback` **先校验 state 匹配**（不匹配直接 400），再拿 code 换 token，再拉用户信息
3. 用户名对照 `github_allowed_users` 白名单

**白名单为空时拒绝所有人**，不是放行所有人：

```rust
if allowed.is_empty() {
    bail!("no allowed GitHub users configured");
}
```

改这段的时候注意方向别反了——反了就是任何 GitHub 账号都能登进你的后台。

只申请 `read:user` scope。

### 本地密码

- argon2id 哈希，每次生成独立 salt
- 最短 12 位（`api::save_settings` 里校验）
- 首次启动生成一个 24 字符的随机密码打印到 stdout，只打印这一次
- 哈希解析失败或为空时**验证失败**，不是通过（fail closed，有测试）

### 登录限流

`auth::Throttle`：同一来源地址 15 分钟内 5 次失败就锁死。成功登录清零。

来源地址取 `X-Forwarded-For` 的第一跳，没有就用 peer 地址。**这个值只用于限流和面板上显示的
节点地址，绝不用于鉴权**——它是客户端可伪造的。

**而且只在 peer 本身是本地地址（回环、私有网段、IPv6 ULA/link-local）时才采信**，也就是确实
有反代在前面的情况。hub 直接暴露在公网时这个头就是攻击者自己写的：每个请求换一个伪造地址，
既能绕开锁定，又能让 `Throttle` 的 map 无限长大。`auth::client_ip` 守着这条，
`record_failure` 顺手清掉过窗口的条目，map 的大小由此有界。

## 节点 token

- 256 位随机值，**数据库里存明文**（`node.token`），因为面板要能随时把安装命令显示出来。
  这是用户拍板的取舍，理由和被推翻的旧设计都记在 [decisions.md](decisions.md)
- **只在 `full=true` 的节点视图里输出**，和 ip / hostname / remark 同一个分支。公开页拿到的
  JSON 里没有这个 key。有测试守着（`the_public_view_hides_private_nodes_and_sensitive_fields`
  里断言整个公开 payload 的字符串不含任何节点的 token）
- 可以重新生成（`POST /api/nodes/{id}/token`），旧的立刻失效——包括**已经连上的那条连接**：
  token 只在 WebSocket 握手时校验，所以换发时 hub 会从 `App.agents` 和 `live` 里删掉这个节点，
  agent 的循环随即结束，它拿旧 token 重连会吃到 401。少了这一步，泄露的 token 打开的会话能一直报下去
- 走 `Authorization: Bearer` 头，不走 URL query——query 会进反代的 access log
- 无效/缺失一律返回 401，不区分"格式不对"和"不存在"

**明文存储意味着数据库本身就是凭证。** 拿到 `monitor.db` 的人可以冒充任何节点上报假数据。但同一个
文件里已经有 session、GitHub client secret 和管理员密码哈希，它本来就必须当作机密对待——备份要加密，
文件权限要收紧。管理员密码和 session 不受影响，仍然分别是 argon2id 和 sha256。

### agent 侧

安装脚本把 token 写进 `/etc/monitor/agent.env`，`0600` + `umask 077`，目录 `0700`。**不写在 systemd unit 里**——unit 内容会出现在 `systemctl cat` 和 journal 里。

systemd 单元加固：`DynamicUser`、`NoNewPrivileges`、`ProtectSystem=strict`、`ProtectHome`、`PrivateTmp`、`PrivateDevices`、`RestrictAddressFamilies=AF_INET AF_INET6 AF_NETLINK`、`MemoryMax=64M`。

`AF_NETLINK` 是 `getifaddrs(3)` 问内核"这台机器有哪些地址"的通道，agent 靠它上报自己的
IPv4/IPv6。去掉它不会报错，只会让上报的地址一直是空字符串——这个坑踩过一次。

### agent 拒绝明文传输

`ws_url()` 在目标不是回环地址时拒绝 `ws://`，裸主机名默认升级到 `wss://`。token 不能明文过公网。

## 公开状态页

两层开关：全局 `public_page` 设置，以及每个节点的 `node.public`。

**过滤在序列化的时候做，不是在前端做**：

```rust
// src/api.rs, node_view()
if full {
    view["hostname"] = json!(node.hostname);
    view["ip"] = json!(node.ip);
    view["remark"] = json!(node.remark);
}
```

`full=false` 时这三个字段**根本不会写进 JSON**。不是设成空字符串，不是靠前端不显示——匿名请求拿到的 payload 里就没有这些 key。

agent 上报里的 `boot_id`、`net_rx_total`、`net_tx_total` 同样只留给面板：前两个是机器标识，后两个是
网卡的**整机历史**计数（面板上那个"总流量"是 hub 自己累加的，两者不是一回事）。公开页看到的是 hub 的
累计值，不是这台机器一辈子跑了多少。

守着这条的测试：`the_public_view_hides_private_nodes_and_sensitive_fields`。

单节点的历史查询走 `readable()`，同样检查登录状态 + 节点公开标志 + 全局开关。测试：`per_node_reads_follow_the_public_flag_and_the_public_page_switch`。

### `/agent/{arch}`

公开路由，把 GitHub Release 的 agent 二进制转发给装不了的节点。`arch` 只认 `x86_64` 和 `aarch64`
两个字面量，其余一律 404——URL 是拿 `release_repo` 设置拼出来的，没有任何一段来自请求，不存在
拿它当跳板打内网的可能。转发的东西本来就是公开的 release 文件，不需要鉴权。

### 加新字段时的规矩

往 `node_view()` 里加字段，**默认放在 `full` 分支里**。只有确认这个字段公开出去无害，才放到公共部分。

### 排查 GitHub 登录不通

回调路径是 **`/api/auth/github/callback`**。OAuth App 里必须精确填这个。

komari 用的是 `/api/oauth_callback`——从 komari 迁过来的人很容易照抄那个路径。抄错的表现极具迷惑性：GitHub 授权成功，浏览器回到站点看起来一切正常，但没登上，**且服务端没有任何日志**，因为那个路径压根没进任何 handler。

（现在未匹配的 `/api/` 会返回 404 而不是 SPA，所以这个错误会立刻现形。见 `src/main.rs` 的 `is_api_path()`。）

其余失败都会记进 journal：

```bash
journalctl -u monitor-hub -f | grep sign-in
```

- `no allowed GitHub users configured` —— 白名单为空。**空 = 拒绝所有人**，不是放行所有人
- `GitHub user X is not on the allowed list` —— 用户名不在白名单里
- `GitHub returned access_denied: ...` —— 在 GitHub 页面上点了拒绝
- `state mismatch or missing` —— 不是从登录页发起的，或 state cookie 过期（10 分钟）
- `incorrect_client_credentials` —— client secret 不对

同样的原因也会红字显示在登录页上（回调失败会带 `?login_error=` 跳回来）。

## 密钥不回读

`GET /api/settings` 有一个白名单 `READABLE_SETTINGS`，`github_client_secret` 不在里面。面板可以**设置**它，但读不回来，只能拿到一个 `github_secret_set: true/false`。

测试：`settings_never_hand_back_the_github_secret`（断言里直接检查响应字符串不含密钥值）。

## 其它

- 请求体上限 64 KiB（`RequestBodyLimitLayer`）。一次上报几百字节，超过就不是正常上报
- 所有 SQL 走 rusqlite 的参数绑定，没有字符串拼接
- 探测目标必须是 `host:port` 格式，间隔 clamp 到 5–3600 秒
- 历史查询窗口 clamp 到 1–2160 小时
- agent 发来的垃圾消息只记日志，不断连接
- 外部主题是本地静态文件。hub 对主题根和请求文件都执行 `canonicalize()`，并确认真实路径仍在主题目录内；`..` 和指向目录外的符号链接都会被拒绝
- 主题列表和切换都必须经过 `Admin`；主题不能覆盖 `/admin/*`，所以登录与恢复入口始终使用内置前端

## 改动时的自查清单

- [ ] 新增的面板接口签名里有 `_: Admin` 吗
- [ ] 新增的节点字段是敏感信息吗？是就只放 `full` 分支
- [ ] 新增的设置项是密钥吗？是就别加进 `READABLE_SETTINGS`
- [ ] 有没有把用户可控的输入当成信任来源（尤其是 `X-Forwarded-For`）
- [ ] 白名单/黑名单的空集合语义对吗（空 = 拒绝，不是放行）
