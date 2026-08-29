# monitor

Rust 服务器探针的 **hub**，功能只有四件事：看状态、看流量、看延迟、算成本。

agent 在 [独立仓库](https://github.com/stqfdyr/agent)。后台在 `web-admin/`；默认公开页主题在
[monitor-theme-default](https://github.com/stqfdyr/monitor-theme-default)，开发时检出到被忽略的
`web-theme/`，两份构建产物都由 `rust-embed` 编译进二进制。外部主题由 hub 在运行时从磁盘读取。

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
cargo test
cargo clippy --all-targets
cd web-admin && npm run build
cd web-theme && npm run build      # 先 clone monitor-theme-default 到这里
cargo run -- --listen 127.0.0.1:9911 --db /tmp/dev.db --themes /tmp/themes --site http://127.0.0.1:9911
```

**别用 `pkill -f` 停进程**——会匹配到跑命令的 shell 自己。用 `ss -lptn "sport = :9911"` 拿 PID 再 kill。

完整开发流程见 [docs/development.md](docs/development.md)。
