# monitor

Rust 服务器探针的 **hub**，替代 komari，功能砍到只剩：看状态、看流量、看延迟、算成本。

agent 在 [另一个仓库](https://github.com/stqfdyr/agent)。前端在 `web/`，由 `rust-embed` 编译进本二进制——**没有**单独的主题仓库，也不打算有。

> ⚠️ **有一件事没做完**：主题系统与前端拆分，见
> [docs/wip-theme-system.md](docs/wip-theme-system.md)。`web-admin/` 和 `web-theme/`
> 两个目录已拆出但**从未构建验证过**，Rust 侧还没开始。`web/` 仍是当前生效的前端。

**动手之前先读 [docs/](docs/)**，尤其是 [docs/decisions.md](docs/decisions.md)——里面记了每个选择的理由和被否决的方案。

## 三条铁律

1. **总流量永不回退。** VPS 重启、hub 重启、agent 掉线，累计值都要继续加。见 [docs/traffic.md](docs/traffic.md)
2. **内存和硬盘必须和 `free` / `df` 对得上。** 见 [agent 仓库的 data-accuracy.md](https://github.com/stqfdyr/agent/blob/main/docs/data-accuracy.md)
3. **公开状态页永远不输出 IP、主机名、备注。** 见 [docs/security.md](docs/security.md)

## 明确不做的

不要"顺手"加回来，想加先问用户：通知、远程 SSH、插件系统、ICMP/HTTP ping、agent 自动更新、跨平台 agent。

## 工作方式

- 用 `ponytail` skill（full），别过度设计、别过度测试。一段非平凡逻辑留一个能跑的检查就够，不要每个函数一个测试
- 用现成的主流组件，但过重的宁可自己写
- 除了已经定下来的，其它取舍问用户，别自己定
- 面板新接口的签名里必须有 `_: Admin`；新的节点字段默认放 `node_view()` 的 `full` 分支

## 常用命令

```bash
cargo test                        # 34 个测试
cargo clippy --all-targets
cd web && npm run build                       # hub 嵌入 web/dist，改前端后要重跑
cargo run -- --listen 127.0.0.1:9911 --db /tmp/dev.db --site http://127.0.0.1:9911
```

**别用 `pkill -f` 停进程**——会匹配到跑命令的 shell 自己。用 `ss -lptn "sport = :9911"` 拿 PID 再 kill。

完整开发流程见 [docs/development.md](docs/development.md)。
