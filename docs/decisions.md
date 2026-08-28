# 设计决策

**动手改之前读这个。** 下面每一条都是深思熟虑的结果，不是随手写的。要推翻其中任何一条，先跟用户确认。

标注说明：**[用户]** = 用户明确拍板的；**[默认]** = 我按项目原则定的，用户确认过或未反对。

---

## 通信

### WebSocket + JSON-RPC 2.0 **[用户]**

用户要求"用 komari 最推荐的方式"。komari 的 v2 协议（`/api/clients/v2/rpc`）和 NodeGet 都是 WebSocket 上跑 JSON-RPC。

**否决**：HTTP POST 轮询（Scout 的做法）——hub 无法主动下发探测任务；gRPC / protobuf（Scout 也用了）——多一个 codegen 步骤和 `protoc` 依赖，JSON 在这个数据量下够用且调试方便。

只用**通知**（无 `id`、无响应）而不是完整 RPC：没有任何一个调用需要返回值。

### token 走 `Authorization` 头，不走 URL query **[默认]**

komari 是 `?token=xxx`。query string 会进 nginx/caddy 的 access log。agent 不是浏览器，能设请求头，所以没有理由把 token 放 URL 里。

改动位置：[agent 仓库](https://github.com/stqfdyr/agent) 的 `src/main.rs` 的 `session()`，`src/agent_ws.rs` 的 `bearer()`。

---

## Agent

### 只支持 Linux **[用户]**

用户在"只 Linux"和"Linux + Windows/macOS"之间选了前者。

**这个决定的连锁效果很大**：直接读 `/proc` 和 `statvfs`，完全不引入 `sysinfo`。数据更准（sysinfo 的内存和硬盘口径就是要修的 bug）、二进制更小、零抽象层。

**要加跨平台就等于要推翻整个 `collect.rs`。** 先问用户。

### agent 完全无状态 **[默认]**

agent 不写任何文件、不记忆任何跨重启的状态。它只上报"此刻内核告诉我的数字"，累加全部由 hub 负责。

**为什么**：agent 跑在别人的 VPS 上，可能被重装、被迁移、被 kill -9。让它维护持久状态就等于给每台机器引入一个可能损坏的状态文件。hub 只有一个，备份和修复都容易。

**否决**：komari 的 `netstatic` 方案（agent 侧落盘记流量）——每台 VPS 一个状态文件，重装即丢。

### 不做自动更新 **[用户]**

komari-agent 能从 GitHub release 拉新二进制自替换。用户选了不做。

**为什么**：自更新意味着 agent 能以 root 下载并执行任意二进制，是整个探针最大的攻击面。升级方式是重跑一遍安装命令，一行的事。

---

## Hub

### 零配置文件 **[默认]**

只有四个命令行参数：`--listen`、`--db`、`--themes`、`--site`。`--themes` 默认是数据库旁的 `themes/`；其余全部存在 SQLite 的 `setting` 表里，在面板配置。

**为什么**：少一个 TOML 解析、少一类"配置文件在哪/权限对不对/和 DB 不同步"的问题。密钥存在 DB 里也比存在明文配置文件里好。

首次启动生成一次性应急密码打印到 stdout——否则全新的 hub 在 GitHub 配好之前完全进不去。

**`--site` 有两个作用**，改动时注意：安装命令里的地址，以及 session cookie 要不要带 `Secure`（`http://` 开头就不带，否则本地 HTTP 部署浏览器会拒绝存 cookie）。

### 后台与默认主题嵌进二进制 **[默认]**

`rust-embed` 把 `web-admin/dist` 和构建时检出的 `web-theme/dist` 打进 hub。没有安装第三方主题时，部署仍是一个二进制 + 一个 db 文件。

注意：**debug 构建时 rust-embed 从磁盘读**，所以开发时要保留两个 `dist/`；release 才真正嵌入。

### 单管理员，不做用户表 **[默认]**

没有 `user` 表，没有角色，没有权限系统。要么是管理员（有有效 session），要么不是。

**为什么**：自用探针。加多用户就要加权限模型、加"谁能看哪些节点"，是另一个量级的复杂度。

### 历史明细每分钟一行，不做降采样 **[默认]**

agent 每 2 秒上报（实时视图用），但 `metric` 表每节点每分钟只写一行（`src/agent_ws.rs` 里 `now / 60` 变了才写）。超过保留天数直接删，不做多级降采样。

**为什么能这么简单**：因为累计流量不是从 `metric` 算出来的，所以明细可以随便删。见 [traffic.md](traffic.md)。

---

## 流量

### hub 侧累加 + boot_id 识别重启 **[用户提的需求，方案是默认]**

用户的原话：komari 的 VPS 一重启总流量就清零，"我的探针要能续上，一直累加"。

完整机制见 [traffic.md](traffic.md)。核心：agent 报内核 lifetime 计数器 + `boot_id`，hub 检测 `boot_id` 变化或计数器回退就把当前读数整个计为增量。

### 总流量和月流量彻底分开 **[用户]**

用户明确说"这个和总流量不是一回事，要区分开"。

`traffic` 表里 `total_rx/total_tx`（永不重置）和 `month_rx/month_tx`（按商家重置日重置）是两组独立的列，同一次上报同时更新。月流量在默认主题里和 CPU、内存、硬盘共用同一个进度条组件（开发检出路径 `web-theme/src/components/Meter.tsx`），按节点的 `traffic_mode` 计量——面板的节点表用的是同一套口径。

### 月度周期按商家重置日算 **[默认]**

`db::period_start()` 处理了短月份（重置日 31 号遇到 2 月落到 28/29 号）和跨年回绕。有专门的测试覆盖这些边界。

---

## 安全

### GitHub SSO + 本地应急密码 **[用户]**

用户在"纯 GitHub SSO"和"GitHub + 应急密码"之间选了后者。

**为什么**：GitHub 挂了、OAuth App 配错了、被墙了，纯 SSO 会把用户彻底锁在自己的后台外面。komari 也是双通道。

细节见 [security.md](security.md)。

### 公开状态页默认开启 **[用户]**

用户在三个选项里选了"要，且默认开启"。可以在设置里全局关，也可以按节点关（`node.public`）。

**公开视图永远不输出 `ip`、`hostname`、`remark`**——`src/api.rs` 的 `node_view(app, node, full)` 里 `full=false` 时这三个字段根本不写进 JSON。有测试守着。

### 手写 OAuth，不用 oauth2 crate **[默认]**

GitHub OAuth 就是两个 HTTP 请求。`oauth2` crate 带一堆用不上的 flow 和类型体操。手写约 40 行，state nonce 用 HttpOnly cookie 防 CSRF。

### token 用 sha256，密码用 argon2 **[默认]**

节点 token 是 256 位随机值，高熵，sha256 足够（argon2 的慢哈希是为了对抗低熵密码的暴力破解，对随机 token 没意义，还会让每次 agent 连接都多几十毫秒）。管理员密码是人选的，必须 argon2id。

---

## 前端

### 公开主题独立仓库，后台留在 hub **[用户]**

用户在了解跨仓库构建代价后，明确要求像 komari 一样拆出主题，并能方便更换。主题只拥有公开状态页；登录页与 `/admin/*` 固定在 hub。

真正的换主题能力来自 hub 运行时读取 `<themes>/<short>/dist`，不是拆仓库本身。选中主题缺失、损坏或被删除时自动回落内置默认主题，保证零配置启动和恢复入口可用。

默认主题放在独立的 `monitor-theme-default` 仓库。hub release CI clone 并构建它，再作为内置 fallback 嵌入。这引入了一个明确接受的跨仓库构建依赖，换来独立的主题版本、契约和开发流程。

**否决**：让主题接管后台——每个主题作者都会被迫实现节点 CRUD、OAuth、密码修改；zip 上传——需要额外依赖并扩大 zip-slip 攻击面，复制目录 + 面板选择已经够用。

### shadcn/ui **[用户]**

用户指定的。`web-admin/src/components/ui/` 和主题的 `src/components/ui/` 是 CLI 生成的组件，**不要手改**——要改样式改各自 `src/index.css` 里的 CSS 变量。

用不到的组件删掉了（dropdown-menu / progress / tooltip / sonner 的 wrapper）。

### 节点接入分两步 **[用户]**

首次添加只填名称，不立刻打断用户配置 Agent。需要部署时再点节点旁的下载按钮，调整 agent 已经支持且
实际有用的参数：上报间隔（默认 1 秒），以及仅用于下载 GitHub Release 的代理。

节点的展示/流量设置留在普通编辑里，价格、付款周期、到期日放在独立的「续费设置」里。

**额外排除的网卡前缀已经删掉** **[用户]**：agent 内置的虚拟网卡前缀列表够用，多一个输入框、
多一条 install.sh 分支、多一路 agent 参数，只为一个没人填的框。hub、install.sh、agent 三边一起删。

### hub 自己转发 agent 二进制 **[用户]**

`install.sh` 默认从 `<hub>/agent/<arch>` 下载，hub 去 GitHub Release 取回来再转发。起因是一台
IPv6-only 的 Alpine 机器：github.com 没有 AAAA 记录，试过的三个加速站里只有一个能用（还要 10 秒），
但它访问 hub 只要 0.6 秒——既然节点本来就得连上 hub 才有意义，就让 hub 顺手把二进制递过去。

`--github-proxy` 保留，作为 hub 自己拉不到 release 时的退路。**否决**：hub 侧缓存二进制（安装是低频
操作，1.6 MB 直接转发就够）、把二进制打进 hub（每次发 agent 都要重发 hub）。

### install.sh 支持 OpenRC **[用户]**

agent 本来就是 musl 静态二进制，Alpine 上跑得了，缺的只是 init 脚本。systemd 那套加固
（`DynamicUser`、`ProtectSystem`）在 OpenRC 下没有对应物，用 `supervise-daemon` 拿到等价的自动重启。

### 面板可以手改流量计数 **[用户]**

`PUT /api/nodes/{id}/traffic` 一直都在，只是没有入口。节点换了机器，boot_id 变化会让 hub 把新机器的
历史计数当成一次增量记进来（见 [traffic.md](traffic.md)），编辑弹窗里的「流量校正」就是给这种情况准备的。
按 GiB 填写；没动过输入框就不发这个请求，免得四舍五入把几百 KB 抹成 0。

### 到期日自动顺延 **[用户]**

过期当天还在上报的节点，显然是续过费了，只是没人回来改日期。hub 每小时巡检一次：节点在线、
到期日已过、付款周期不是「一次性」，就按周期整月往后推，一直推到今天之后（`src/main.rs` 的
`renewed()`）。月末日期由 chrono 的 `checked_add_months` 收敛到下月最后一天，1 月 31 日续成 2 月 28 日。

只动日期，不碰钱：仍然不做自动扣款，也不给离线节点顺延——节点掉线才是真到期的信号。
**否决**：每个节点一个开关（还没人需要按节点关掉）、到期通知（用户明确砍掉了通知）。

节点 token 仍然只存哈希。生成安装命令时会换 token，并立刻使旧 agent 失效；
不为了复刻 Komari 的随时查看命令而降低这个安全边界。

"立刻失效"要靠 hub 主动断开那条连接（`api::reset_token` 从 `App.agents` 里删掉发送端）：
token 只在握手时校验一次，光换库里的哈希不会打扰已经连上的 agent。所以打开安装弹窗就会
让这个节点掉线，弹窗里也这么写。

### recharts **[用户]**

打包后 774 kB（gzip 232 kB），大头是它。用户在"保留 / 换 uPlot / 自己写 SVG"里选了保留——理由是主流选择，且用户自己的原则是"不要自己造轮子"。带 hash 的资源设了 immutable 缓存头，只加载一次。

### 不用 react-router **[默认]**

后台只有几个内部页面。`web-admin/src/App.tsx` 里的小型 `usePath()` 就够了，用 `history.pushState` + `popstate`。

---

## 用户明确砍掉的功能

不要"顺手"加回来。想加先问。

| 功能 | 用户原话 |
|---|---|
| 通知 | "我不需要通知功能" |
| 远程 SSH | "我不需要远程 ssh 功能" |
| 插件系统 | "不需要插件系统" |
| ICMP / HTTP ping | "参考 komari 的配置方式，砍掉 icmp 和 http" |

## 待办

- `release_repo` 设置项目前只能改数据库，面板没有输入框。仓库名不是 `stqfdyr/monitor` 的话需要手动 `UPDATE setting`
- `install.sh` 依赖 GitHub release 存在。需要先建仓库并推 `v0.1.0` tag，`.github/workflows/release.yml` 会构建 musl 静态二进制
