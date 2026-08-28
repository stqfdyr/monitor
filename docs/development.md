# 开发

## 环境

Rust stable（1.98 验证过）、Node 24（CI 用的也是 24）。

```bash
rustup component add clippy rustfmt
cd web-admin && npm ci && cd ..
git clone https://github.com/stqfdyr/monitor-theme-default web-theme
cd web-theme && npm ci && cd ..
```

## 构建

```bash
# 两个前端要先构建，hub 会把它们的 dist 嵌进二进制
cd web-admin && npm run build && cd ..
cd web-theme && npm run build && cd ..
cargo build --release
```

**debug 构建时 rust-embed 从磁盘读两个 `dist/`**，所以开发期间改完前端不用重新 `cargo build`，重新运行对应目录的 `npm run build` 就行。release 才真正嵌入。

## 测试

```bash
cargo test
cargo clippy --all-targets
cargo fmt --all
```

覆盖重点：

| 位置 | 覆盖什么 |
|---|---|
| `src/db.rs` | **流量累加、重启与缩水对齐、日/月周期滚动**、级联删除、排序完整性、prune |
| `src/auth.rs` | 密码哈希、限流锁定的完整生命周期、cookie 往返、OAuth 回调往返、转发头 |
| `src/agent_ws.rs` | RPC 分发、每分钟落盘、探测结果归属、会话拆除竞态、header 鉴权 |
| `src/api.rs` | 公开视图过滤、快照缓存与两个受众、读权限、密钥不回读 |
| `src/frontend.rs` | 主题路径穿越与越界符号链接、主题短名的两道守卫 |
| `src/main.rs` | `--site` 推出的 cookie 标志与明文告警、静态路由、账单滚动、首次运行 |

写测试的原则（用户明确要求不要过度测试）：**一段非平凡逻辑留一个能跑的检查就够**，不要每个函数一个测试。优先测边界和不变量，不测 getter。

### 断言必须能被证伪

一条在逻辑被改坏之后依然通过的断言，是摆设，不是测试。加断言之前先想清楚：**什么样的代码改动会让它变红？**想不出来就别写。这个仓库踩过的几种：

- **主键替你去重。** `metric` 和 `ping_record` 都是 `INSERT OR REPLACE` + 复合主键。同一秒写进去的几条记录本来就会塌成一行，所以「五份上报只落一行」这种计数断言，不管每分钟落盘的门控还在不在都是绿的。要么让各条记录带上不同的键（不同 task_id），要么改断言别的东西（落盘时间戳是不是对齐到整分钟）。
- **容器替你去重。** 「限流表不会无限增长」曾经断言 `map.len() <= 去重后的地址数`——`HashMap` 的键天生保证这一点，把清扫逻辑整个删掉照样绿。
- **常量时间窗关不掉。** 锁定窗口写死成 15 分钟，「过期自动解锁」这条分支在测试里永远够不着。`Throttle` 的窗口因此是字段而不是常量，生产路径取 `LOCKOUT`，测试塞个几十毫秒的。
- **重建出来的东西和缓存的一模一样。** 验证快照缓存生效，不能拿两次读取比相等——不走缓存重建一遍，字节也一样。得在两次读取之间悄悄改掉底层数据。
- **`recv().await` 等的是永远不来的消息。** 断言通道已关闭要用 `try_recv()`：真出了回归，`recv().await` 会把整个测试挂死，CI 上是超时不是报错，比不测还难查。

### 变异检查

想确认一批测试到底护住了什么，就把逻辑挨个改坏，看有没有测试变红：

```bash
# 例：让同 boot 的读数缩水时不再钳零，铁律一应该立刻报警
sed -i 's/(rx - last_rx).max(0), (tx - last_tx).max(0)/rx - last_rx, tx - last_tx/' src/db.rs
cargo test        # 期望 db::tests::a_shrinking_reading... 变红
git checkout -- src/db.rs
```

批量跑的时候有两个坑：**改完源码要让 mtime 前进一秒**（同一秒内连着改，cargo 会拿旧产物糊弄你，好测试也显示成漏网），以及**别用 `git checkout` 还原**——手头没提交的改动会跟着一起没。存一份文件内容再写回去。

## 本地跑起来

