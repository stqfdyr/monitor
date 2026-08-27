# monitor-theme-default

[monitor](https://github.com/stqfdyr/monitor) 的内置默认主题，也是开发第三方主题的参考实现。

## 开发

先在本机启动 hub：

```bash
monitor-hub --listen 127.0.0.1:9911 --db /tmp/monitor.db --site http://127.0.0.1:9911
```

再启动主题开发服务器；Vite 会把 `/api` 和 WebSocket 代理到 hub：

```bash
npm ci
npm run dev
```

提交前运行 `npm run build && npm run lint`。构建产物在 `dist/`。

## 主题包

一个可安装主题是一个目录，名字必须与 `theme.json` 的 `short` 相同：

```text
<themes-dir>/<short>/
├── theme.json
├── dist/
│   └── index.html
└── preview.png        # 可选
```

`theme.json` 包含这些字符串字段：

| 字段 | 含义 |
|---|---|
| `name` | 显示名称 |
| `short` | 唯一短名，只能使用字母、数字、`-`、`_`；`default` 保留给内置主题 |
| `description` | 简介 |
| `version` | 主题版本 |
| `author` | 作者 |
| `url` | 源码地址 |

把整个目录复制到 hub 的 `--themes` 目录后，在后台「主题」页选择即可；不需要重启 hub。

## 主题契约

主题是纯静态 SPA，只能依赖下列同源接口：

| 接口 | 用途 |
|---|---|
| `GET /api/me` | 站点名、登录状态、公开页开关 |
| `GET /api/nodes` | 节点列表、实时指标和累计流量 |
| `GET /api/nodes/{id}/metrics?hours=N` | 历史指标和延迟记录 |
| `GET /api/ws` | 每 2 秒推送一次节点快照的 WebSocket |
| `GET /api/ping-tasks` | 延迟曲线名称；仅管理员可读，匿名访问会返回 401，主题必须容错 |

匿名访问 `GET /api/nodes` 时只会得到 `public=1` 的节点，且响应中没有 `ip`、`hostname`、`remark`。字段的权威定义在 hub 的 `src/api.rs` 中。

未知前端路径会回落到主题自己的 `dist/index.html`，所以可以使用客户端路由。`/admin/*` 始终由 hub 内置后台接管，不属于主题契约。

## 许可

MIT
