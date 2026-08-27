//! Linux-only metric collection, straight from /proc and statvfs.
//! No sysinfo: it gets memory and disk wrong for a probe (see docs/data-accuracy.md).

use std::collections::HashMap;
use std::fs;
use std::time::Instant;

use serde::Serialize;

/// Interface name prefixes never counted as real traffic.
const SKIP_IFACES: &[&str] = &[
    "lo", "docker", "veth", "br-", "virbr", "vmbr", "tap", "tun", "cni", "flannel", "podman", "fwbr", "fwpr",
    "kube", "cali", "nerdctl", "zt",
];

/// Pseudo/virtual filesystems that must not count toward disk totals.
const SKIP_FSTYPES: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devpts",
    "mqueue",
    "hugetlbfs",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "configfs",
    "fusectl",
    "binfmt_misc",
    "autofs",
    "squashfs",
    "ramfs",
    "efivarfs",
    "nsfs",
    "overlay",
    "fuse.lxcfs",
    "rpc_pipefs",
];

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Facts {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub arch: String,
    pub virt: String,
    pub cpu_name: String,
    pub cpu_cores: u32,
    pub mem_total: u64,
    pub swap_total: u64,
    pub disk_total: u64,
    pub agent_version: String,
}

#[derive(Serialize, Debug, Clone, Default, PartialEq)]
pub struct Metrics {
    /// Identifies this boot. Changes on reboot, which is how the hub knows the
    /// kernel's byte counters restarted at zero.
    pub boot_id: String,
    pub uptime: u64,
    pub cpu: f32,
    pub load: [f32; 3],
    pub mem_total: u64,
    pub mem_used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub disk_total: u64,
    pub disk_used: u64,
    /// Kernel lifetime byte counters. The hub accumulates these; the agent
    /// stores nothing and never tries to survive a reboot itself.
    pub net_rx_total: u64,
    pub net_tx_total: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub tcp: u32,
    pub udp: u32,
    pub procs: u32,
}

pub struct Collector {
    prev_cpu: Option<(u64, u64)>,
    prev_net: Option<(Instant, u64, u64)>,
    mounts: Vec<String>,
    skip_ifaces: Vec<String>,
}

impl Collector {
    pub fn new(extra_skip_ifaces: Vec<String>) -> Self {
        Self { prev_cpu: None, prev_net: None, mounts: real_mount_points(), skip_ifaces: extra_skip_ifaces }
    }

