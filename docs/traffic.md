# 流量累计

这是这个项目存在的主要理由之一。改这块代码之前请读完。

## 要解决的问题

网卡的流量数来自内核计数器（`/proc/net/dev` 里的 bytes），而这个计数器**每次开机从 0 开始**。
照原样显示，VPS 一重启面板上的「总流量」就归零。需求是重启后能续上，一直累加。

## 方案

**累加放在 hub 侧，agent 保持无状态。**

agent 每次上报两样东西：

- `net_rx_total` / `net_tx_total` — 内核 lifetime 计数器的当前值（原样，不加工）
- `boot_id` — 来自 `/proc/sys/kernel/random/boot_id`，每次开机变一次

hub 在 `db::accumulate()`（`src/db.rs`）里做这件事：

**只有一条规则：只计入 hub 亲眼看着计数器涨过去的那部分字节。**

```
if 这次的 boot_id 和上次存的一样:
    delta = max(当前读数 - 上次读数, 0)
else:
    delta = 0                   ← 这个 boot 下还没有基线，只对基线，不计流量

total_rx += delta               ← 单调递增，永不回退
month_rx += delta
last_rx = 当前读数              ← 无论哪条分支都更新
```

rx 和 tx 各判各的，一个方向变小不影响另一个方向的增量。

**任何情况下都不把「当前读数」本身当成增量。** 读数是这台机器的整机历史，不是一个差值。
没有基线可减的情况有三种，处理方式相同：

| 情况 | 为什么没有基线 |
|---|---|
| 节点第一次上报 | hub 还没见过它 |
| 同一个 boot 下读数变小 | 被计入的接口消失了（`wg0` 关掉、隧道停了，`SKIP_IFACES` 挡不住），当前读数是**剩余接口的历史累计** |
| `boot_id` 变了 | 计数器重新从 0 开始了——**或者**另一台机器正拿同一个 token 上报，从这里分不出来 |

赌错的代价差几个数量级，所以三种都选「重新对基线」：

- 猜错方向计进去：一次虚增一整个 lifetime 计数器（几百 GB），而且 `total` 单调递增，只能手工校正
- 重新对基线：重启那一次丢掉「开机到首次上报」之间的量。agent 由 systemd/OpenRC 在开机时拉起、
  连上就每秒上报，这个窗口通常是几十秒、几百 KB

**为什么 `boot_id` 变了也不计。** 曾经是计的，理由是「计数器刚从 0 开始，当前值就是重启后跑的
量」。但同一条信号还有另一个来源：**同一条安装命令粘到了第二台机器上**。两个 agent 都能通过鉴权，
在 `App.agents` 里互相踢掉对方再重连，hub 每秒看到两个 boot_id 来回切换，每次都带着各自机器的整机
计数器，实测每来回一轮涨 180 GB。这和「读数变小」是同一个判断。

32 位回绕不必考虑：`/proc/net/dev` 在 64 位内核上是 `u64`，实测 `rx` 早已跑到 2^32 的几十倍。

测试：`a_shrinking_reading_re_aligns_instead_of_re_counting_history`、
`two_machines_sharing_one_token_cannot_inflate_the_total`。

## 五个必须守住的不变量

### 1. 没有基线的读数只建立基线，不计流量

```rust
let (d_rx, d_tx) = if prev_boot.is_empty() || prev_boot != boot_id {
    (0, 0)
} else {
    ((rx - last_rx).max(0), (tx - last_tx).max(0))
};
```

新加的节点第一次上报时，网卡计数器可能已经是几百 GB。重启后的第一次上报同理——那个读数没有配对
的基线。任何时候把一个**读数**当成**增量**，记进去的都是整机历史。

测试：`traffic_survives_a_reboot_instead_of_resetting` 的第一个断言、
`two_machines_sharing_one_token_cannot_inflate_the_total`。

### 2. 累计流量不从历史明细算出来

`total_rx` / `total_tx` 存在 `traffic` 表里，是一个独立的、每次上报都更新的状态，
**不是 `SUM(metric.net_rx)`**。所以 `metric` 表可以按保留天数随便清理而累计值毫发无损：
`db::prune()` 只删 `metric` 和 `ping_record`，永远不碰 `traffic`。

测试：`prune_drops_history_but_never_traffic_totals`。

