# 安全

「安全」是这个项目三条设计哲学的第一条。碰鉴权、API 边界、公开页之前读这篇。

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

`src/api.rs` 定义了一个 `Admin` 类型，实现 `FromRequestParts`。handler 签名里带 `_: Admin`，
axum 就会在进入函数体之前校验 session，不通过直接 401。

```rust
pub async fn delete_node(_: Admin, State(app): State<Shared>, ...) -> Response
```

**鉴权在类型签名里，忘不掉。** 新增面板接口照抄这个模式，不要在函数体里手写检查。

### session

- 256 位随机 token，**数据库里只存 sha256**
- cookie：`HttpOnly`、`SameSite=Lax`、`Path=/`；`--site` 不是 `http://` 时加 `Secure`，没有 `--site` 时看请求的 `X-Forwarded-Proto`
- 14 天过期，每小时清一次过期记录
- 改密码时 `drop_all_sessions()`，所有登录立即失效

`SameSite=Lax` + 同源 API 就是 CSRF 防护，没有额外的 CSRF token。

## 两条登录通道

GitHub SSO 为主，本地密码为辅。**不要删掉密码通道**——GitHub 挂了、被墙了、OAuth App 配错了的
时候，那是唯一的入口。

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

改这段时注意方向别反了：反了就是任何 GitHub 账号都能登进后台。只申请 `read:user` scope。

### 本地密码

- argon2id 哈希，每次生成独立 salt
- 最短 12 位（`api::save_settings` 里校验）
- 首次启动生成一个 24 字符的随机密码打印到 stdout，只打印这一次
- 哈希解析失败或为空时**验证失败**，不是通过（fail closed，有测试）

### 登录限流

`auth::Throttle`：同一来源地址 15 分钟内 5 次失败就锁死。成功登录清零。

**外加一道并发闸门**（`auth::PASSWORD_CHECKS`，值为 1）：同一时刻只允许一次密码校验，挤不进来的
直接 429，不排队。锁定管的是「一个地址试几次」，管不住「几个地址一起试」——而 IPv6 下攻击者手里
的地址是一个 /64。argon2 一次要 19 MiB 和大约十分之一秒的一个核，这个开销是故意的，没有上界它
就从防御变成杠杆。

**闸门的值必须小于机器真能同时跑的数量，否则等于没有。** 最早写的是 4，读起来宽松、量出来是零：
argon2 吃满一个核，三核的 hub 凑不出 4 个在飞，洪水穿过 4 的闸门和没有闸门一样。实测（64 线程
× 10 轮）：

| | 洪水后 RSS | 结果 |
|---|---:|---|
| 无闸门 | **570 MB** | 160 次全跑完 argon2 |
| 闸门 = 4 | **570 MB** | 与无闸门相同 |
| 闸门 = 1 | **104–143 MB** | 640 次里 633 次被 429 挡下 |

unit 文件给的是 `MemoryMax=256M`：无闸门的 570 MB 是它的 2.2 倍，也就是被 OOM 掉再重启，再打
再挂。取 1 而不是跟着核数走，因为跟着核数走只会在更小的机器上把洞重新打开。

来源地址取 `X-Forwarded-For` 的第一跳，没有就用 peer 地址。**这个值只用于限流和面板上显示的
节点地址，绝不用于鉴权**——它是客户端可伪造的。

**而且只在 peer 本身是本地地址（回环、私有网段、IPv6 ULA/link-local）时才采信**，也就是确实有
反代在前面的情况。hub 直接暴露在公网时这个头是攻击者自己写的：每个请求换一个伪造地址，既能绕开
锁定，又能让 `Throttle` 的 map 无限长大。`auth::client_ip` 守着这条，`record_failure` 顺手清掉过
窗口的条目。

## 节点 token

- 256 位随机值，**数据库里存明文**（`node.token`），因为面板要能随时把安装命令显示出来。
  这是用户拍板的取舍，理由和被推翻的旧设计都记在 [decisions.md](decisions.md)
- **只在 `full=true` 的节点视图里输出**，和 ip / hostname / remark 同一个分支。公开页拿到的
  JSON 里没有这个 key。有测试守着（`the_public_view_hides_private_nodes_and_sensitive_fields`
  里断言整个公开 payload 的字符串不含任何节点的 token）