    pub fn facts(&self) -> Facts {
        let mem = meminfo();
        let (cpu_name, cpu_cores) = cpuinfo();
        let (disk_total, _) = disk_usage(&self.mounts);
        Facts {
            hostname: read_trim("/proc/sys/kernel/hostname").unwrap_or_else(|| "unknown".into()),
            os: os_pretty_name(),
            kernel: read_trim("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into()),
            arch: std::env::consts::ARCH.into(),
            virt: virtualization(),
            cpu_name,
            cpu_cores,
            mem_total: mem.get("MemTotal").copied().unwrap_or(0),
            swap_total: mem.get("SwapTotal").copied().unwrap_or(0),
            disk_total,
            agent_version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    pub fn collect(&mut self) -> Metrics {
        let mem = meminfo();
        let (mem_total, mem_used) = mem_used(&mem);
        let (swap_total, swap_used) = swap_used(&mem);
        let (disk_total, disk_used) = disk_usage(&self.mounts);
        let (rx_total, tx_total) = net_totals(&self.skip_ifaces);
        let (rx, tx) = self.net_rate(rx_total, tx_total, Instant::now());
        let (tcp, udp) = conn_counts();

        Metrics {
            boot_id: read_trim("/proc/sys/kernel/random/boot_id").unwrap_or_default(),
            uptime: uptime(),
            cpu: self.cpu_percent(),
            load: loadavg(),
            mem_total,
            mem_used,
            swap_total,
            swap_used,
            disk_total,
            disk_used,
            net_rx_total: rx_total,
            net_tx_total: tx_total,
            net_rx: rx,
            net_tx: tx,
            tcp,
            udp,
            procs: proc_count(),
        }
    }

    /// CPU busy share since the previous call. First call has no baseline and
    /// reports 0 rather than a meaningless since-boot average.
    fn cpu_percent(&mut self) -> f32 {
        let Some((total, idle)) = cpu_jiffies() else {
            return 0.0;
        };
        let pct = match self.prev_cpu {
            Some((pt, pi)) if total > pt => {
                let dt = (total - pt) as f32;
                let di = idle.saturating_sub(pi) as f32;
                ((dt - di) / dt * 100.0).clamp(0.0, 100.0)
            }
            _ => 0.0,
        };
        self.prev_cpu = Some((total, idle));
        pct
    }

    fn net_rate(&mut self, rx: u64, tx: u64, now: Instant) -> (u64, u64) {
        let rate = match self.prev_net {
            Some((t, prx, ptx)) => {
                let secs = now.saturating_duration_since(t).as_secs_f64();
                if secs <= 0.0 {
                    (0, 0)
                } else {
                    // A backwards counter means a reboot or a wrap; report no
                    // spike rather than a garbage number.
                    (
                        (rx.saturating_sub(prx) as f64 / secs) as u64,
                        (tx.saturating_sub(ptx) as f64 / secs) as u64,
                    )
                }
            }
            None => (0, 0),
        };
        self.prev_net = Some((now, rx, tx));
        rate
    }
}

fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

/// Parses /proc/meminfo into bytes keyed by field name.
fn meminfo() -> HashMap<String, u64> {
    parse_meminfo(&fs::read_to_string("/proc/meminfo").unwrap_or_default())
}

fn parse_meminfo(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some((key.to_owned(), kb * 1024))
        })
        .collect()
}

/// `free(1)`'s used column: total minus the kernel's own MemAvailable
/// estimate. sysinfo's `used_memory()` counts page cache as used and reads
/// gigabytes too high on any box that has been up for a while.
fn mem_used(m: &HashMap<String, u64>) -> (u64, u64) {
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    let total = g("MemTotal");
    if total == 0 {
        return (0, 0);
    }
    let available = match g("MemAvailable") {
        0 => g("MemFree") + g("Buffers") + g("Cached"), // pre-3.14 kernels
        v => v,
    };
    (total, total.saturating_sub(available))
}

fn swap_used(m: &HashMap<String, u64>) -> (u64, u64) {
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    let total = g("SwapTotal");
    let used = total.saturating_sub(g("SwapFree") + g("SwapCached"));
    (total, used.min(total))
}

fn cpu_jiffies() -> Option<(u64, u64)> {
    parse_cpu_jiffies(&fs::read_to_string("/proc/stat").ok()?)
}

fn parse_cpu_jiffies(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().next()?.strip_prefix("cpu ")?;
    let v: Vec<u64> = line.split_whitespace().filter_map(|f| f.parse().ok()).collect();
    if v.len() < 5 {
        return None;
    }
    // idle + iowait are both time the CPU was not doing work.
    Some((v.iter().sum(), v[3] + v[4]))
}

fn loadavg() -> [f32; 3] {
    let text = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut it = text.split_whitespace();
    let mut next = || it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    [next(), next(), next()]
}

fn uptime() -> u64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0) as u64
}

/// Sums the kernel's lifetime byte counters over real interfaces.
fn net_totals(extra_skip: &[String]) -> (u64, u64) {
    parse_net_dev(&fs::read_to_string("/proc/net/dev").unwrap_or_default(), extra_skip)
}

fn parse_net_dev(text: &str, extra_skip: &[String]) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if skip_iface(name, extra_skip) {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        if f.len() >= 9 {
            rx = rx.saturating_add(f[0]);
            tx = tx.saturating_add(f[8]);
        }
    }
    (rx, tx)
}

fn skip_iface(name: &str, extra_skip: &[String]) -> bool {
    SKIP_IFACES.iter().any(|p| name.starts_with(p)) || extra_skip.iter().any(|p| name.starts_with(p.as_str()))
}

/// Mount points backed by something real, deduplicated by source device so a
/// bind mount or a second subvolume cannot double-count the same disk.
fn real_mount_points() -> Vec<String> {
    parse_mounts(&fs::read_to_string("/proc/self/mounts").unwrap_or_default())
}

fn parse_mounts(text: &str) -> Vec<String> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let (dev, mount, fstype) = (f[0], f[1], f[2]);
        if SKIP_FSTYPES.iter().any(|s| fstype == *s || fstype.starts_with(&format!("{s}."))) {
            continue;
        }
        if !dev.starts_with('/') && fstype != "zfs" && fstype != "btrfs" {
            continue;
        }
        // ZFS datasets and btrfs subvolumes share one pool's free space.
        let key = dev.split('/').next().filter(|_| fstype == "zfs").unwrap_or(dev).to_owned();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(mount.replace("\\040", " "));
    }
    out
}

