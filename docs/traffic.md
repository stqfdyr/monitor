# 流量累计

这是这个项目存在的主要理由之一。改这块代码之前请读完。

## 要解决的问题

komari 的 agent 上报的是网卡的内核计数器（`/proc/net/dev` 里的 bytes）。这个计数器**每次开机从 0 开始**。所以 VPS 一重启，面板上的"总流量"就归零，之前跑了多少全没了。

用户的原话：

> komari 的设计我不喜欢，它 agent 所在的 vps 只要一重启，显示的信息里总流量就清零了，我的探针要能续上，一直累加

## 方案

**累加放在 hub 侧，agent 保持无状态。**

agent 每次上报两样东西：

- `net_rx_total` / `net_tx_total` — 内核 lifetime 计数器的当前值（原样，不加工）
- `boot_id` — 来自 `/proc/sys/kernel/random/boot_id`，每次开机变一次

hub 在 `db::accumulate()`（`src/db.rs`）里做这件事：

```
rebooted = (boot_id 变了) 或 (rx < last_rx) 或 (tx < last_tx)

if rebooted:
    delta = 当前读数            ← 计数器刚从 0 开始，当前值就是重启后跑的量
else:
    delta = 当前读数 - 上次读数

total_rx += delta               ← 单调递增，永不回退
month_rx += delta
```

计数器回退但 `boot_id` 没变，说明是 32 位计数器溢出回绕或网卡被重置。处理方式和重启一样：把当前读数整个当增量。宁可少算一个回绕周期的量，也不能算出负数。

## 三个必须守住的不变量

### 1. 首次上报只建立基线，不计流量

```rust
let (d_rx, d_tx) = if prev_boot.is_empty() { (0, 0) } else { (d_rx, d_tx) };
```

新加的节点第一次上报时，网卡计数器可能已经是几百 GB 了（机器跑了很久）。如果不特判，这几百 GB 会一次性记到账上。

测试：`traffic_survives_a_reboot_instead_of_resetting` 的第一个断言。

### 2. 累计流量不从历史明细算出来

`total_rx` / `total_tx` 存在 `traffic` 表里，是一个独立的、每次上报都更新的状态。**它不是 `SUM(metric.net_rx)`。**

这样 `metric` 表可以按保留天数随便清理，累计值毫发无损。`db::prune()` 只删 `metric` 和 `ping_record`，永远不碰 `traffic`。

测试：`prune_drops_history_but_never_traffic_totals`。

### 3. 月度重置只重置月度计数器

```rust
if month_start != period {
    month_rx = d_rx;      // 新周期只算这次上报的增量
    month_tx = d_tx;
}
// total_rx 不受影响
```

测试：`month_counter_restarts_but_total_keeps_climbing`。

## 月度周期

每个节点有自己的 `traffic_reset_day`（1–31，商家的流量重置日）。`db::period_start(today, reset_day)` 算出当前周期的起始日期：

- 今天 >= 本月的重置日 → 周期从本月的重置日开始
- 否则 → 从上月的重置日开始
- 重置日超过当月天数就落到当月最后一天（重置日 31 号，2 月落到 28 或 29 号）
- 1 月往回退到上一年 12 月

存储的 `month_start` 是这个日期的字符串。每次上报都重算，和存的不一样就说明跨周期了，重置月度计数器。

**没有定时任务做重置**，是上报时惰性触发的。所以一个离线很久的节点重新上线时会正确地开始一个新周期。

测试：`period_start_handles_short_months_and_wraparound`。

## 月度用量怎么算

`traffic` 表存的是分开的 `month_rx` 和 `month_tx`。至于额度按哪个算，看节点的 `traffic_mode`，在前端 `web/src/components/NodeCard.tsx` 的 `monthUsage()` 里：

| mode | 计算 | 典型场景 |
|---|---|---|
| `sum` | rx + tx | 大部分商家 |
| `max` | max(rx, tx) | 按较大方向计费 |
| `up` | 仅 tx | 只限上行 |
| `down` | 仅 rx | 只限下行 |

圆环组件是 `web/src/components/TrafficRing.tsx`。`traffic_limit` 为 0 时显示"不限"，不画进度弧。

## 一个副作用（是特性不是 bug）

agent 掉线期间产生的流量，重连后会被补上。因为读的是内核 lifetime 计数器，agent 挂掉的这段时间内核照样在计数，重连后的第一次 delta 会把这段全包进去。

这是对的：流量确实跑了，商家确实会算。

## 手动修正

面板可以改累计值：`PUT /api/nodes/{id}/traffic`，对应 `db::set_traffic()`。用途是换机器、迁移、或者修正一次误算。

## 怎么验证

单元测试覆盖了逻辑（`src/db.rs` 的 `tests` 模块，7 个）。要验真机行为：

```bash
# 记下当前累计值
curl -s -b jar http://127.0.0.1:9911/api/nodes | python3 -c \
  'import sys,json; n=json.load(sys.stdin)["nodes"][0]; print(n["total_rx"], n["total_tx"])'

# 重启 hub（不是 kill -9，用 SIGTERM 走优雅关闭）
kill $(ss -lptn "sport = :9911" | grep -oP 'pid=\K[0-9]+')
./target/debug/monitor-hub --listen 127.0.0.1:9911 --db e2e.db &

# 等 agent 重连（退避最多 60 秒），再查一次——必须只增不减
```

VPS 重启无法在本机模拟（`boot_id` 不会变）。单元测试用伪造的 `boot_id` 覆盖了这条路径。