- 可以重新生成（`POST /api/nodes/{id}/token`），旧的立刻失效——包括**已经连上的那条连接**：
  token 只在 WebSocket 握手时校验，所以换发时 hub 会把这个节点从 `App.agents` 里删掉，
  agent 的循环随即结束，它拿旧 token 重连会吃到 401。少了这一步，泄露的 token 打开的会话能一直报下去
- 走 `Authorization: Bearer` 头，不走 URL query——query 会进反代的 access log
- 无效/缺失一律返回 401，不区分「格式不对」和「不存在」

**明文存储意味着数据库本身就是凭证。** 拿到 `monitor.db` 的人可以冒充任何节点上报假数据。但同一
个文件里已经有 session、GitHub client secret 和管理员密码哈希，它本来就必须当作机密对待，备份要
加密。管理员密码和 session 不受影响，仍然分别是 argon2id 和 sha256。

文件权限由 `Db::open` 收到 `0600`，`-wal` 和 `-shm` 一起——那两个文件装着同样的行。SQLite 建库时
只认 umask，默认的 022 给出的是**所有人可读**，只靠目录权限挡是把凭证押在一层上。best effort：
没有 Unix 权限位的文件系统上照常启动，在那里拒绝启动比暴露更糟。

### agent 侧

安装脚本把 token 写进 `/opt/monitor/agent.env`，`0600` + `umask 077`——`/opt/monitor` 本身是
`0755`（二进制在里面），私密性全靠文件位。**不写在 systemd unit 里**——unit 内容会出现在 `systemctl cat` 和 journal 里。

systemd 单元加固：`DynamicUser`、`NoNewPrivileges`、`ProtectSystem=strict`、`ProtectHome`、`PrivateTmp`、`PrivateDevices`、`RestrictAddressFamilies=AF_INET AF_INET6 AF_NETLINK`、`MemoryMax=64M`。

这一整行只对 systemd 成立。**OpenRC 上 agent 以 root 跑**，理由和这笔债的还法见
[architecture.md](architecture.md)。token 的存放两边一样（root-only 的 env 文件，不进 init 脚本，不进日志）。

`AF_NETLINK` 是 `getifaddrs(3)` 问内核「这台机器有哪些地址」的通道，agent 靠它上报自己的
IPv4/IPv6。去掉它不会报错，只会让上报的地址一直是空字符串。

### agent 拒绝明文传输

`ws_url()` 在目标不是回环地址时拒绝 `ws://`，裸主机名默认升级到 `wss://`。token 不能明文过公网。

`install.sh` 对 `--server` 执行同一条规则，**包括裸主机名升级到 `https://`**。它拿这个地址下载的是
**接下来要以 root 运行的二进制**，明文 HTTP 上任何一跳都能换掉它——比 token 泄露更糟。
两处规则必须一起改，只改一边就是留了条明文的路。

裸主机名那一半曾经只有 agent 实现：脚本只拦显式的 `http://`，把无 scheme 的值原样交给 curl，而
**curl 对没有 scheme 的 URL 默认走 `http://`**。于是同一条安装命令里 token 走 TLS，那个二进制走
明文——规则里更糟的那半反而没护住。

### `--insecure` 是这道闸唯一的开关

裸 ip:port 部署的 hub 只有明文 HTTP，所以两边都认一个 `--insecure`。它不是「忽略警告」，而是三件
事一起改，缺一件都会留下坑：

1. 放行明文到远程 hub
2. **裸主机名不再升级成 TLS**——否则这个 flag 会去 dial 一个永远握不上手的 `wss://`
3. `install.sh` 往 stderr 打风险说明，并把 `--insecure` 写进 agent 的 `ExecStart`

显式的 `https://` 地址加了 `--insecure` 也仍然走 TLS：这个 flag 允许明文，不强制明文。

上述开关仅保留给手工运行脚本和 agent。面板只从 HTTPS 域名生成安装命令，不自动添加 `--insecure`。
IP（包括回环）或明文入口不允许添加节点、开启批量窗口和注册新节点。前端检查当前 origin，hub 的三个
创建入口共用 `provisioning_allowed()`：检查 Host、HTTPS 标志及请求携带的 Origin；`--site` 不能将 IP
入口变成域名入口。hub 应保持仅回环监听，受信反代透传 Host 并设置 X-Forwarded-Proto，不能让公网绕过反代。

