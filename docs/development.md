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
| `src/db.rs` | **流量累加与月度周期**、级联删除、prune |
| `src/auth.rs` | 密码哈希、限流、cookie、转发头 |
| `src/agent_ws.rs` | RPC 分发、每分钟落盘、header 鉴权 |
| `src/api.rs` | 公开视图过滤、读权限、密钥不回读 |
| `src/frontend.rs` | 主题路径穿越与越界符号链接 |
| `src/main.rs` | cookie 安全标志、静态路由、安装命令、首次运行 |

写测试的原则（用户明确要求不要过度测试）：**一段非平凡逻辑留一个能跑的检查就够**，不要每个函数一个测试。优先测边界和不变量，不测 getter。

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

`install.sh` 从 `https://github.com/{release_repo}/releases/latest/download/monitor-agent-{arch}-unknown-linux-musl` 下载，`release_repo` 默认 `stqfdyr/monitor`，存在 `setting` 表里。

## 几个容易踩的坑

- **别用 `pkill -f`** 停进程。它会匹配到正在跑这条命令的 shell 自己，把会话一起干掉。用 `ss -lptn "sport = :9911"` 找 PID 再 `kill`
- shadcn 的组件（`web-admin/src/components/ui/` 和主题的 `src/components/ui/`）是 CLI 生成的，**不要手改**。改样式改各自 `src/index.css` 的 CSS 变量
- `tsconfig` 里不要加 `baseUrl`，TypeScript 6 里已废弃会直接报错。`paths` 单独用就行
- `erasableSyntaxOnly` 开着，构造函数参数属性（`constructor(public x: number)`）不能用
- lucide-react v1 删掉了品牌图标，没有 `Github` 组件。登录页里是手写的内联 SVG
