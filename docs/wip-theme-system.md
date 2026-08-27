# 进行中：主题系统与前端拆分

> **这是一份未完成工作的交接文档。做完后请删除本文件**，并把其中该保留的结论合并进
> [architecture.md](architecture.md) 和 [decisions.md](decisions.md)。
>
> 中断时间：2026-08-28。中断原因：单轮对话时长不够，交由后续会话继续。

## 目标

用户要求：像 komari 一样把前端拆成独立仓库，**并且方便更换不同主题**。

注意这是两件事，第二件才是重点。光拆仓库不会带来换主题的能力——真正需要的是
**hub 在运行时从磁盘加载主题**。拆仓库只是它的副产品。

用户此前问过"有必要给前端主题也创建一个仓库吗"，当时的回答是"不必要"，理由记在
[decisions.md](decisions.md) 的前端章节。**用户在了解代价后明确改变了决定**，
所以那一节需要改写，不要当成矛盾去"纠正"回来。

## 已确定的设计（不要推翻，这是照 komari 验证过的划分）

### 主题只管公开页，后台是内置的

```
主题（可换、独立仓库）   /            状态面板
                        节点详情      （当前是弹窗，不是独立路由）
内置（编译进 hub）       /admin/*      后台管理
                        登录页
```

依据：komari 的主题包里只有 `Home.tsx` / `Instance.tsx` / `NotFound.tsx` /
`ThemeManage.tsx`，没有任何节点增删改或设置页面。如果后台也归主题管，每个主题作者
都得重新实现节点 CRUD、OAuth 配置、密码修改——不现实。

### 资源路径隔离

两个 SPA 都会产出 `assets/index-<hash>.js`，直接放一起会撞。解决办法是给后台设
vite `base: "/admin/"`，它的资源就落在 `/admin/assets/` 下。**已经改好了。**

### 主题包结构（照 komari 的形状）

```
<themes-dir>/<short>/
    theme.json      清单：name / short / description / version / author / url
    dist/           构建产物，index.html 在它根下
    preview.png     可选
```

komari 的实际布局可参考本机 `/opt/komari/data/theme/Lumina/`（已安装的主题）和
`/opt/komari-theme-Lumina/`（主题源码仓库）。

## 已经做完的部分

**只有前端目录拆分，而且从未构建过、完全未验证。** Rust 侧一行没动。

| 路径 | 状态 |
|---|---|
| `web-admin/` | 从 `web/` 复制后删掉公开页组件，只剩 `Admin.tsx` / `Login.tsx`；`App.tsx` 已重写；`vite.config.ts` 已设 `base: "/admin/"`；标题已改 |
| `web-theme/` | 从 `web/` 复制后删掉后台组件，只剩 `Meter` / `NodeCard` / `NodeDetail` / `Summary` / `TrafficRing`；`App.tsx` 已重写；用不到的 shadcn 组件已删；`theme.json` 已建 |
| `web/` | **原封不动**。线上跑的、hub 当前 `rust-embed` 指向的仍然是它 |

⚠️ **两个新目录都没有 `node_modules`，没跑过 `npm install`，没跑过 `npm run build`，
一次都没有编译验证过。里面很可能有 import 残留或类型错误。** 第一步就该是把它们构建通。

`main` 分支是绿的：34 个 Rust 测试通过，`web/` 完好，`cargo build --release` 正常，
线上部署不受影响——新增的两个目录对 hub 没有任何影响。

## 待办（按顺序）

### 1. 把两个前端构建通过

```bash
cd web-admin && npm install && npm run build
cd ../web-theme && npm install && npm run build
```

预期会遇到的问题：

- `web-admin/src/lib/api.ts` 里 `useNodes()` 返回的 `admin` 字段在新 App.tsx 里没用到，
  TS 的 `noUnusedLocals` 会报错
- `web-theme` 删掉了 `sonner.tsx`，但 `package.json` 里还留着 `sonner` 依赖；
  `NodeDetail.tsx` 可能还 import 了已删除的 shadcn 组件
- 两边的 `api.ts` / `format.ts` / `utils.ts` 是完整拷贝，各自都有用不到的部分，
  能删就删，但**不要为了删而删出编译错误**

### 2. hub 支持运行时加载主题

这是本次工作的核心，全部在 `src/main.rs` 的 `serve_asset()` 附近，建议抽成
`src/frontend.rs`。

路由决策：

```
/api/*          → 已有逻辑，未匹配返回 404（不要退回 SPA，见 decisions.md）
/admin, /admin/*→ 内置后台（rust-embed，资源在 /admin/assets/）
其它            → 当前选中的主题
                    磁盘上有 → 从 <themes-dir>/<short>/dist/ 服务
                    没有     → 退回内置的默认主题（保证零配置可用）
```

必须做到的几点：