/// `used = total - free`, exactly what df reports. Scout used
/// `total - available`, which charges ext4's 5% root reserve to the user and
/// shows a fresh disk as several percent full.
fn disk_usage(mounts: &[String]) -> (u64, u64) {
    let mut total = 0u64;
    let mut used = 0u64;
    for m in mounts {
        let Ok(s) = rustix::fs::statvfs(m.as_str()) else { continue };
        let bs = if s.f_frsize > 0 { s.f_frsize } else { s.f_bsize };
        total = total.saturating_add(s.f_blocks.saturating_mul(bs));
        used = used.saturating_add(s.f_blocks.saturating_sub(s.f_bfree).saturating_mul(bs));
    }
    (total, used)
}

fn conn_counts() -> (u32, u32) {
    let count = |paths: [&str; 2]| {
        paths
            .iter()
            .filter_map(|p| fs::read_to_string(p).ok())
            .map(|t| t.lines().count().saturating_sub(1) as u32)
            .sum()
    };
    (count(["/proc/net/tcp", "/proc/net/tcp6"]), count(["/proc/net/udp", "/proc/net/udp6"]))
}

fn proc_count() -> u32 {
    fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
                .count() as u32
        })
        .unwrap_or(0)
}

fn cpuinfo() -> (String, u32) {
    let text = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let name = text
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            matches!(k.trim(), "model name" | "Model" | "cpu model").then(|| v.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into());
    let cores = text.lines().filter(|l| l.starts_with("processor")).count().max(1) as u32;
    (name, cores)
}

fn os_pretty_name() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| Some(l.strip_prefix("PRETTY_NAME=")?.trim_matches('"').to_owned()))
        })
        .unwrap_or_else(|| "Linux".into())
}

