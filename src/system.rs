//! This computer's own load, one tray icon per chosen metric (like
//! indicator-multiload, without re-registering the icons all the time).
//!
//! Memory is headroom, not "used": MemAvailable is what can still be had
//! without stalling, page cache included. MemTotal - MemFree counts
//! reclaimable cache as used and calls a machine with 48 G to spare full.
//! Pressure (PSI) is the only figure that says whether anything waited.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::icon::Column;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    Cpu,
    Memory,
    Swap,
    Network,
    Disk,
    Pressure,
}

impl Metric {
    pub const ALL: [Metric; 6] = [Metric::Cpu, Metric::Memory, Metric::Swap, Metric::Network, Metric::Disk, Metric::Pressure];

    pub fn key(self) -> &'static str {
        match self {
            Metric::Cpu => "cpu",
            Metric::Memory => "memory",
            Metric::Swap => "swap",
            Metric::Network => "network",
            Metric::Disk => "disk",
            Metric::Pressure => "pressure",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Metric::Cpu => "CPU",
            Metric::Memory => "Memory",
            Metric::Swap => "Swap",
            Metric::Network => "Network",
            Metric::Disk => "Disk",
            Metric::Pressure => "Pressure",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Metric::Cpu => "Busy share of all cores. A mark on top: tasks waited for a CPU (pressure).",
            Metric::Memory => {
                "The number is the headroom (available memory, page cache counted as free); \
                 the graph is what is really taken. A mark on top: something waited for memory."
            }
            Metric::Swap => "Swap in use (the graph as a share of all swap); zram, compressed in RAM, and disk swap apart in the menu.",
            Metric::Network => "Bytes in and out per second, all real interfaces (no loopback, Docker or bridges).",
            Metric::Disk => "Bytes read and written per second, all disks.",
            Metric::Pressure => "Worst of CPU, memory and I/O pressure: the share of time some task had to wait.",
        }
    }

    /// Its own hue: outside the green → red quality ramp, and distinct
    /// from the others so that two icons side by side never look alike.
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Metric::Cpu => [0xc0, 0x61, 0xcb],      // purple
            Metric::Memory => [0xcd, 0xab, 0x8f],   // sand
            Metric::Swap => [0xa5, 0x7c, 0x5a],     // brown
            Metric::Network => [0x99, 0xc1, 0xf1],  // light blue
            Metric::Disk => [0x9a, 0x99, 0x96],     // grey
            Metric::Pressure => [0xf6, 0xf5, 0xf4], // white
        }
    }

    /// Graph top: shares have a fixed scale, rates follow the data.
    fn fixed_ceiling(self) -> Option<f64> {
        match self {
            Metric::Cpu | Metric::Memory | Metric::Swap => Some(100.0),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemConfig {
    /// One tray icon each, in this order.
    pub metrics: Vec<Metric>,
}

impl Default for SystemConfig {
    fn default() -> Self {
        SystemConfig { metrics: vec![Metric::Cpu, Metric::Memory] }
    }
}

// ---- reading /proc ----

#[derive(Clone, Copy, Debug, Default)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

fn cpu_times(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 8 {
        return None;
    }
    // user nice system idle iowait irq softirq steal (guest is inside user)
    let busy = v[0] + v[1] + v[2] + v[5] + v[6] + v[7];
    Some(CpuTimes { busy, total: busy + v[3] + v[4] })
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Memory {
    pub total_kb: u64,
    pub available_kb: u64,
    /// Page cache and reclaimable kernel memory: counted as available.
    pub cache_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
}

fn memory(meminfo: &str) -> Memory {
    let field = |name: &str| {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|r| r.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    Memory {
        total_kb: field("MemTotal"),
        available_kb: field("MemAvailable"),
        cache_kb: field("Cached") + field("Buffers") + field("SReclaimable"),
        swap_total_kb: field("SwapTotal"),
        swap_free_kb: field("SwapFree"),
    }
}

/// Swap in use, in kB: (zram, other).
fn swap_split(swaps: &str) -> (u64, u64) {
    let mut out = (0, 0);
    for l in swaps.lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        let Some(used) = f.get(3).and_then(|u| u.parse::<u64>().ok()) else { continue };
        if f[0].contains("zram") { out.0 += used } else { out.1 += used }
    }
    out
}

/// RAM zram really uses for what it holds (compressed), in bytes.
fn zram_ram_bytes() -> u64 {
    let Ok(dir) = std::fs::read_dir("/sys/block") else { return 0 };
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("zram"))
        .filter_map(|e| std::fs::read_to_string(e.path().join("mm_stat")).ok())
        .filter_map(|s| s.split_whitespace().nth(2)?.parse::<u64>().ok())
        .sum()
}

/// "some avg10" of a /proc/pressure file, in percent.
fn psi_some(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some"))?;
    line.split_whitespace().find_map(|f| f.strip_prefix("avg10=")?.parse().ok())
}

fn psi(resource: &str) -> f64 {
    std::fs::read_to_string(format!("/proc/pressure/{resource}")).ok().and_then(|t| psi_some(&t)).unwrap_or(0.0)
}

/// Interfaces that carry real traffic, not copies of it.
fn real_interface(name: &str) -> bool {
    !["lo", "docker", "veth", "br-", "virbr", "vnet", "tun", "tap"].iter().any(|p| name.starts_with(p))
}

/// Bytes (received, sent) so far.
fn net_bytes(dev: &str) -> (u64, u64) {
    let mut out = (0, 0);
    for l in dev.lines().skip(2) {
        let Some((name, rest)) = l.split_once(':') else { continue };
        if !real_interface(name.trim()) {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if f.len() >= 9 {
            out.0 += f[0];
            out.1 += f[8];
        }
    }
    out
}

/// Whole disks only: partitions and device-mapper volumes would count twice.
fn whole_disk(name: &str) -> bool {
    !["loop", "ram", "zram", "dm-", "sr", "md"].iter().any(|p| name.starts_with(p))
        && std::path::Path::new("/sys/block").join(name).exists()
}

/// Bytes (read, written) so far.
fn disk_bytes(stats: &str, is_disk: impl Fn(&str) -> bool) -> (u64, u64) {
    let mut out = (0, 0);
    for l in stats.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 10 || !is_disk(f[2]) {
            continue;
        }
        let sectors = |i: usize| f[i].parse::<u64>().unwrap_or(0) * 512;
        out.0 += sectors(5);
        out.1 += sectors(9);
    }
    out
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Everything read at one instant.
#[derive(Clone, Copy, Debug, Default)]
struct Raw {
    cpu: CpuTimes,
    mem: Memory,
    swap: (u64, u64),
    net: (u64, u64),
    disk: (u64, u64),
    psi_cpu: f64,
    psi_mem: f64,
    psi_io: f64,
}

fn read_all() -> Raw {
    Raw {
        cpu: cpu_times(&read("/proc/stat")).unwrap_or_default(),
        mem: memory(&read("/proc/meminfo")),
        swap: swap_split(&read("/proc/swaps")),
        net: net_bytes(&read("/proc/net/dev")),
        disk: disk_bytes(&read("/proc/diskstats"), whole_disk),
        psi_cpu: psi("cpu"),
        psi_mem: psi("memory"),
        psi_io: psi("io"),
    }
}

/// One second's worth: rates from the difference with the previous read.
#[derive(Clone, Copy, Debug, Default)]
pub struct Reading {
    pub cpu_pct: f64,
    pub mem: Memory,
    pub swap_zram_kb: u64,
    pub swap_disk_kb: u64,
    pub zram_ram_bytes: u64,
    pub net_in: f64,
    pub net_out: f64,
    pub disk_read: f64,
    pub disk_write: f64,
    pub psi_cpu: f64,
    pub psi_mem: f64,
    pub psi_io: f64,
}

fn rate(now: u64, before: u64, secs: f64) -> f64 {
    now.saturating_sub(before) as f64 / secs.max(0.001)
}

fn reading(now: &Raw, before: &Raw, secs: f64) -> Reading {
    let total = now.cpu.total.saturating_sub(before.cpu.total);
    let busy = now.cpu.busy.saturating_sub(before.cpu.busy);
    Reading {
        cpu_pct: if total == 0 { 0.0 } else { busy as f64 * 100.0 / total as f64 },
        mem: now.mem,
        swap_zram_kb: now.swap.0,
        swap_disk_kb: now.swap.1,
        zram_ram_bytes: 0,
        net_in: rate(now.net.0, before.net.0, secs),
        net_out: rate(now.net.1, before.net.1, secs),
        disk_read: rate(now.disk.0, before.disk.0, secs),
        disk_write: rate(now.disk.1, before.disk.1, secs),
        psi_cpu: now.psi_cpu,
        psi_mem: now.psi_mem,
        psi_io: now.psi_io,
    }
}

impl Reading {
    /// The graphed value.
    pub fn value(&self, m: Metric) -> f64 {
        match m {
            Metric::Cpu => self.cpu_pct,
            Metric::Memory => {
                let t = self.mem.total_kb.max(1) as f64;
                (t - self.mem.available_kb as f64) * 100.0 / t
            }
            Metric::Swap => (self.swap_zram_kb + self.swap_disk_kb) as f64 * 100.0 / self.mem.swap_total_kb.max(1) as f64,
            Metric::Network => self.net_in + self.net_out,
            Metric::Disk => self.disk_read + self.disk_write,
            Metric::Pressure => self.psi_cpu.max(self.psi_mem).max(self.psi_io),
        }
    }

    /// Whether something waited on what this metric measures.
    pub fn stalled(&self, m: Metric) -> bool {
        // avg10 above this is a real wait, below it noise
        const STALL_PCT: f64 = 1.0;
        match m {
            Metric::Cpu => self.psi_cpu >= STALL_PCT,
            Metric::Memory | Metric::Swap => self.psi_mem >= STALL_PCT,
            Metric::Disk => self.psi_io >= STALL_PCT,
            Metric::Network | Metric::Pressure => false,
        }
    }

    /// The number on the icon.
    pub fn label(&self, m: Metric) -> String {
        match m {
            Metric::Cpu => format!("{:.0}%", self.cpu_pct),
            Metric::Memory => gib_short(self.mem.available_kb as f64 * 1024.0),
            Metric::Swap => gib_short((self.swap_zram_kb + self.swap_disk_kb) as f64 * 1024.0),
            Metric::Network | Metric::Disk => bytes_short(self.value(m)),
            Metric::Pressure => format!("{:.0}%", self.value(m)),
        }
    }

    /// Menu lines.
    pub fn status(&self, m: Metric) -> Vec<String> {
        let g = |kb: u64| kb as f64 / 1024.0 / 1024.0;
        let pressure = |what: &str, p: f64| {
            if p < 0.05 { format!("{what} pressure: none, nothing waited") } else { format!("{what} pressure: {p:.1} % of the time something waited") }
        };
        match m {
            Metric::Cpu => vec![format!("CPU {:.0} % busy", self.cpu_pct), pressure("CPU", self.psi_cpu)],
            Metric::Memory => vec![
                format!("{:.1} G available of {:.1} G", g(self.mem.available_kb), g(self.mem.total_kb)),
                format!("{:.1} G of it is cache, given back on demand", g(self.mem.cache_kb)),
                pressure("Memory", self.psi_mem),
            ],
            Metric::Swap => {
                let mut v = vec![format!(
                    "Swap {:.1} G used of {:.1} G",
                    g(self.swap_zram_kb + self.swap_disk_kb),
                    g(self.mem.swap_total_kb)
                )];
                if self.swap_zram_kb > 0 {
                    v.push(format!(
                        "zram {:.1} G, compressed in RAM to {:.1} G",
                        g(self.swap_zram_kb),
                        self.zram_ram_bytes as f64 / 1024f64.powi(3)
                    ));
                }
                if self.swap_disk_kb > 0 {
                    v.push(format!("on disk {:.1} G", g(self.swap_disk_kb)));
                }
                v.push(pressure("Memory", self.psi_mem));
                v
            }
            Metric::Network => vec![format!("In {}/s, out {}/s", bytes_short(self.net_in), bytes_short(self.net_out))],
            Metric::Disk => {
                vec![format!("Read {}/s, write {}/s", bytes_short(self.disk_read), bytes_short(self.disk_write)), pressure("I/O", self.psi_io)]
            }
            Metric::Pressure => vec![pressure("CPU", self.psi_cpu), pressure("Memory", self.psi_mem), pressure("I/O", self.psi_io)],
        }
    }
}

fn gib_short(bytes: f64) -> String {
    let g = bytes / 1024f64.powi(3);
    if g < 9.95 { format!("{g:.1}G") } else { format!("{g:.0}G") }
}

/// 3 significant characters or so: "850", "12k", "1.2M".
pub fn bytes_short(b: f64) -> String {
    let (v, unit) = match b {
        b if b < 1000.0 => return format!("{b:.0}"),
        b if b < 1e6 => (b / 1e3, "k"),
        b if b < 1e9 => (b / 1e6, "M"),
        b => (b / 1e9, "G"),
    };
    if v < 9.95 { format!("{v:.1}{unit}") } else { format!("{v:.0}{unit}") }
}

// ---- history for the icon ----

/// The last few minutes, one reading a second.
pub struct History {
    readings: VecDeque<(i64, Reading)>,
    keep_ms: i64,
    previous: Option<(std::time::Instant, Raw)>,
}

impl History {
    pub fn new(keep_ms: i64) -> History {
        History { readings: VecDeque::new(), keep_ms, previous: None }
    }

    /// Reads /proc; the first call only sets the starting point.
    pub fn sample(&mut self, now_ms: i64) -> Option<Reading> {
        let raw = read_all();
        let at = std::time::Instant::now();
        let before = self.previous.replace((at, raw));
        let (t0, prev) = before?;
        let mut r = reading(&raw, &prev, at.duration_since(t0).as_secs_f64());
        r.zram_ram_bytes = if raw.swap.0 > 0 { zram_ram_bytes() } else { 0 };
        self.push(now_ms, r);
        Some(r)
    }

    fn push(&mut self, now_ms: i64, r: Reading) {
        self.readings.push_back((now_ms, r));
        while self.readings.front().is_some_and(|(t, _)| *t < now_ms - self.keep_ms) {
            self.readings.pop_front();
        }
    }

    /// Columns of `column_ms` ending now, with whether something stalled in each.
    pub fn columns(&self, m: Metric, now_ms: i64, n: usize, column_ms: i64) -> Vec<(Column, bool)> {
        let end = (now_ms / column_ms + 1) * column_ms;
        let start = end - n as i64 * column_ms;
        let mut cols: Vec<(Vec<Option<f64>>, bool)> = vec![(Vec::new(), false); n];
        for (t, r) in &self.readings {
            if *t < start || *t >= end {
                continue;
            }
            let c = &mut cols[((t - start) / column_ms) as usize];
            c.0.push(Some(r.value(m)));
            c.1 |= r.stalled(m);
        }
        cols.into_iter().map(|(v, s)| (Column::from_samples(&v), s)).collect()
    }

    pub fn ceiling(&self, m: Metric, columns: &[(Column, bool)]) -> f64 {
        if let Some(c) = m.fixed_ceiling() {
            return c;
        }
        let max = columns.iter().filter_map(|(c, _)| c.pcts.map(|p| p[4])).fold(0.0, f64::max);
        let floor = match m {
            Metric::Pressure => 10.0,
            // a quiet link or disk still has some scale
            Metric::Network | Metric::Disk => 100e3,
            _ => 1.0,
        };
        nice_ceiling(max.max(floor))
    }
}

/// 1, 2 or 5 times a power of ten, at or above `v`.
fn nice_ceiling(v: f64) -> f64 {
    let p = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 5.0, 10.0].into_iter().map(|k| k * p).find(|c| *c >= v).unwrap_or(10.0 * p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_is_a_delta() {
        let a = cpu_times("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3").unwrap();
        let b = cpu_times("cpu  150 0 100 850 50 0 0 0 0 0\n").unwrap();
        let r = reading(&Raw { cpu: b, ..Default::default() }, &Raw { cpu: a, ..Default::default() }, 1.0);
        assert!((r.cpu_pct - 100.0 * 100.0 / 150.0).abs() < 1e-9);
    }

    #[test]
    fn memory_counts_cache_as_available() {
        let m = memory("MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 700 kB\nBuffers: 50 kB\nCached: 500 kB\nSReclaimable: 20 kB\nSwapCached: 9 kB\n");
        assert_eq!((m.total_kb, m.available_kb, m.cache_kb), (1000, 700, 570));
        let r = Reading { mem: m, ..Default::default() };
        assert!((r.value(Metric::Memory) - 30.0).abs() < 1e-9);
    }

    #[test]
    fn swap_zram_apart() {
        let s = "Filename Type Size Used Priority\n/dev/zram0 partition 100 40 100\n/swap.img file 200 5 -2\n";
        assert_eq!(swap_split(s), (40, 5));
    }

    #[test]
    fn psi_and_interfaces() {
        assert_eq!(psi_some("some avg10=1.25 avg60=0.00 avg300=0.00 total=1\nfull avg10=0.50"), Some(1.25));
        let dev = "h1\nh2\n    lo: 9 0 0 0 0 0 0 0 9 0\n  wlp0: 100 1 0 0 0 0 0 0 40 1\nveth1: 7 0 0 0 0 0 0 0 7 0\n";
        assert_eq!(net_bytes(dev), (100, 40));
        let ds = "259 0 nvme0n1 1 0 10 0 1 0 20 0 0 0 0\n259 1 nvme0n1p1 1 0 10 0 1 0 20 0 0 0 0\n";
        assert_eq!(disk_bytes(ds, |n| n == "nvme0n1"), (10 * 512, 20 * 512));
    }

    #[test]
    fn columns_and_scale() {
        let mut h = History::new(300_000);
        for s in 0..30 {
            h.push(s * 1000, Reading { cpu_pct: s as f64, psi_cpu: if s == 29 { 3.0 } else { 0.0 }, ..Default::default() });
        }
        let cols = h.columns(Metric::Cpu, 29_000, 2, 15_000);
        assert_eq!(cols[0].0.count, 15);
        assert!(cols[1].1 && !cols[0].1);
        assert_eq!(nice_ceiling(130e3), 200e3);
        assert_eq!(bytes_short(1_234_567.0), "1.2M");
    }
}