### 3. 月度重置只重置月度计数器

```rust
if month_start != period {
    month_rx = d_rx;      // 新周期只算这次上报的增量
    month_tx = d_tx;
}
// total_rx 不受影响
```

测试：`day_and_month_restart_independently_while_the_total_keeps_climbing`。

### 4. 今日计数器同理，但按本地时区跨天

`day_rx` / `day_tx` 和月度计数器结构一样，只是比较的是日期：`day_start != 今天` 就从这次上报的增量重新开始。

**两条边界都走 hub 所在机器的本地时区**，不是 UTC：重置日和「今天」都是人说的日期。原来月边界读
`Utc::now()`、日边界读 `Local::now()`，同一行上的两个计数器对「今天几号」给出不同答案——CST 的
hub 上商家月度重置实际发生在重置日当天 08:00，而「今日流量」在 00:00 就翻了。

历史明细里没有累计量，所以这个计数器上线当天只能从上线那一刻开始算。

测试：同上——日和月是同一条结构，一个测试把两条边界连着跨一遍。

### 5. 读取侧和写入侧守同一条口径

周期计数器是**惰性重置**的——只有节点下一次上报时 `accumulate()` 才会发现跨了边界。所以一个在
边界之前就掉线的节点，磁盘上留着的是**上一个周期**的字节数；原样读出来就是拿昨天的流量当今天的、
拿上个月的用量去画这个月配额的进度条。

`db::all_traffic()` 因此在读的时候再判一次周期：`day_start` 不是今天、`month_start` 不是本节点
当前的 `period_start()`，这两个计数器就答 0，`total_rx/total_tx` 不受影响。**这是全项目唯一的读取
入口**——规则有两个落脚点就有两个漏掉它的地方。

这条不能放到前端做：第三方主题得跟着重新实现一遍 `period_start()`（还要拿到节点的重置日），
少实现一次就又是一个显示错数字的主题。

测试：`a_node_that_went_quiet_before_a_boundary_reads_as_zero_this_period`。

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

`traffic` 表存的是分开的 `month_rx` 和 `month_tx`。至于额度按哪个算，看节点的 `traffic_mode`，在默认主题仓库 `src/components/NodeCard.tsx` 的 `monthUsage()` 里：

| mode | 计算 | 典型场景 |
|---|---|---|
| `sum` | rx + tx | 大部分商家 |
| `max` | max(rx, tx) | 按较大方向计费 |
| `up` | 仅 tx | 只限上行 |
| `down` | 仅 rx | 只限下行 |

画进度条的是主题仓库的 `src/components/Meter.tsx`。`traffic_limit` 为 0 时百分比显示 `—`，条留空，页脚写「不限」。

## agent 掉线期间的流量会被补上

这是特性不是 bug：读的是内核 lifetime 计数器，agent 挂掉时内核照样在计数，重连后的第一次 delta
把这段全包进去。流量确实跑了，商家确实会算。

**前提是机器没重启。** 掉线期间重启过的话 `boot_id` 变了，那一段没有基线可减，只能从重启后的第一
次上报重新开始算。这是上面那条规则的代价，换来的是两台机器共用一个 token 时总流量不会爆掉。

## 手动修正

面板可以改累计值：`PUT /api/nodes/{id}/traffic`，对应 `db::set_traffic()`。用途是换机器、迁移、或者修正一次误算。

## 怎么验证

逻辑由 `src/db.rs` 的 `tests` 模块覆盖。要验真机行为：

```bash
# 记下当前累计值
curl -s -b jar http://127.0.0.1:9911/api/nodes | python3 -c \
  'import sys,json; n=json.load(sys.stdin)["nodes"][0]; print(n["total_rx"], n["total_tx"])'

# 重启 hub（不是 kill -9，用 SIGTERM 走优雅关闭）
kill $(ss -lptn "sport = :9911" | grep -oP 'pid=\K[0-9]+')
./target/debug/monitor-hub --listen 127.0.0.1:9911 --db e2e.db &

# 等 agent 重连（连接稳定过的话是 1 秒），再查一次——必须只增不减
```

VPS 重启无法在本机模拟（`boot_id` 不会变）。单元测试用伪造的 `boot_id` 覆盖了这条路径。