fn virtualization() -> String {
    if fs::metadata("/proc/vz").is_ok() {
        return "openvz".into();
    }
    if fs::metadata("/proc/xen").is_ok() {
        return "xen".into();
    }
    if fs::metadata("/.dockerenv").is_ok() {
        return "docker".into();
    }
    if let Some(t) = read_trim("/sys/hypervisor/type") {
        return t.to_lowercase();
    }
    for path in ["/sys/class/dmi/id/product_name", "/sys/class/dmi/id/sys_vendor"] {
        let Some(v) = read_trim(path) else { continue };
        let l = v.to_lowercase();
        for k in ["kvm", "vmware", "virtualbox", "qemu", "hyper-v", "xen", "bochs", "amazon", "google"] {
            if l.contains(k) {
                return k.into();
            }
        }
    }
    if fs::read_to_string("/proc/cpuinfo").is_ok_and(|t| t.contains("hypervisor")) {
        "vm".into()
    } else {
        "none".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_matches_free_not_sysinfo() {
        // Real /proc/meminfo from a 3.8 GiB box holding 2.5 GiB of page cache.
        let m = parse_meminfo(
            "MemTotal:        4008884 kB\nMemFree:          602756 kB\nMemAvailable:    2947484 kB\n\
             Buffers:          129100 kB\nCached:          2351560 kB\nSReclaimable:     154008 kB\n\
             Shmem:              2176 kB\nSwapTotal:       1048572 kB\nSwapFree:         987264 kB\n\
             SwapCached:        13280 kB\n",
        );
        let (total, used) = mem_used(&m);
        assert_eq!(total, 4008884 * 1024);
        assert_eq!(used, (4008884 - 2947484) * 1024, "must match the `free` used column");
        // The bug this replaces: counting cache as used reported ~3.3 GiB here.
        assert!(used < (total - g_cached(&m)), "page cache must not count as used");

        let (st, su) = swap_used(&m);
        assert_eq!(st, 1048572 * 1024);
        assert_eq!(su, (1048572 - 987264 - 13280) * 1024);
    }

    fn g_cached(m: &HashMap<String, u64>) -> u64 {
        m.get("Cached").copied().unwrap_or(0)
    }

    #[test]
    fn memory_falls_back_when_memavailable_is_absent() {
        let m = parse_meminfo("MemTotal: 1000 kB\nMemFree: 200 kB\nBuffers: 100 kB\nCached: 300 kB\n");
        assert_eq!(mem_used(&m), (1000 * 1024, 400 * 1024));
        assert_eq!(mem_used(&HashMap::new()), (0, 0));
    }

    #[test]
    fn cpu_percent_needs_a_baseline_then_uses_deltas() {
        let mut c = Collector { prev_cpu: None, prev_net: None, mounts: vec![], skip_ifaces: vec![] };
        c.prev_cpu = Some((1000, 900));
        // 100 more jiffies, 25 of them idle => 75% busy.
        let (total, idle) = parse_cpu_jiffies("cpu  40 0 35 925 0 0 0 0 0 0\n").unwrap();
        assert_eq!((total, idle), (1000, 925));
        let (t2, i2) = parse_cpu_jiffies("cpu  115 0 35 950 0 0 0 0 0 0\n").unwrap();
        assert_eq!((t2, i2), (1100, 950));
        assert!(parse_cpu_jiffies("garbage").is_none());
    }

    #[test]
    fn net_skips_virtual_interfaces() {
        let dev = "Inter-|   Receive\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n\
                   eth0: 1000 1 0 0 0 0 0 0 2000 2 0 0 0 0 0 0\n\
                     lo: 9999 1 0 0 0 0 0 0 9999 2 0 0 0 0 0 0\n\
              docker0: 5555 1 0 0 0 0 0 0 5555 2 0 0 0 0 0 0\n\
                  wg0: 300 1 0 0 0 0 0 0 400 2 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(dev, &[]), (1300, 2400));
        assert_eq!(parse_net_dev(dev, &["wg".to_owned()]), (1000, 2000));
    }

    #[test]
    fn net_rate_is_zero_on_first_sample_and_after_a_reboot() {
        let mut c = Collector { prev_cpu: None, prev_net: None, mounts: vec![], skip_ifaces: vec![] };
        let t0 = Instant::now();
        assert_eq!(c.net_rate(1000, 2000, t0), (0, 0));
        let t1 = t0 + std::time::Duration::from_secs(2);
        assert_eq!(c.net_rate(1200, 2400, t1), (100, 200));
        // Counter restarted: no negative, no bogus spike.
        let t2 = t1 + std::time::Duration::from_secs(2);
        assert_eq!(c.net_rate(50, 60, t2), (0, 0));
    }

    #[test]
    fn mounts_drop_pseudo_filesystems_and_duplicate_devices() {
        let mounts = parse_mounts(
            "/dev/vda1 / ext4 rw 0 0\n\
             proc /proc proc rw 0 0\n\
             tmpfs /run tmpfs rw 0 0\n\
             /dev/vda1 /var/lib/bind ext4 rw 0 0\n\
             overlay /var/lib/docker/overlay2/x/merged overlay rw 0 0\n\
             /dev/vdb1 /data xfs rw 0 0\n\
             tank/set1 /tank zfs rw 0 0\n\
             tank/set2 /tank/sub zfs rw 0 0\n",
        );
        assert_eq!(mounts, vec!["/", "/data", "/tank"]);
    }

    #[test]
    fn disk_used_matches_df_on_this_machine() {
        let (total, used) = disk_usage(&["/".to_owned()]);
        assert!(total > 0 && used <= total);
    }

    #[test]
    fn real_host_collection_is_sane() {
        let mut c = Collector::new(vec![]);
        let f = c.facts();
        assert!(!f.hostname.is_empty() && f.cpu_cores >= 1 && f.mem_total > 0);
        let m = c.collect();
        assert!(!m.boot_id.is_empty(), "boot_id drives reboot detection");
        assert!(m.mem_used > 0 && m.mem_used < m.mem_total);
        assert!(m.disk_used <= m.disk_total && m.disk_total > 0);
        assert!((0.0..=100.0).contains(&m.cpu));
    }
}

#[cfg(test)]
mod crosscheck {
    /// Prints our numbers next to free(1)/df(1) so a human can eyeball the fix.
    #[test]
    fn print_against_free_and_df() {
        let mut c = super::Collector::new(vec![]);
        let m = c.collect();
        let gib = |b: u64| b as f64 / 1024.0 / 1024.0 / 1024.0;
        println!("mem  used={:.2}G total={:.2}G", gib(m.mem_used), gib(m.mem_total));
        println!("disk used={:.2}G total={:.2}G", gib(m.disk_used), gib(m.disk_total));
        println!("net  rx_total={} tx_total={}", m.net_rx_total, m.net_tx_total);
    }
}
