# 数据口径

Scout 的内存和硬盘数字不对，这是迁到 monitor 要修的原始 bug 之一。改 `agent/src/collect.rs` 之前请读完这篇。

**验收标准很硬：面板上的数字必须和目标机器上 `free` / `df` 的输出对得上。**

## 内存

### 错在哪

Scout 用的是 `sysinfo` 的 `used_memory()`。sysinfo 把 **page cache 算成已用内存**。Linux 会拿所有空闲内存做磁盘缓存，所以一台开机跑了几天的机器，cache 通常有好几个 GB——面板上就显示成内存快满了，实际上完全没有压力。

### 现在怎么算

```rust
used = MemTotal - MemAvailable
```

`MemAvailable` 是内核自己给出的估计值：**在不触发 swap 的前提下还能分配多少**。它已经扣掉了可回收的 cache 和 slab。

这也正好是现代 `free(1)` 的 used 列的算法（procps 的 `free.c`：`mem_used = kb_main_total - kb_main_available`）。

`MemAvailable` 不存在时（3.14 以前的内核、某些容器）退回 `MemFree + Buffers + Cached`。

### 为什么不用 htop 的公式

htop / komari 用的是：

```
used = MemTotal - (MemFree + Buffers + Cached + SReclaimable) + Shmem
```

在实机上这个值比 `free` 少约 0.25 GB（差在 `MemAvailable` 对 slab 可回收部分的保守估计）。两个都"对"，但：

- 用户对照的是 `free -h`，不是 htop
- 一行减法 vs 五个字段的公式，前者更难写错

原本第一版实现的就是 htop 公式，实机对照后换掉了。commit `8fa5ab8` 之后的修改。

### swap

```rust
used = SwapTotal - SwapFree - SwapCached
```

`SwapCached` 是已经换回内存但 swap 里还留着副本的页，不算真正占用。

## 硬盘

### 错在哪

Scout 用 `total_space - available_space`。

ext4 默认给 root 预留 5% 的块（`tune2fs -m`）。这部分块普通用户用不了，所以不计入 `available`，但它们**也没被使用**。用 `total - available` 就等于把这 5% 记成已用——一块刚格式化的 100 GB 盘会显示已用 5 GB。

### 现在怎么算

```rust
total = f_blocks * f_frsize
used  = (f_blocks - f_bfree) * f_frsize    // f_bfree 是原始空闲块，含预留
```

这和 `df` 的 Used 列**完全一致**（不是近似，是逐字节相同）。

区别就在 `f_bfree`（所有空闲块）和 `f_bavail`（非特权用户可用的块）——差值就是 root 预留量。

### 挂载点去重

`parse_mounts()` 读 `/proc/self/mounts`，然后：

- 按 fstype 排除伪文件系统（tmpfs、proc、sysfs、overlay、squashfs……完整名单在 `SKIP_FSTYPES`）
- 排除设备名不以 `/` 开头的（zfs 和 btrfs 例外，它们的"设备"是池名/子卷）
- **按源设备去重**——同一块盘挂两次（bind mount、btrfs 子卷）只算一次
- zfs 按池名去重（`tank/set1` 和 `tank/set2` 共享 `tank` 的空间）

不去重的话，一台有 bind mount 的机器硬盘容量会翻倍。

测试：`mounts_drop_pseudo_filesystems_and_duplicate_devices`。

## CPU

读 `/proc/stat` 第一行的 jiffies，算**两次采样之间的差值**：

```
busy% = (总增量 - idle 增量) / 总增量 * 100
```

`idle` 取 `idle + iowait` 两项之和（都是 CPU 没在干活的时间）。

**第一次采集返回 0**，因为没有基线。返回自开机以来的平均值会是个毫无意义的数字。

## 网络速率

`net_rx` / `net_tx` 是 agent 自己算的瞬时速率（B/s），用两次采样的计数器差值除以经过的时间。

计数器回退时返回 `0` 而不是负数或巨大的正数——不要在重启的瞬间画出一根冲天的尖峰。

`net_rx_total` / `net_tx_total` 是**原样上报**的内核计数器，累加是 hub 的事，见 [traffic.md](traffic.md)。

网卡过滤：`SKIP_IFACES` 排除 lo、docker、veth、br-、virbr、tap、tun、cni 等前缀。`--skip-iface` 可以追加（比如你不想把 wireguard 的流量算进去）。

## 怎么验证

`collect.rs` 里有个 `crosscheck` 测试模块专门干这个：

```bash
cargo test -p monitor-agent crosscheck -- --nocapture
free -b  | awk 'NR==2{printf "free used=%.2fG total=%.2fG\n", $3/1073741824, $2/1073741824}'
df -B1 --output=size,used / | tail -1
```

**硬盘必须逐字节相同。内存允许几十 MB 的差异**（两次采样之间机器还在跑）。差到几百 MB 或几个 GB 就是口径错了。

最近一次实机对照（Debian 12，3.8 GiB 内存，59 GiB 盘）：

| | monitor | 系统工具 |
|---|---|---|
| 内存 | 1.01 GiB / 3.82 GiB | `free` 1.02 / 3.82 |
| 硬盘 | 11.83 GiB / 58.94 GiB | `df` 11.83 / 58.94 |

## 不要引入 sysinfo

agent 只跑 Linux（用户的决定，见 [decisions.md](decisions.md)），直接读 `/proc` 更准、更小、更好懂。sysinfo 的内存和硬盘口径就是这篇文档在修的东西——把它加回来等于把 bug 加回来。
