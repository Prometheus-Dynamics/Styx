use std::collections::HashMap;
use std::fmt;

use crate::metrics::{GraphTelemetryStats, HealthReport, PipelineMemoryStats};

#[derive(Clone, Debug, Default)]
pub struct RuntimeMemoryReport {
    pub process: ProcessMemoryStats,
    pub mappings: Vec<MappingCategoryStats>,
    pub fds: FdInventoryStats,
    pub kernel_dmabuf: KernelDmabufStats,
    pub styx: Option<PipelineMemoryStats>,
    pub health: Option<HealthReport>,
    pub graph: Option<GraphTelemetryStats>,
    pub unexplained_pss_bytes: Option<u64>,
    pub warnings: Vec<String>,
}

impl RuntimeMemoryReport {
    pub fn to_compact_string(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for RuntimeMemoryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Process:")?;
        if self.process.available {
            writeln!(f, "  PSS: {}", format_bytes_opt(self.process.pss_bytes))?;
            writeln!(f, "  RSS: {}", format_bytes_opt(self.process.rss_bytes))?;
            writeln!(
                f,
                "  Private: {}",
                format_bytes_opt(sum_opts(
                    self.process.private_clean_bytes,
                    self.process.private_dirty_bytes,
                ))
            )?;
            writeln!(
                f,
                "  Shared: {}",
                format_bytes_opt(sum_opts(
                    self.process.shared_clean_bytes,
                    self.process.shared_dirty_bytes,
                ))
            )?;
        } else {
            writeln!(
                f,
                "  unavailable: {}",
                self.process
                    .unavailable_reason
                    .as_deref()
                    .unwrap_or("unknown reason")
            )?;
        }

        writeln!(f)?;
        writeln!(f, "Styx tracked:")?;
        if let Some(styx) = &self.styx {
            for backing in &styx.external_backings {
                writeln!(
                    f,
                    "  {}: {} buffers / {}",
                    backing.label,
                    backing.current_buffers,
                    format_bytes(backing.current_bytes)
                )?;
            }
            if let Some(pool) = &styx.transform_pool {
                writeln!(
                    f,
                    "  transform pool: retained {} / in-use {}",
                    format_bytes(pool.retained_bytes as u64),
                    format_bytes(pool.in_use_bytes as u64)
                )?;
            }
            #[cfg(target_os = "linux")]
            {
                if let Some(pool) = &styx.shared_decode_pool {
                    writeln!(
                        f,
                        "  shared decode pool: {} free / chunk {}",
                        format_bytes(pool.free_bytes as u64),
                        format_bytes(pool.chunk_size as u64)
                    )?;
                }
                if let Some(pool) = &styx.shared_encode_pool {
                    writeln!(
                        f,
                        "  shared encode pool: {} free / chunk {}",
                        format_bytes(pool.free_bytes as u64),
                        format_bytes(pool.chunk_size as u64)
                    )?;
                }
            }
        } else {
            writeln!(f, "  no pipeline memory stats attached")?;
        }
        if let Some(health) = &self.health {
            writeln!(
                f,
                "  copies: {} / bytes moved {}",
                health.copy_count,
                format_bytes(health.bytes_moved)
            )?;
            writeln!(
                f,
                "  residency transitions: {} recent",
                health.recent_residency_transitions.len()
            )?;
        }

        writeln!(f)?;
        writeln!(f, "Smaps:")?;
        if self.mappings.is_empty() {
            writeln!(f, "  unavailable or empty")?;
        } else {
            for mapping in self.mappings.iter().take(8) {
                writeln!(
                    f,
                    "  {}: {} PSS / {} RSS / {} mappings",
                    mapping.category,
                    format_bytes(mapping.pss_bytes),
                    format_bytes(mapping.rss_bytes),
                    mapping.mappings
                )?;
            }
        }

        writeln!(f)?;
        writeln!(f, "FDs:")?;
        if self.fds.available {
            writeln!(f, "  total: {}", self.fds.total)?;
            for class in self.fds.classes.iter().take(8) {
                writeln!(f, "  {}: {}", class.class, class.count)?;
            }
        } else {
            writeln!(
                f,
                "  unavailable: {}",
                self.fds
                    .unavailable_reason
                    .as_deref()
                    .unwrap_or("unknown reason")
            )?;
        }

        writeln!(f)?;
        writeln!(
            f,
            "Kernel DMA-BUF: {}",
            if self.kernel_dmabuf.available {
                format!(
                    "available ({} buffers / {})",
                    self.kernel_dmabuf.total_buffers.unwrap_or(0),
                    format_bytes_opt(self.kernel_dmabuf.total_bytes)
                )
            } else {
                self.kernel_dmabuf
                    .unavailable_reason
                    .clone()
                    .unwrap_or_else(|| "unavailable".to_string())
            }
        )?;
        if self.kernel_dmabuf.cma_total_bytes.is_some()
            || self.kernel_dmabuf.cma_free_bytes.is_some()
        {
            writeln!(
                f,
                "  CMA: used {} / total {} / free {}",
                format_bytes_opt(self.kernel_dmabuf.cma_used_bytes),
                format_bytes_opt(self.kernel_dmabuf.cma_total_bytes),
                format_bytes_opt(self.kernel_dmabuf.cma_free_bytes)
            )?;
        }
        for exporter in self.kernel_dmabuf.exporters.iter().take(5) {
            writeln!(
                f,
                "  exporter {}: {} buffers / {}",
                exporter.exporter,
                exporter.buffers,
                format_bytes(exporter.bytes)
            )?;
        }
        writeln!(
            f,
            "Unexplained PSS: {}",
            format_bytes_opt(self.unexplained_pss_bytes)
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessMemoryStats {
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub rss_bytes: Option<u64>,
    pub pss_bytes: Option<u64>,
    pub shared_clean_bytes: Option<u64>,
    pub shared_dirty_bytes: Option<u64>,
    pub private_clean_bytes: Option<u64>,
    pub private_dirty_bytes: Option<u64>,
    pub swap_bytes: Option<u64>,
    pub swap_pss_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MappingCategory {
    Heap,
    Stack,
    SharedLibraries,
    Anonymous,
    Memfd,
    PispMemfd,
    LibcameraOrIpa,
    DmabufOrDmaHeap,
    DeviceMapping,
    MmapFile,
    Unknown,
}

impl MappingCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Heap => "heap",
            Self::Stack => "stack",
            Self::SharedLibraries => "shared_libraries",
            Self::Anonymous => "anonymous",
            Self::Memfd => "memfd",
            Self::PispMemfd => "pisp_memfd",
            Self::LibcameraOrIpa => "libcamera_or_ipa",
            Self::DmabufOrDmaHeap => "dmabuf_or_dma_heap",
            Self::DeviceMapping => "device_mapping",
            Self::MmapFile => "mmap_file",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for MappingCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MappingCategoryStats {
    pub category: String,
    pub mappings: u64,
    pub rss_bytes: u64,
    pub pss_bytes: u64,
    pub private_bytes: u64,
    pub shared_bytes: u64,
    pub top_mappings: Vec<MappingNameStats>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MappingNameStats {
    pub name: String,
    pub pss_bytes: u64,
    pub rss_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FdClass {
    RegularFile,
    Socket,
    Pipe,
    EventFd,
    TimerFd,
    Epoll,
    AnonInode,
    Memfd,
    PispMemfd,
    DmaBufOrDmaHeap,
    MediaOrVideoDevice,
    Unknown,
}

impl FdClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RegularFile => "regular_file",
            Self::Socket => "socket",
            Self::Pipe => "pipe",
            Self::EventFd => "eventfd",
            Self::TimerFd => "timerfd",
            Self::Epoll => "epoll",
            Self::AnonInode => "anon_inode",
            Self::Memfd => "memfd",
            Self::PispMemfd => "pisp_memfd",
            Self::DmaBufOrDmaHeap => "dmabuf_or_dma_heap",
            Self::MediaOrVideoDevice => "media_or_video_device",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for FdClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FdInventoryStats {
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub total: u64,
    pub classes: Vec<FdClassStats>,
    pub top_targets: Vec<FdTargetStats>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FdClassStats {
    pub class: String,
    pub count: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FdTargetStats {
    pub target: String,
    pub count: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KernelDmabufStats {
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub probed_paths: Vec<String>,
    pub total_buffers: Option<u64>,
    pub total_bytes: Option<u64>,
    pub exporters: Vec<KernelDmabufExporterStats>,
    pub cma_total_bytes: Option<u64>,
    pub cma_free_bytes: Option<u64>,
    pub cma_used_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KernelDmabufExporterStats {
    pub exporter: String,
    pub buffers: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default)]
struct SmapsEntry {
    name: String,
    rss_bytes: u64,
    pss_bytes: u64,
    shared_clean_bytes: u64,
    shared_dirty_bytes: u64,
    private_clean_bytes: u64,
    private_dirty_bytes: u64,
}

#[derive(Default)]
struct MappingAccumulator {
    mappings: u64,
    rss_bytes: u64,
    pss_bytes: u64,
    private_bytes: u64,
    shared_bytes: u64,
    by_name: HashMap<String, MappingNameStats>,
}

pub fn runtime_memory_report() -> RuntimeMemoryReport {
    runtime_memory_report_parts(None, None, None)
}

pub fn runtime_memory_report_with_styx(
    styx: PipelineMemoryStats,
    health: Option<HealthReport>,
    graph: Option<GraphTelemetryStats>,
) -> RuntimeMemoryReport {
    runtime_memory_report_parts(Some(styx), health, graph)
}

pub(crate) fn runtime_memory_report_parts(
    styx: Option<PipelineMemoryStats>,
    health: Option<HealthReport>,
    graph: Option<GraphTelemetryStats>,
) -> RuntimeMemoryReport {
    let mut warnings = Vec::new();
    let process = collect_process_memory(&mut warnings);
    let mappings = collect_mapping_stats(&mut warnings);
    let fds = collect_fd_inventory(&mut warnings);
    let kernel_dmabuf = collect_kernel_dmabuf_stats();
    let unexplained_pss_bytes = process
        .pss_bytes
        .map(|pss| pss.saturating_sub(known_memory_bytes(styx.as_ref(), graph.as_ref())));

    RuntimeMemoryReport {
        process,
        mappings,
        fds,
        kernel_dmabuf,
        styx,
        health,
        graph,
        unexplained_pss_bytes,
        warnings,
    }
}

fn known_memory_bytes(
    styx: Option<&PipelineMemoryStats>,
    graph: Option<&GraphTelemetryStats>,
) -> u64 {
    let mut known = 0u64;
    if let Some(styx) = styx {
        known = known.saturating_add(
            styx.external_backings
                .iter()
                .map(|stats| stats.current_bytes)
                .sum::<u64>(),
        );
        if let Some(pool) = &styx.transform_pool {
            known = known.saturating_add(pool.retained_bytes as u64);
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(pool) = &styx.shared_decode_pool {
                known = known.saturating_add(pool.free_bytes as u64);
            }
            if let Some(pool) = &styx.shared_encode_pool {
                known = known.saturating_add(pool.free_bytes as u64);
            }
        }
    }
    if let Some(graph) = graph {
        known = known.saturating_add(graph.copied_bytes);
        known = known.saturating_add(graph.transport_bytes);
    }
    known
}

fn sum_opts(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))).filter(|sum| *sum > 0)
}

fn format_bytes_opt(bytes: Option<u64>) -> String {
    bytes
        .map(format_bytes)
        .unwrap_or_else(|| "unavailable".to_string())
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(target_os = "linux")]
fn collect_process_memory(warnings: &mut Vec<String>) -> ProcessMemoryStats {
    match std::fs::read_to_string("/proc/self/smaps_rollup") {
        Ok(contents) => parse_smaps_rollup(&contents),
        Err(err) => {
            let reason = err.to_string();
            warnings.push(format!("smaps_rollup unavailable: {reason}"));
            ProcessMemoryStats {
                available: false,
                unavailable_reason: Some(reason),
                ..ProcessMemoryStats::default()
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn collect_process_memory(warnings: &mut Vec<String>) -> ProcessMemoryStats {
    let reason = "process memory telemetry is only supported on linux".to_string();
    warnings.push(reason.clone());
    ProcessMemoryStats {
        available: false,
        unavailable_reason: Some(reason),
        ..ProcessMemoryStats::default()
    }
}

#[cfg(target_os = "linux")]
fn collect_mapping_stats(warnings: &mut Vec<String>) -> Vec<MappingCategoryStats> {
    match std::fs::read_to_string("/proc/self/smaps") {
        Ok(contents) => mapping_category_stats(parse_smaps(&contents)),
        Err(err) => {
            warnings.push(format!("smaps unavailable: {err}"));
            Vec::new()
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn collect_mapping_stats(_warnings: &mut Vec<String>) -> Vec<MappingCategoryStats> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn collect_fd_inventory(warnings: &mut Vec<String>) -> FdInventoryStats {
    let entries = match std::fs::read_dir("/proc/self/fd") {
        Ok(entries) => entries,
        Err(err) => {
            let reason = err.to_string();
            warnings.push(format!("fd inventory unavailable: {reason}"));
            return FdInventoryStats {
                available: false,
                unavailable_reason: Some(reason),
                ..FdInventoryStats::default()
            };
        }
    };

    let mut class_counts: HashMap<FdClass, u64> = HashMap::new();
    let mut target_counts: HashMap<String, u64> = HashMap::new();
    let mut total = 0u64;

    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let target = target.to_string_lossy().to_string();
        let class = classify_fd_target(&target);
        *class_counts.entry(class).or_default() += 1;
        *target_counts.entry(target).or_default() += 1;
        total += 1;
    }

    let mut classes = class_counts
        .into_iter()
        .map(|(class, count)| FdClassStats {
            class: class.to_string(),
            count,
        })
        .collect::<Vec<_>>();
    classes.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.class.cmp(&b.class)));

    let mut top_targets = target_counts
        .into_iter()
        .map(|(target, count)| FdTargetStats { target, count })
        .collect::<Vec<_>>();
    top_targets.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.target.cmp(&b.target)));
    top_targets.truncate(16);

    FdInventoryStats {
        available: true,
        unavailable_reason: None,
        total,
        classes,
        top_targets,
    }
}

#[cfg(not(target_os = "linux"))]
fn collect_fd_inventory(warnings: &mut Vec<String>) -> FdInventoryStats {
    let reason = "fd inventory telemetry is only supported on linux".to_string();
    warnings.push(reason.clone());
    FdInventoryStats {
        available: false,
        unavailable_reason: Some(reason),
        ..FdInventoryStats::default()
    }
}

fn collect_kernel_dmabuf_stats() -> KernelDmabufStats {
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
fn kernel_dmabuf_unavailable(
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
struct DmaBufinfoStats {
    total_buffers: u64,
    total_bytes: u64,
    exporters: Vec<KernelDmabufExporterStats>,
}

#[cfg(target_os = "linux")]
fn parse_dma_bufinfo(contents: &str) -> DmaBufinfoStats {
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

#[cfg(target_os = "linux")]
fn parse_dma_bufinfo_size(value: &str) -> Option<u64> {
    if value.eq_ignore_ascii_case("size") || value.eq_ignore_ascii_case("total") {
        return None;
    }
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .or_else(|| {
            if value.len() > 1 && value.starts_with('0') {
                u64::from_str_radix(value, 16).ok()
            } else if value
                .chars()
                .any(|ch| ch.is_ascii_hexdigit() && ch.is_ascii_alphabetic())
            {
                u64::from_str_radix(value, 16).ok()
            } else {
                value.parse::<u64>().ok()
            }
        })
}

#[derive(Default)]
struct CmaStats {
    total_bytes: Option<u64>,
    free_bytes: Option<u64>,
    used_bytes: Option<u64>,
}

#[cfg(target_os = "linux")]
fn collect_cma_stats() -> CmaStats {
    let Ok(contents) = std::fs::read_to_string("/proc/meminfo") else {
        return CmaStats::default();
    };
    parse_cma_stats(&contents)
}

#[cfg(target_os = "linux")]
fn parse_cma_stats(contents: &str) -> CmaStats {
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

fn parse_smaps_rollup(contents: &str) -> ProcessMemoryStats {
    let mut stats = ProcessMemoryStats {
        available: true,
        ..ProcessMemoryStats::default()
    };
    for line in contents.lines() {
        let Some((key, bytes)) = parse_kib_line(line) else {
            continue;
        };
        match key {
            "Rss" => stats.rss_bytes = Some(bytes),
            "Pss" => stats.pss_bytes = Some(bytes),
            "Shared_Clean" => stats.shared_clean_bytes = Some(bytes),
            "Shared_Dirty" => stats.shared_dirty_bytes = Some(bytes),
            "Private_Clean" => stats.private_clean_bytes = Some(bytes),
            "Private_Dirty" => stats.private_dirty_bytes = Some(bytes),
            "Swap" => stats.swap_bytes = Some(bytes),
            "SwapPss" => stats.swap_pss_bytes = Some(bytes),
            _ => {}
        }
    }
    stats
}

fn parse_smaps(contents: &str) -> Vec<SmapsEntry> {
    let mut entries = Vec::new();
    let mut current: Option<SmapsEntry> = None;
    for line in contents.lines() {
        if is_smaps_header(line) {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(SmapsEntry {
                name: smaps_header_name(line),
                ..SmapsEntry::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        let Some((key, bytes)) = parse_kib_line(line) else {
            continue;
        };
        match key {
            "Rss" => entry.rss_bytes = bytes,
            "Pss" => entry.pss_bytes = bytes,
            "Shared_Clean" => entry.shared_clean_bytes = bytes,
            "Shared_Dirty" => entry.shared_dirty_bytes = bytes,
            "Private_Clean" => entry.private_clean_bytes = bytes,
            "Private_Dirty" => entry.private_dirty_bytes = bytes,
            _ => {}
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

fn mapping_category_stats(entries: Vec<SmapsEntry>) -> Vec<MappingCategoryStats> {
    let mut grouped: HashMap<MappingCategory, MappingAccumulator> = HashMap::new();
    for entry in entries {
        let category = classify_mapping_name(&entry.name);
        let acc = grouped.entry(category).or_default();
        acc.mappings += 1;
        acc.rss_bytes = acc.rss_bytes.saturating_add(entry.rss_bytes);
        acc.pss_bytes = acc.pss_bytes.saturating_add(entry.pss_bytes);
        acc.private_bytes = acc
            .private_bytes
            .saturating_add(entry.private_clean_bytes)
            .saturating_add(entry.private_dirty_bytes);
        acc.shared_bytes = acc
            .shared_bytes
            .saturating_add(entry.shared_clean_bytes)
            .saturating_add(entry.shared_dirty_bytes);
        let name = if entry.name.is_empty() {
            "[anonymous]".to_string()
        } else {
            entry.name
        };
        let name_stats = acc.by_name.entry(name.clone()).or_insert(MappingNameStats {
            name,
            pss_bytes: 0,
            rss_bytes: 0,
        });
        name_stats.pss_bytes = name_stats.pss_bytes.saturating_add(entry.pss_bytes);
        name_stats.rss_bytes = name_stats.rss_bytes.saturating_add(entry.rss_bytes);
    }

    let mut stats = grouped
        .into_iter()
        .map(|(category, acc)| {
            let mut top_mappings = acc.by_name.into_values().collect::<Vec<_>>();
            top_mappings.sort_by(|a, b| {
                b.pss_bytes
                    .cmp(&a.pss_bytes)
                    .then_with(|| a.name.cmp(&b.name))
            });
            top_mappings.truncate(8);
            MappingCategoryStats {
                category: category.to_string(),
                mappings: acc.mappings,
                rss_bytes: acc.rss_bytes,
                pss_bytes: acc.pss_bytes,
                private_bytes: acc.private_bytes,
                shared_bytes: acc.shared_bytes,
                top_mappings,
            }
        })
        .collect::<Vec<_>>();
    stats.sort_by(|a, b| {
        b.pss_bytes
            .cmp(&a.pss_bytes)
            .then_with(|| a.category.cmp(&b.category))
    });
    stats
}

fn parse_kib_line(line: &str) -> Option<(&str, u64)> {
    let (key, rest) = line.split_once(':')?;
    let mut parts = rest.split_whitespace();
    let value = parts.next()?.parse::<u64>().ok()?;
    Some((key, value.saturating_mul(1024)))
}

fn is_smaps_header(line: &str) -> bool {
    let Some(first) = line.split_whitespace().next() else {
        return false;
    };
    let Some((start, end)) = first.split_once('-') else {
        return false;
    };
    !start.is_empty()
        && !end.is_empty()
        && start.bytes().all(|b| b.is_ascii_hexdigit())
        && end.bytes().all(|b| b.is_ascii_hexdigit())
}

fn smaps_header_name(line: &str) -> String {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() <= 5 {
        String::new()
    } else {
        parts[5..].join(" ")
    }
}

fn classify_mapping_name(name: &str) -> MappingCategory {
    let lower = name.to_ascii_lowercase();
    if name.is_empty() {
        MappingCategory::Anonymous
    } else if lower == "[heap]" {
        MappingCategory::Heap
    } else if lower.starts_with("[stack") {
        MappingCategory::Stack
    } else if lower.contains("memfd:pisp") || lower.contains("/memfd:pisp") {
        MappingCategory::PispMemfd
    } else if lower.contains("memfd:") || lower.contains("/memfd:") {
        MappingCategory::Memfd
    } else if lower.contains("libcamera")
        || lower.contains("/ipa_")
        || lower.contains("/rpi/")
        || lower.contains("pisp")
    {
        MappingCategory::LibcameraOrIpa
    } else if lower.contains("/dev/dma_heap")
        || lower.contains("/sys/kernel/debug/dma_buf")
        || lower.contains("dma-buf")
        || lower.contains("dmabuf")
    {
        MappingCategory::DmabufOrDmaHeap
    } else if lower.starts_with("/dev/") {
        MappingCategory::DeviceMapping
    } else if lower.ends_with(".so") || lower.contains(".so.") {
        MappingCategory::SharedLibraries
    } else if lower.starts_with('/') {
        MappingCategory::MmapFile
    } else {
        MappingCategory::Unknown
    }
}

fn classify_fd_target(target: &str) -> FdClass {
    let lower = target.to_ascii_lowercase();
    if lower.contains("memfd:pisp") {
        FdClass::PispMemfd
    } else if lower.starts_with("socket:") {
        FdClass::Socket
    } else if lower.starts_with("pipe:") {
        FdClass::Pipe
    } else if lower.contains("eventfd") {
        FdClass::EventFd
    } else if lower.contains("timerfd") {
        FdClass::TimerFd
    } else if lower.contains("eventpoll") || lower.contains("epoll") {
        FdClass::Epoll
    } else if lower.contains("memfd:") {
        FdClass::Memfd
    } else if lower.contains("/dev/dma_heap")
        || lower.contains("/sys/kernel/debug/dma_buf")
        || lower.contains("dma-buf")
        || lower.contains("dmabuf")
    {
        FdClass::DmaBufOrDmaHeap
    } else if lower.starts_with("/dev/video")
        || lower.starts_with("/dev/media")
        || lower.starts_with("/dev/v4l")
    {
        FdClass::MediaOrVideoDevice
    } else if lower.starts_with("anon_inode:") || lower.starts_with("anon_inode") {
        FdClass::AnonInode
    } else if lower.starts_with('/') {
        FdClass::RegularFile
    } else {
        FdClass::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_smaps_rollup_memory_fields() {
        let stats = parse_smaps_rollup(
            r#"5a4c0000-7ffd0000 ---p 00000000 00:00 0 [rollup]
Rss:                1024 kB
Pss:                 512 kB
Shared_Clean:        128 kB
Shared_Dirty:         64 kB
Private_Clean:       256 kB
Private_Dirty:       128 kB
Swap:                 32 kB
SwapPss:              16 kB
"#,
        );

        assert!(stats.available);
        assert_eq!(stats.rss_bytes, Some(1024 * 1024));
        assert_eq!(stats.pss_bytes, Some(512 * 1024));
        assert_eq!(stats.shared_clean_bytes, Some(128 * 1024));
        assert_eq!(stats.shared_dirty_bytes, Some(64 * 1024));
        assert_eq!(stats.private_clean_bytes, Some(256 * 1024));
        assert_eq!(stats.private_dirty_bytes, Some(128 * 1024));
        assert_eq!(stats.swap_bytes, Some(32 * 1024));
        assert_eq!(stats.swap_pss_bytes, Some(16 * 1024));
    }

    #[test]
    fn groups_smaps_entries_by_mapping_category() {
        let entries = parse_smaps(
            r#"7f000000-7f001000 rw-p 00000000 00:00 0
Rss:                   4 kB
Pss:                   4 kB
Private_Dirty:         4 kB
7f001000-7f003000 rw-s 00000000 00:01 1 /memfd:pisp_frontend (deleted)
Rss:                   8 kB
Pss:                   6 kB
Shared_Dirty:          8 kB
7f003000-7f004000 r-xp 00000000 08:01 2 /usr/lib/libcamera.so.1
Rss:                   4 kB
Pss:                   1 kB
Shared_Clean:          4 kB
7f004000-7f006000 r-xp 00000000 08:01 3 /usr/lib/libcamera/ipa_rpi_pisp.so
Rss:                   8 kB
Pss:                   2 kB
Shared_Clean:          8 kB
7f006000-7f007000 r-xp 00000000 08:01 4 /usr/lib/libc.so.6
Rss:                   4 kB
Pss:                   1 kB
Shared_Clean:          4 kB
"#,
        );

        let grouped = mapping_category_stats(entries);
        let pisp = grouped
            .iter()
            .find(|stats| stats.category == "pisp_memfd")
            .expect("pisp category");
        assert_eq!(pisp.mappings, 1);
        assert_eq!(pisp.pss_bytes, 6 * 1024);
        let anon = grouped
            .iter()
            .find(|stats| stats.category == "anonymous")
            .expect("anonymous category");
        assert_eq!(anon.private_bytes, 4 * 1024);
        let libs = grouped
            .iter()
            .find(|stats| stats.category == "shared_libraries")
            .expect("shared library category");
        assert_eq!(libs.shared_bytes, 4 * 1024);
        let libcamera = grouped
            .iter()
            .find(|stats| stats.category == "libcamera_or_ipa")
            .expect("libcamera/ipa category");
        assert_eq!(libcamera.mappings, 2);
        assert_eq!(libcamera.pss_bytes, 3 * 1024);
    }

    #[test]
    fn classifies_fd_targets() {
        assert_eq!(
            classify_fd_target("/memfd:pisp_backend (deleted)"),
            FdClass::PispMemfd
        );
        assert_eq!(classify_fd_target("socket:[123]"), FdClass::Socket);
        assert_eq!(classify_fd_target("pipe:[123]"), FdClass::Pipe);
        assert_eq!(classify_fd_target("anon_inode:[eventfd]"), FdClass::EventFd);
        assert_eq!(classify_fd_target("anon_inode:[timerfd]"), FdClass::TimerFd);
        assert_eq!(classify_fd_target("anon_inode:[eventpoll]"), FdClass::Epoll);
        assert_eq!(
            classify_fd_target("/dev/video0"),
            FdClass::MediaOrVideoDevice
        );
        assert_eq!(
            classify_fd_target("/sys/kernel/debug/dma_buf/bufinfo"),
            FdClass::DmaBufOrDmaHeap
        );
        assert_eq!(classify_fd_target("/tmp/file"), FdClass::RegularFile);
    }

    #[test]
    fn parses_cma_meminfo_fields() {
        let stats = parse_cma_stats(
            "\
MemTotal:        1024 kB
CmaTotal:         256 kB
CmaFree:           64 kB
",
        );

        assert_eq!(stats.total_bytes, Some(256 * 1024));
        assert_eq!(stats.free_bytes, Some(64 * 1024));
        assert_eq!(stats.used_bytes, Some(192 * 1024));
    }

    #[test]
    fn parses_dma_bufinfo_exporter_totals() {
        let stats = parse_dma_bufinfo(
            "\
Dma-buf Objects:
size flags mode count exp_name ino
00001000 00000000 00000000 00000002 system 42
8192 00000000 00000000 00000001 pisp 43
0x2000 00000000 00000000 00000001 pisp 44
",
        );

        assert_eq!(stats.total_buffers, 3);
        assert_eq!(stats.total_bytes, 4096 + 8192 + 8192);
        assert_eq!(stats.exporters[0].exporter, "pisp");
        assert_eq!(stats.exporters[0].buffers, 2);
        assert_eq!(stats.exporters[0].bytes, 16_384);
    }

    #[test]
    fn known_memory_adds_styx_and_graph_tracked_bytes() {
        let styx = PipelineMemoryStats {
            capture_queue: None,
            external_backings: vec![crate::metrics::ExternalBackingStats {
                label: "test_dmabuf".to_string(),
                current_buffers: 2,
                current_bytes: 4096,
                peak_buffers: 2,
                peak_bytes: 4096,
            }],
            transform_pool: None,
            #[cfg(target_os = "linux")]
            shared_decode_pool: None,
            #[cfg(target_os = "linux")]
            shared_encode_pool: None,
        };
        let graph = GraphTelemetryStats {
            copied_bytes: 1024,
            transport_bytes: 2048,
            ..GraphTelemetryStats::default()
        };

        assert_eq!(
            known_memory_bytes(Some(&styx), Some(&graph)),
            4096 + 1024 + 2048
        );
    }

    #[test]
    fn compact_report_formats_key_sections() {
        let report = RuntimeMemoryReport {
            process: ProcessMemoryStats {
                available: true,
                pss_bytes: Some(2 * 1024 * 1024),
                rss_bytes: Some(3 * 1024 * 1024),
                ..ProcessMemoryStats::default()
            },
            health: Some(HealthReport {
                copy_count: 2,
                bytes_moved: 4096,
                ..HealthReport::default()
            }),
            unexplained_pss_bytes: Some(1024),
            ..RuntimeMemoryReport::default()
        };

        let formatted = report.to_compact_string();
        assert!(formatted.contains("Process:"));
        assert!(formatted.contains("PSS: 2.0 MiB"));
        assert!(formatted.contains("copies: 2 / bytes moved 4.0 KiB"));
        assert!(formatted.contains("Unexplained PSS: 1.0 KiB"));
    }

    #[test]
    #[ignore = "requires target kernel/debugfs DMA-BUF telemetry"]
    fn kernel_dmabuf_collector_is_opt_in() {
        let stats = collect_kernel_dmabuf_stats();
        assert!(
            stats.available || stats.unavailable_reason.is_some(),
            "collector should either return data or explain why it is unavailable"
        );
    }
}