## 数据库的导出与导入

面板的「数据」页有三个按钮，都在 `Admin` 后面，但它们碰的是整个库，所以边界值得单独写清楚。

### 导出的文件就是全部凭证

`VACUUM INTO` 写出来的副本里有节点 token 的明文、GitHub client secret、管理员密码哈希和所有
session 行。**它和 `monitor.db` 是同一个密级**，下载下来要按密钥保管，别丢进网盘或者仓库。

副本先写在库文件旁边（同一个文件系统，同一份目录权限），`db::own_only` 立刻把它收到 `0600`——
SQLite 建文件只认 umask，默认的 022 是所有人可读。然后**打开即 unlink**：文件只在这次响应活着的
时候存在，客户端半路断开也不会留下一份完整的库躺在磁盘上。

导出是 `GET` 而不是 `POST`，为的是浏览器原生的 `<a download>`——响应流式落盘，不经过页面内存。
代价说清楚：`SameSite=Lax` 允许顶层导航带上 cookie，所以一个恶意页面可以让管理员的浏览器**下载**
一份备份到管理员自己的磁盘上。它读不到内容（跨域响应对页面不可见），拿到的只是一次 hub 侧的整库
复制，所以留着这条路；真要收紧就得改成 `POST` + JS 取 blob，那等于把整个库读进浏览器内存。

### 导入的文件是不可信输入

恢复是这个面板上唯一能一次删掉所有数据的操作，而文件来自谁的磁盘不知道。`db::check_backup` 在
**任何一页被复制之前**逐条检查，任何一条不过就 400，当前库一个字节都不动：

- `PRAGMA integrity_check` 必须是 `ok`——不是 SQLite、或者是坏掉的 SQLite，到这里就停
- `sqlite_master` 里**不能有 view 或 trigger**。恢复是逐页复制，文件带来的 schema 就是 hub 接下来
  执行每条语句时面对的 schema：本该是表的地方放一个视图或触发器，等于让上传者的代码跑在 hub 的
  写路径上
- 八张表一张都不能少（`db::TABLES`），否则它不是这个 hub 的备份
- `user_version` 不能高于本版本的 `SCHEMA_VERSION`（来自更新的 hub，降级读不了）；低于就在恢复
  之后跑和重启时一样的那几条迁移
- page size 必须和当前库一致——WAL 模式下在线备份 API 不接受页大小变化，这条是把
  `SQLITE_READONLY` 换成一句人话

上传分片写进库文件旁的临时文件（`0600`），整份最大 `api::MAX_RESTORE`（256 MiB），无论成败都删
——连同 SQLite 可能在它旁边建的 `-wal`/`-shm`。校验和覆盖都在 `spawn_blocking` 里：`integrity_check`
要读完整个文件，256 MiB 的上传不该占着 runtime 线程。

**这和主题上传是全站仅有的两条把调用方的字节写进磁盘的路径**，所以三层上界各自独立成立，不靠上一层
兜底：单请求由 `RequestBodyLimitLayer(MAX_CHUNK)` 卡在 8 MiB；整份由 `total` 在**第一个请求**就卡
（超限连第一个字节都不会发出来）；handler 里再自己数一遍收到的字节，写超 `total` 当场中止并把文件
截回片首。分片本身把单次请求的落盘上界从 256 MiB 降到了 8 MiB。

**反代的上限在 hub 的上限之前生效**：nginx 默认 `client_max_body_size 1m` 连一片都放不过去，要改成
`8m`。这个数只跟分片大小有关，不随数据库增长（Cloudflare 免费版那 100 MB 因此也不再是天花板）。
被反代拒掉时 413 来自反代，hub 这边一行日志都没有，所以面板收到 413 会直接把 `client_max_body_size`
念出来——见 README 的反向代理一节。

### 上传的主题是在访客浏览器里运行的第三方代码

主题上传（`POST /api/themes`）要 `Admin`，但装进去的 JS/CSS 会在**公开状态页**上对每个访客执行，
和 hub 同源。这不是上传引入的新信任边界——把目录复制进 `<themes>/` 一直是同样的效果——但入口从
「能 ssh 到机器」降到了「有面板密码」，所以面板上那句「只装你信得过的来源」是这条路的全部防线。