```bash
# hub
cargo run -- --listen 127.0.0.1:9911 --db /tmp/dev.db --themes /tmp/themes --site http://127.0.0.1:9911
# 记下打印出来的一次性密码

# 后台热更新（API 代理到 9911）
cd web-admin && npm run dev
```

后台开发服务器使用 `/admin/`。默认主题在另一个终端运行 `cd web-theme && npm run dev`，其 Vite 也会把 `/api` 和 WebSocket 代理到 9911。

第三方主题不用加入 hub 仓库。构建后按下面的形状复制到 `--themes` 指向的目录，再到后台「主题」页切换：

```text
/tmp/themes/<short>/theme.json
/tmp/themes/<short>/dist/index.html
```

目录名必须等于 `theme.json` 的 `short`；短名只允许字母、数字、`-`、`_`。完整接口契约见默认主题仓库的 README。

登录后建一个节点，拿到 token，在 [agent 仓库](https://github.com/stqfdyr/agent) 里跑一个指向自己的 agent：

```bash
cd /path/to/agent
cargo run -- --server http://127.0.0.1:9911 --token <token> --interval 1
```

`http://` 只对回环地址放行，正好够本地调试。

## 端到端验证

改完核心逻辑后跑一遍这个：

```bash
H=http://127.0.0.1:9911

# 1. 未登录不能碰面板接口
curl -s -o /dev/null -w "%{http_code}\n" -X POST $H/api/nodes -d '{"name":"x"}'   # 期望 401

# 2. 无效 token 连不上
WSH=(-H 'Connection: Upgrade' -H 'Upgrade: websocket' \
     -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==')
curl -s -o /dev/null -w "%{http_code}\n" "${WSH[@]}" $H/api/agent/ws                        # 401
curl -s -o /dev/null -w "%{http_code}\n" "${WSH[@]}" -H 'Authorization: Bearer bad' $H/api/agent/ws  # 401
curl -s -o /dev/null -w "%{http_code}\n" "${WSH[@]}" -H "Authorization: Bearer $TOKEN" $H/api/agent/ws # 101

# 3. 公开页不含敏感字段
curl -s $H/api/nodes | python3 -c '
import sys,json; n=json.load(sys.stdin)["nodes"][0]
assert all(k not in n for k in ("ip","remark","hostname")), "泄露了敏感字段"
print("公开视图 OK")'

# 4. 数据口径对得上系统工具
cd /path/to/agent && cargo test crosscheck -- --nocapture
free -b | awk 'NR==2{printf "free used=%.2fG\n", $3/1073741824}'
df -B1 --output=size,used / | tail -1

# 5. 累计流量跨 hub 重启不回退（见 traffic.md 的完整步骤）
```

## 截图检查前端

机器上有 puppeteer 的 Chrome，可以直接驱动：

```bash
CHROME=$(find /root/.cache/puppeteer/chrome -name chrome -type f | head -1)
"$CHROME" --headless --disable-gpu --no-sandbox --hide-scrollbars \
  --window-size=1280,1400 --screenshot=out.png --virtual-time-budget=6000 \
  http://127.0.0.1:9911/
```

需要登录后的页面就用 `puppeteer-core` 写个几十行的脚本走一遍登录表单。

## 发布

推一个 `v*` tag 触发 `.github/workflows/release.yml`。CI 会构建内置后台、clone 并构建默认主题，再构建 hub 的 musl 静态二进制。agent 和主题由各自仓库独立发布。

`install.sh` 默认走 hub 的 `/agent/{arch}` 拿二进制；只有 `--github-proxy` 会绕过 hub 直连 GitHub，
拼的是 `src/main.rs` 里的 `AGENT_REPO` 常量。

## 几个容易踩的坑

- **别用 `pkill -f`** 停进程。它会匹配到正在跑这条命令的 shell 自己，把会话一起干掉。用 `ss -lptn "sport = :9911"` 找 PID 再 `kill`
- shadcn 的组件（`web-admin/src/components/ui/` 和主题的 `src/components/ui/`）是 CLI 生成的，**不要手改**。改样式改各自 `src/index.css` 的 CSS 变量
- `tsconfig` 里不要加 `baseUrl`，TypeScript 6 里已废弃会直接报错。`paths` 单独用就行
- `erasableSyntaxOnly` 开着，构造函数参数属性（`constructor(public x: number)`）不能用
- lucide-react v1 删掉了品牌图标，没有 `Github` 组件。登录页里是手写的内联 SVG
