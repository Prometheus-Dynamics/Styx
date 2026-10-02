//! CPU, memory and dma-buf use of this process and its descendants (libcamera's IPA proxy),
//! from `/proc`.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// `utime + stime` of a process, in clock ticks, and its name.
fn cpu_ticks(pid: u32) -> Option<(String, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (head, rest) = stat.rsplit_once(')')?;
    let name = head.split_once('(')?.1.to_string();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // Fields after the name: state(0) ppid(1) ... utime(11) stime(12).
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((name, utime + stime))
}

fn parent(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// This process and every live descendant.
pub fn process_tree() -> Vec<u32> {
    let me = std::process::id();
    let mut tree = vec![me];
    let all: Vec<(u32, u32)> = fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| Some((pid, parent(pid)?)))
        .collect();
    let mut i = 0;
    while i < tree.len() {
        let p = tree[i];
        tree.extend(all.iter().filter(|(_, pp)| *pp == p).map(|(pid, _)| *pid));
        i += 1;
    }
    tree
}

/// `USER_HZ`: the unit of `/proc/<pid>/stat` times, 100 on every Linux architecture.
const CLOCK_TICKS_PER_SECOND: f64 = 100.0;

/// CPU time of the process tree at one instant.
pub struct CpuSnapshot {
    at: Instant,
    ticks: BTreeMap<u32, (String, u64)>,
}

impl CpuSnapshot {
    pub fn take() -> Self {
        Self {
            at: Instant::now(),
            ticks: process_tree()
                .into_iter()
                .filter_map(|pid| Some((pid, cpu_ticks(pid)?)))
                .collect(),
        }
    }

    /// CPU use between `self` and `later`, per process, in percent of one core. Processes
    /// started in between count from zero.
    pub fn until(&self, later: &CpuSnapshot) -> CpuStats {
        let wall = later.at.duration_since(self.at).as_secs_f64().max(1e-9);
        let hz = CLOCK_TICKS_PER_SECOND;
        let me = std::process::id();
        let processes: Vec<ProcessCpu> = later
            .ticks
            .iter()
            .map(|(pid, (name, t1))| {
                let t0 = self.ticks.get(pid).map_or(0, |(_, t)| *t);
                ProcessCpu {
                    pid: *pid,
                    name: name.clone(),
                    percent: t1.saturating_sub(t0) as f64 / hz / wall * 100.0,
                }
            })
            .collect();
        let own: f64 = processes
            .iter()
            .filter(|p| p.pid == me)
            .map(|p| p.percent)
            .sum();
        let total: f64 = processes.iter().map(|p| p.percent).sum();
        CpuStats {
            window_s: wall,
            process_percent: own,
            children_percent: total - own,
            total_percent: total,
            processes,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProcessCpu {
    pub pid: u32,
    pub name: String,
    pub percent: f64,
}

/// CPU use in percent of one core over a measurement window.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CpuStats {
    pub window_s: f64,
    /// This process.
    pub process_percent: f64,
    /// Its descendants (libcamera's `raspberrypi_ipa_proxy`).
    pub children_percent: f64,
    pub total_percent: f64,
    pub processes: Vec<ProcessCpu>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProcessMemory {
    pub pid: u32,
    pub name: String,
    pub rss_kib: u64,
    pub pss_kib: u64,
    /// dma-bufs this process holds file descriptors for.
    pub dmabuf_count: usize,
    pub dmabuf_kib: u64,
}

/// Memory of the process tree while streaming.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryStats {
    pub rss_kib: u64,
    pub pss_kib: u64,
    /// Distinct dma-bufs held by any process of the tree (by inode), and their size.
    pub dmabuf_count: usize,
    pub dmabuf_kib: u64,
    /// dma-bufs by exporter (`system`, `cma`, `videobuf2`...), KiB.
    pub dmabuf_by_exporter: BTreeMap<String, u64>,
    /// Change of the system-wide dma-buf total (`/sys/kernel/dmabuf/buffers`) since the
    /// baseline, KiB; includes buffers the kernel holds for the pipeline (`None` when the kernel
    /// does not export the statistics).
    pub system_dmabuf_delta_kib: Option<i64>,
    pub processes: Vec<ProcessMemory>,
}

/// `Pss:` and `Rss:` from `smaps_rollup`, KiB.
fn rss_pss(pid: u32) -> (u64, u64) {
    let text = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap_or_default();
    let field = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
            .unwrap_or(0)
    };
    (field("Rss:"), field("Pss:"))
}

/// dma-bufs a process holds fds for: `(inode, exporter, bytes)`.
fn dmabufs(pid: u32) -> Vec<(u64, String, u64)> {
    let Ok(dir) = fs::read_dir(format!("/proc/{pid}/fdinfo")) else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| {
            let text = fs::read_to_string(e.path()).ok()?;
            let get = |k: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()))
            };
            let exporter = get("exp_name:")?;
            let size = get("size:")?.parse().ok()?;
            let ino = get("ino:").and_then(|v| v.parse().ok()).unwrap_or(0);
            Some((ino, exporter, size))
        })
        .collect()
}

/// Total size of every dma-buf in the system, bytes (`None` without dma-buf sysfs stats).
pub fn system_dmabuf_bytes() -> Option<u64> {
    let dir = fs::read_dir("/sys/kernel/dmabuf/buffers").ok()?;
    Some(
        dir.flatten()
            .filter_map(|e| {
                fs::read_to_string(e.path().join("size"))
                    .ok()?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .sum(),
    )
}

/// Memory of this process tree now; `system_baseline` is [`system_dmabuf_bytes`] before the
/// capture opened.
pub fn memory_stats(system_baseline: Option<u64>) -> MemoryStats {
    let mut stats = MemoryStats::default();
    let mut seen = HashSet::new();
    for pid in process_tree() {
        let name = cpu_ticks(pid).map(|(n, _)| n).unwrap_or_default();
        let (rss, pss) = rss_pss(pid);
        let bufs = dmabufs(pid);
        let mut own = ProcessMemory {
            pid,
            name,
            rss_kib: rss,
            pss_kib: pss,
            ..ProcessMemory::default()
        };
        for (ino, exporter, bytes) in bufs {
            own.dmabuf_count += 1;
            own.dmabuf_kib += bytes / 1024;
            if ino == 0 || seen.insert(ino) {
                stats.dmabuf_count += 1;
                stats.dmabuf_kib += bytes / 1024;
                *stats.dmabuf_by_exporter.entry(exporter).or_default() += bytes / 1024;
            }
        }
        stats.rss_kib += rss;
        stats.pss_kib += pss;
        stats.processes.push(own);
    }
    stats.system_dmabuf_delta_kib = match (system_baseline, system_dmabuf_bytes()) {
        (Some(before), Some(now)) => Some((now as i64 - before as i64) / 1024),
        _ => None,
    };
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_measured() {
        if !std::path::Path::new("/proc/self/stat").exists() {
            return;
        }
        let a = CpuSnapshot::take();
        let mut x = 0u64;
        for i in 0..2_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        let cpu = a.until(&CpuSnapshot::take());
        assert!(cpu.total_percent >= 0.0);
        assert!(cpu.processes.iter().any(|p| p.pid == std::process::id()));
        let mem = memory_stats(None);
        assert!(mem.rss_kib > 0);
        assert!(process_tree().contains(&std::process::id()));
    }
}