- **路径穿越防护。** 这是唯一一处按用户可控路径读磁盘的地方。必须
  `canonicalize()` 之后校验结果仍在主题目录内，`..` 和符号链接都要挡住。
  这条不能省，也不要"回头再加"
- **主题内的 SPA fallback。** 主题目录里找不到的路径要返回该主题的 `index.html`，
  这样主题自己的前端路由刷新才不会 404
- **内置默认主题不能删。** 全新部署、主题目录不存在、选中的主题被删掉——
  这三种情况都要能正常打开页面。零配置启动是刻意的设计（见 decisions.md）
- **缓存头**：带 hash 的 `assets/` 用 `immutable`，入口 HTML 用 `no-cache`。
  已有逻辑照搬即可

新增配置：

- `--themes <dir>` 命令行参数，默认取数据库文件所在目录下的 `themes/`
- `theme` 设置项（存 `setting` 表），值是主题的 `short`，空或 `default` 表示内置主题

新增接口：

- `GET /api/themes` → 扫描主题目录，返回每个主题的 `theme.json` + 是否为当前选中。
  **要带 `_: Admin`**（见 [security.md](security.md) 的自查清单）

### 3. 后台加一个主题选择界面

`web-admin/src/components/Admin.tsx` 里已经有侧边栏结构（`ADMIN_SECTIONS`），
加一项「主题」即可，或者并进设置页。至少要能列出已安装主题并切换。

**先不要做 zip 上传。** 那需要引入 zip 依赖并处理 zip-slip 路径穿越，是这块最容易
出安全问题的地方。scp 一个目录进去 + 面板里选，已经够用。等基础功能跑通再说。

### 4. hub 构建流程要能产出内置默认主题

`rust-embed` 现在指向 `web/dist`。拆完之后需要两份嵌入产物：

- 后台：`web-admin/dist`
- 内置默认主题：`web-theme/dist`

`.github/workflows/release.yml` 里的 `npm ci && npm run build` 要相应改成构建两个目录。

### 5. 把主题移出去成为独立仓库

**放在最后做。** 前面几步先在单仓库内验证通过，再移目录，否则出了问题很难定位是
拆仓库导致的还是逻辑本身的问题。

- 新仓库 `stqfdyr/monitor-theme-default`（用户的 komari 主题仓库命名习惯是
  `komari-theme-<Name>`，可参考）
- 用 `git subtree split --prefix=web-theme` 保留历史，参考 agent 仓库的拆分方式
  （见 commit `5cd0c20`）
- hub 仓库如何拿到内置默认主题：CI 里 clone + build 主题仓库然后嵌入。
  这会引入跨仓库构建依赖，是拆分的已知代价，用户已接受
- 主题仓库需要自己的 README，说清楚**主题契约**：可以调哪些接口、`theme.json`
  的字段、怎么本地开发（vite 代理到 hub）

### 6. 收尾

- 改写 [decisions.md](decisions.md) 的前端章节：原文写的是"前端不单独开仓库"，
  连同理由一起改成现在的决定和新的理由
- 更新 [architecture.md](architecture.md) 的仓库表和源码地图
- 更新 [development.md](development.md)：两个前端怎么各自开发、主题怎么本地调试
- 根 `README.md` 和 `CLAUDE.md` 里"前端没有单独仓库"的说法都要改
- 删掉 `web/`（被两个新目录取代）
- **删掉本文件**

## 主题契约（拆仓库时要写进主题仓库的 README）

主题是纯静态站点，只通过这些接口和 hub 通信。这些接口一旦对外，就成了不能随意改的
公开契约——这是拆主题的真实代价，动它们之前想清楚。

| 接口 | 用途 |
|---|---|
| `GET /api/me` | 站点名、是否已登录、公开页是否开启 |
| `GET /api/nodes` | 节点列表（含实时指标和累计流量）。未登录时只返回 `public=1` 的节点，且不含 `ip` / `hostname` / `remark` |
| `GET /api/nodes/{id}/metrics?hours=N` | 历史曲线和延迟记录 |
| `GET /api/ws` | 每 2 秒推一次快照 |
| `GET /api/ping-tasks` | 仅管理员可读，用来给延迟图的曲线取名。未登录时会 401，主题应当容错 |

字段的权威定义在 `src/api.rs` 的 `node_view()`。

## 其它注意事项

- 用 `ponytail` skill（full），别过度设计。用户明确要求过
- 面板新接口签名里必须有 `_: Admin`
- 用户的部署在 `m.3301921.xyz`，改完要重新构建部署验证，不能只跑测试
- 机器上有 puppeteer 的 Chrome，可以驱动浏览器截图核对界面，用法见
  [development.md](development.md)
- 别用 `pkill -f` 停进程，会匹配到执行命令的 shell 自己
