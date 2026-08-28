# monitor 文档

给后续接手的人（含 AI session）看的。用户面向的部署说明在仓库根目录的 [README.md](../README.md)，这里放的是**为什么这么做**。

## 按这个顺序读

| 文档 | 什么时候需要 |
|---|---|
| [architecture.md](architecture.md) | 第一次接触这个项目。组件、数据流、线上协议、数据模型 |
| [decisions.md](decisions.md) | **动手改之前必读。** 每个选择的理由，以及被否决的方案 |
| [traffic.md](traffic.md) | 碰流量相关代码之前。这是项目的核心特性，有一个不变量必须守住 |
| [data-accuracy.md](https://github.com/stqfdyr/agent/blob/main/docs/data-accuracy.md) | 碰 agent 采集代码之前。内存/硬盘/CPU 的口径和验证方法 |
| [security.md](security.md) | 碰鉴权、API 边界、公开页之前 |
| [development.md](development.md) | 要构建、测试、本地跑起来 |
| [benchmark.md](benchmark.md) | 想知道比 komari 快多少、省多少。也是调优改动的记录 |

## 30 秒版本

服务器探针的 hub。agent 在 [另一个仓库](https://github.com/stqfdyr/agent)。用来替代 komari——功能砍到只剩四件事：看状态、看流量、看延迟、算成本。

- **agent** 只跑 Linux，直接读 `/proc` 和 `statvfs`，无状态、不落盘
- **hub** 是 axum + SQLite，前端构建产物嵌进二进制，零配置文件启动
- **通信** WebSocket 上跑 JSON-RPC 2.0 通知，token 走 `Authorization` 头
- **前端** 内置后台与可替换公开主题都是 React + shadcn/ui；默认主题有独立仓库

## 三条铁律

改代码之前先确认没有违反这三条，它们是这个项目存在的理由：

1. **总流量永不回退。** VPS 重启、hub 重启、agent 掉线重连，累计值都必须继续往上加。见 [traffic.md](traffic.md)
2. **内存和硬盘的数字必须和 `free` / `df` 对得上。** 这是从 Scout 迁过来要修的原始 bug。见 [data-accuracy.md](https://github.com/stqfdyr/agent/blob/main/docs/data-accuracy.md)
3. **公开状态页永远不输出 IP、主机名和备注。** 见 [security.md](security.md)

## 明确不做的

下面这些是用户明确砍掉的，**不要"顺手"加回来**：

- 通知（离线告警、流量告警、任何形式的推送）
- 远程 SSH / web terminal
- 插件系统
- ICMP ping 和 HTTP ping（只保留 TCP）
- agent 自动更新（升级方式是重跑一遍安装命令）
- 跨平台 agent（Windows / macOS / BSD）

理由见 [decisions.md](decisions.md)。想加任何一条之前先问用户。