装之前对压缩包本身的检查（逐条目类型、路径、条目数、单文件与解压总量，见
[decisions.md](decisions.md#主题可以从面板上传格式是-themetargz-用户)）挡的是**写到目录外**和
**撑爆磁盘**，不体检里面的 JS——那件事没法自动做。

三个上界：整包 `api::MAX_THEME`（32 MiB）、解压总量 64 MiB、单文件 8 MiB。解压总量必须和上传上限
分开，gz 的压缩比没有上界。

预览图（`GET /api/themes/{short}/preview`）同样要 `Admin`，读的是写死的 `preview.png`——**文件名是
常量而不是 manifest 里的字段，所以没有第二处路径要防**。大小在这里重新拦一次（8 MiB），因为直接
复制进 `<themes>/` 的主题没经过解压那道闸。

### 恢复之后

- **所有 session 作废，调用方当场换发一张新的**。备份里带着它被制作那一刻的 session 行，恢复不是
  让那些早就登出的会话复活的理由；而换发让点按钮的人不会被自己的操作踢出去
- **所有 agent 连接断开**（`App.agents` 清空）。token 是握手时校验一次的，恢复之后它们手里的 token
  可能属于别的节点或者不再存在，让它们重连去对新的库
- 快照缓存失效，否则面板还在显示上一份库的节点

### VACUUM

「回收空间」= 按保留天数 `prune` + `VACUUM` + `wal_checkpoint(TRUNCATE)`。SQLite 对 `VACUUM` 的
要求都成立：不在事务里、连接上没有活着的语句（只有一条连接，且这次调用独占它）、需要与库等量的
空闲磁盘（不够就失败回滚，原库不受影响）、可能重排 rowid（`metric` 和 `ping_record` 是
WITHOUT ROWID，其余表都有自己声明的主键，没人依赖 rowid）。WAL 模式下重建先落在 WAL 里，
**不 checkpoint 的话文件只会变大不会变小**。

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

`full=false` 时这三个字段**根本不会写进 JSON**：不是设成空字符串，不是靠前端不显示——匿名请求
拿到的 payload 里就没有这些 key。

`country` 是**故意放在公共部分**的那一个。它从 `node.ip` 查出来，但两者不是一回事：地址指向一台
机器，国家只说这台机器在哪个市场，而「这批节点分布在哪几个地区」正是状态页要回答的问题。判断标准
是下面那条规则本身——公开出去无害才放公共部分——不是「它是不是从敏感字段推出来的」。地址本身仍然
只在 `full` 分支里。

`metrics` 这一整块走的是**白名单**（`api::PUBLIC_METRICS`），不是「删掉几个已知敏感字段」：

```rust
if !full {
    m.retain(|k, _| PUBLIC_METRICS.contains(&k.as_str()));
}
```

`boot_id`、`net_rx_total`、`net_tx_total` 因此只留给面板：前者是机器标识，后两个是网卡的**整机
历史**计数（面板上那个「总流量」是 hub 自己累加的，两者不是一回事）。

**为什么必须是白名单**：`metrics` 是 agent 上报的 params **原样存下来**的（`agent_ws::report`），
而 agent 在另一个仓库、另一条发版节奏上。黑名单意味着那边加一个字段，这边就漏一个字段，而且是
在匿名页面上漏。更直接的一条：拿到任意一个节点 token 的人（一台被拿下的 VPS，或者安装命令粘错了
地方）可以自己往 params 里塞 `hostname`、`ip`、`remark`，黑名单会把它们一路送到公开页——正好是
第三条铁律禁止的三个字段。白名单下这些键根本进不了 JSON。

改 agent 上报字段时：新字段默认**不**公开，确认无害再加进 `PUBLIC_METRICS`。

字段名白名单不替代类型检查：`report()` 拒收非对象、畸形 load 和类型错误的数值字段，保留上一份有效
指标；缺字段仍兼容旧 agent。默认主题在渲染入口再检查完整指标，缺失或畸形时显示不可用，不让一个
节点的坏数据使整个页面白屏。内核流量计数器仍按「缺失读数不移动基线」单独处理。

测试：`the_public_view_hides_private_nodes_and_sensitive_fields`。单节点的历史查询走 `readable()`，
同样检查登录状态 + 节点公开标志 + 全局开关，测试
`per_node_reads_follow_the_public_flag_and_the_public_page_switch`。

### `/agent/{arch}`

公开路由，把 GitHub Release 的 agent 二进制转发给装不了的节点。`arch` 只认 `x86_64` 和 `aarch64`
两个字面量，其余一律 404。转发的是公开的 release 文件，不需要鉴权，但**并发数有闸门**，见下面
「其它」。

**下载地址里有一段来自数据库**：面板设置的 `github_proxy` 会拼在最前面（`main::release_url`）。
仓库名仍是编译进来的常量，`arch` 仍是白名单，唯一能变的就是这个前缀，而且只有管理员写得了它。
说清楚代价：**管理员可以把这条匿名路径指向任意地址，响应会原样转发给匿名调用方。** 这是设置它的
人自己选的出站目标，和 `github_client_secret` 一样落在管理员的信任范围内；保存时只校验 scheme 是
`http://` 或 `https://`。单次请求的上界没变：4 并发、120 秒超时、流式转发。

### `POST /api/agent/register`

公开路由，用一把**只在窗口内有效**的 key 换一个节点 token，让一批机器共用一条安装命令。三道闸
依次是：窗口时间（`register_until`，开一次一小时）、key（`register_key`，256 位随机，只有
`POST /api/register-window` 生成得了）、窗口内注册数上限（`api::REGISTER_LIMIT`，100 个）。

- **窗口没开和 key 不对返回同一句话**，不替探测的人区分这两件事
- **只有 key 不对才计数**（`App.registrations`，per-IP）。窗口关着时不计——那时没有可猜的秘密，
  而计数会让任何人靠伪造 `X-Forwarded-For` 把别人的地址锁出登录页
- **这个计数器和登录页的 `App.throttle` 是两个**。共用一个的话，一条批量安装脚本拿着过期的 key
  跑五台机器，就把操作者自己的出口地址锁出面板 15 分钟，而注册成功也不会清掉计数（登录成功会）。
  两条路的威胁模型不一样：一边是猜密码，一边是装机手滑。注册成功时清掉本地址的计数
- 单次请求的上界：两次 setting 读、一次 `COUNT`、一次 `INSERT`，不出网，请求体由路由的 64 KiB
  限制封顶。名字取自请求体，`trim` 后去掉控制字符、截到 64 个**字符**（不是字节）
- 两个 setting 在 `/api/settings` 里**只读回显**，`save_settings` 仍然拒绝这两个 key 名——窗口
  只能由它自己那条路由开关，key 只可能是 hub 生成的

**key 泄露的爆炸半径**：在窗口剩下的时间里最多建 100 个节点，每个都拿到自己的 token，而节点
token 只能上报它自己那个节点。读不到任何已有节点的数据，改不了设置，碰不到面板。代价是面板里
多出一批垃圾节点——关掉窗口，删掉它们，已经装好的机器不受影响。

### 加新字段时的规矩

往 `node_view()` 里加字段，**默认放在 `full` 分支里**。只有确认这个字段公开出去无害，才放到公共部分。

### 排查 GitHub 登录不通

回调路径是 **`/api/auth/github/callback`**，OAuth App 里必须精确填这个。别的探针用的是别的路径
（`/api/oauth_callback` 之类），抄错的表现极具迷惑性：GitHub 授权成功，浏览器回到站点看起来一切
正常，但没登上，**且服务端没有任何日志**，因为那个路径压根没进任何 handler。未匹配的 `/api/`
现在返回 404 而不是 SPA，所以这个错误会立刻现形（`frontend.rs` 的 `is_api_path()`）。

其余失败都会记进 journal：

```bash
journalctl -u monitor-hub -f | grep sign-in
```

- `no allowed GitHub users configured` —— 白名单为空。**空 = 拒绝所有人**，不是放行所有人
- `GitHub user X is not on the allowed list` —— 用户名不在白名单里。**完整白名单只进日志，
  不进回给浏览器的原因**：这个原因会写进 `/admin?login_error=` 跳回登录页，而任何一个 GitHub
  账号都能走完授权拿到它——带上白名单就等于把「值得钓鱼的那几个名字」发给所有人
- `GitHub returned access_denied: ...` —— 在 GitHub 页面上点了拒绝
- `state mismatch or missing` —— 不是从登录页发起的，或 state cookie 过期（10 分钟）
- `incorrect_client_credentials` —— client secret 不对

同样的原因也会红字显示在登录页上（回调失败会带 `?login_error=` 跳回来）。

## 密钥不回读

`GET /api/settings` 有一个白名单 `READABLE_SETTINGS`，`github_client_secret` 不在里面。面板可以**设置**它，但读不回来，只能拿到一个 `github_secret_set: true/false`。

测试：`settings_never_hand_back_the_github_secret`（断言里直接检查响应字符串不含密钥值）。

## 其它

- 请求体上限 64 KiB（`RequestBodyLimitLayer`）。一次上报几百字节，超过就不是正常上报
- 密码校验同时最多一次（`auth::PASSWORD_CHECKS`），见上面「登录限流」
- **WebSocket 帧上限同样是 64 KiB**（`api::MAX_FRAME`，两个 socket 都加）。上面那个是 tower layer，
  握手之后就不管事了，默认上限是 64 MiB：一个节点自己的 token 就能买下整个额度，而它发上来的东西
  会落库、并原样推给公开页的每一个访客。这两个数字要一起改
- 所有 SQL 走 rusqlite 的参数绑定，没有字符串拼接
- 探测目标必须是 `host:port` 格式，间隔 clamp 到 5–3600 秒
- 历史查询窗口登录后 clamp 到 1–2160 小时、匿名 1–168 小时（降采样限响应行数，窗口上限限扫描行数），且**样本多于像素时才聚合**（`api::sample_step`），每条曲线上限
  `SAMPLES = 1440` 点。调用方的 `points` 只能把这个预算往下调，往上调不了——**天花板是 hub 的**，
  因为这条路不要凭证。不设上限的话一个月窗口是四万多行 metric 加几倍于此的探测结果，几 MB 的 JSON
  先在内存里建出来再发走，几十个并发请求就能顶到 unit 的 `MemoryMax`。窄窗口本来就在预算内，不受影响
- `/agent/{arch}` 边收边转，不把整个 release 收进内存再发；并发转发数 clamp 到 `RELAY_SLOTS = 4`
  （`main::RELAY_GATE`），超出直接 503。它同样匿名可达，一次请求要 hub 去 GitHub 拉一趟、转出 1.8 MB，
  是这个进程上单次最贵的匿名操作。**permit 挂在响应体上而不是 handler 上**：响应头几微秒就建好了，
  真正的开销在后面那 1.8 MB
- agent 发来的垃圾消息只记日志，不断连接
- 外部主题是本地静态文件。hub 对主题根和请求文件都执行 `canonicalize()`，并确认真实路径仍在主题目录内；`..` 和指向目录外的符号链接都会被拒绝
- 主题列表和切换都必须经过 `Admin`；主题不能覆盖 `/admin/*`，所以登录与恢复入口始终使用内置前端

## 改动时的自查清单

- [ ] 新增的面板接口签名里有 `_: Admin` 吗
- [ ] 新增的节点字段是敏感信息吗？是就只放 `full` 分支
- [ ] 新增的设置项是密钥吗？是就别加进 `READABLE_SETTINGS`
- [ ] 有没有把用户可控的输入当成信任来源（尤其是 `X-Forwarded-For`）
- [ ] 白名单/黑名单的空集合语义对吗（空 = 拒绝，不是放行）
- [ ] 新增的路径会把调用方的字节写进磁盘吗？有没有大小上界、创建时是不是 `0600`、失败会不会留垃圾
- [ ] 新增的匿名可达路径，单个请求最多让 hub 分配多少内存？由请求参数决定的就要有上界。
      **内存不是唯一的额度**——占锁时长和出网字节同样算。公开页的每一条路都要能说出自己的上界：
      `/api/nodes` 靠共享快照，`/api/nodes/{id}/metrics` 靠降采样（响应行数）加 `PUBLIC_HOURS`（扫描行数），
      `/agent/{arch}` 靠流式转发（内存）加 `RELAY_GATE`（并发）
