//! Kernel dma-buf and CMA accounting.

#[allow(unused_imports)]
use super::*;

pub(super) fn collect_kernel_dmabuf_stats() -> KernelDmabufStats {
    #[cfg(target_os = "linux")]
    {
        let cma = collect_cma_stats();
        let debugfs = std::path::Path::new("/sys/kernel/debug");
        let probes = [
            "/sys/kernel/debug/dma_buf/bufinfo",
            "/sys/kernel/debug/dma_heap",
        ];
        let probed_paths = probes.iter().map(|path| (*path).to_string()).collect();

        if !debugfs.exists() {
            return kernel_dmabuf_unavailable(
                "debugfs is unavailable or not mounted",
                probed_paths,
                cma,
            );
        }

        let bufinfo = std::path::Path::new(probes[0]);
        match std::fs::read_to_string(bufinfo) {
            Ok(contents) => {
                let parsed = parse_dma_bufinfo(&contents);
                return KernelDmabufStats {
                    available: true,
                    unavailable_reason: None,
                    probed_paths,
                    total_buffers: Some(parsed.total_buffers),
                    total_bytes: Some(parsed.total_bytes),
                    exporters: parsed.exporters,
                    cma_total_bytes: cma.total_bytes,
                    cma_free_bytes: cma.free_bytes,
                    cma_used_bytes: cma.used_bytes,
                };
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                return kernel_dmabuf_unavailable(
                    "permission denied reading debugfs dma_buf bufinfo",
                    probed_paths,
                    cma,
                );
            }
            Err(_) => {}
        }

        let dma_heap = std::path::Path::new(probes[1]);
        match std::fs::read_dir(dma_heap) {
            Ok(entries) => {
                let mut entries = entries;
                if entries.next().is_some() {
                    KernelDmabufStats {
                        available: true,
                        unavailable_reason: None,
                        probed_paths,
                        total_buffers: None,
                        total_bytes: None,
                        exporters: Vec::new(),
                        cma_total_bytes: cma.total_bytes,
                        cma_free_bytes: cma.free_bytes,
                        cma_used_bytes: cma.used_bytes,
                    }
                } else {
                    kernel_dmabuf_unavailable(
                        "kernel dma-buf debugfs telemetry is unavailable; kernel support may be missing",
                        probed_paths,
                        cma,
                    )
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                kernel_dmabuf_unavailable(
                    "permission denied reading debugfs dma_heap",
                    probed_paths,
                    cma,
                )
            }
            _ => kernel_dmabuf_unavailable(
                "kernel dma-buf debugfs telemetry is unavailable; kernel support may be missing",
                probed_paths,
                cma,
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        KernelDmabufStats {
            available: false,
            unavailable_reason: Some("kernel dma-buf telemetry is only supported on linux".into()),
            probed_paths: Vec::new(),
            total_buffers: None,
            total_bytes: None,
            exporters: Vec::new(),
            cma_total_bytes: None,
            cma_free_bytes: None,
            cma_used_bytes: None,
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn kernel_dmabuf_unavailable(
    reason: &str,
    probed_paths: Vec<String>,
    cma: CmaStats,
) -> KernelDmabufStats {
    KernelDmabufStats {
        available: false,
        unavailable_reason: Some(reason.to_string()),
        probed_paths,
        total_buffers: None,
        total_bytes: None,
        exporters: Vec::new(),
        cma_total_bytes: cma.total_bytes,
        cma_free_bytes: cma.free_bytes,
        cma_used_bytes: cma.used_bytes,
    }
}

#[derive(Default)]
pub(super) struct DmaBufinfoStats {
    pub(super) total_buffers: u64,
    pub(super) total_bytes: u64,
    pub(super) exporters: Vec<KernelDmabufExporterStats>,
}

#[cfg(target_os = "linux")]
pub(super) fn parse_dma_bufinfo(contents: &str) -> DmaBufinfoStats {
    let mut total_buffers = 0u64;
    let mut total_bytes = 0u64;
    let mut by_exporter: HashMap<String, KernelDmabufExporterStats> = HashMap::new();

    for line in contents.lines() {
        let columns = line.split_whitespace().collect::<Vec<_>>();
        if columns.len() < 5 {
            continue;
        }
        let Some(bytes) = parse_dma_bufinfo_size(columns[0]) else {
            continue;
        };
        let exporter = columns[4].to_string();
        if exporter.eq_ignore_ascii_case("exp_name") {
            continue;
        }
        total_buffers = total_buffers.saturating_add(1);
        total_bytes = total_bytes.saturating_add(bytes);
        let entry =
            by_exporter
                .entry(exporter.clone())
                .or_insert_with(|| KernelDmabufExporterStats {
                    exporter,
                    buffers: 0,
                    bytes: 0,
                });
        entry.buffers = entry.buffers.saturating_add(1);
        entry.bytes = entry.bytes.saturating_add(bytes);
    }

    let mut exporters = by_exporter.into_values().collect::<Vec<_>>();
    exporters.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.exporter.cmp(&b.exporter))
    });
    DmaBufinfoStats {
        total_buffers,
        total_bytes,
        exporters,
    }
}

pub(super) fn parse_dma_bufinfo_size(value: &str) -> Option<u64> {
    if value.eq_ignore_ascii_case("size") || value.eq_ignore_ascii_case("total") {
        return None;
    }
    // The kernel prints sizes as zero-padded decimal (`%08zu`): "01536000" is 1.5 MB.
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => value.parse::<u64>().ok(),
    }
}

#[derive(Default)]
pub(super) struct CmaStats {
    pub(super) total_bytes: Option<u64>,
    pub(super) free_bytes: Option<u64>,
    pub(super) used_bytes: Option<u64>,
}

#[cfg(target_os = "linux")]
pub(super) fn collect_cma_stats() -> CmaStats {
    let Ok(contents) = std::fs::read_to_string("/proc/meminfo") else {
        return CmaStats::default();
    };
    parse_cma_stats(&contents)
}

#[cfg(target_os = "linux")]
pub(super) fn parse_cma_stats(contents: &str) -> CmaStats {
    let mut total_bytes = None;
    let mut free_bytes = None;
    for line in contents.lines() {
        let Some((key, bytes)) = parse_kib_line(line) else {
            continue;
        };
        match key {
            "CmaTotal" => total_bytes = Some(bytes),
            "CmaFree" => free_bytes = Some(bytes),
            _ => {}
        }
    }
    let used_bytes = total_bytes
        .zip(free_bytes)
        .map(|(total, free)| total.saturating_sub(free));
    CmaStats {
        total_bytes,
        free_bytes,
        used_bytes,
    }
}
