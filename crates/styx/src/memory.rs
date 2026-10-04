use std::collections::HashMap;
use std::fmt;

use crate::metrics::{HealthReport, PipelineMemoryStats};

mod display;
mod dmabuf;
mod proc;

use dmabuf::*;
use proc::*;

#[derive(Clone, Debug, Default)]
pub struct RuntimeMemoryReport {
    pub process: ProcessMemoryStats,
    pub mappings: Vec<MappingCategoryStats>,
    pub fds: FdInventoryStats,
    pub kernel_dmabuf: KernelDmabufStats,
    pub styx: Option<PipelineMemoryStats>,
    pub health: Option<HealthReport>,
    pub unexplained_pss_bytes: Option<u64>,
    pub warnings: Vec<String>,
}

impl RuntimeMemoryReport {
    pub fn to_compact_string(&self) -> String {
        self.to_string()
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
    pub dmabuf: ProcessDmabufFdStats,
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
pub struct ProcessDmabufFdStats {
    pub fd_count: u64,
    pub unique_buffers: u64,
    pub total_bytes: u64,
    pub exporters: Vec<ProcessDmabufExporterStats>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessDmabufExporterStats {
    pub exporter: String,
    pub buffers: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ProcessDmabufFdEntry {
    key: String,
    exporter: String,
    bytes: u64,
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
    runtime_memory_report_parts(None, None)
}

pub fn runtime_memory_report_with_styx(
    styx: PipelineMemoryStats,
    health: Option<HealthReport>,
) -> RuntimeMemoryReport {
    runtime_memory_report_parts(Some(styx), health)
}

pub(crate) fn runtime_memory_report_parts(
    styx: Option<PipelineMemoryStats>,
    health: Option<HealthReport>,
) -> RuntimeMemoryReport {
    let mut warnings = Vec::new();
    let process = collect_process_memory(&mut warnings);
    let mappings = collect_mapping_stats(&mut warnings);
    let fds = collect_fd_inventory(&mut warnings);
    let kernel_dmabuf = collect_kernel_dmabuf_stats();
    let unexplained_pss_bytes = process
        .pss_bytes
        .map(|pss| pss.saturating_sub(known_memory_bytes(styx.as_ref())));

    RuntimeMemoryReport {
        process,
        mappings,
        fds,
        kernel_dmabuf,
        styx,
        health,
        unexplained_pss_bytes,
        warnings,
    }
}

fn known_memory_bytes(styx: Option<&PipelineMemoryStats>) -> u64 {
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
        if let Some(pool) = &styx.decoder_pool {
            known = known.saturating_add(pool.retained_bytes as u64);
        }
        if let Some(pool) = &styx.encoder_pool {
            known = known.saturating_add(pool.retained_bytes as u64);
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(pool) = &styx.shared_decode_pool {
                known = known.saturating_add(pool.retained_bytes as u64);
            }
            if let Some(pool) = &styx.shared_encode_pool {
                known = known.saturating_add(pool.retained_bytes as u64);
            }
        }
    }
    known
}

fn sum_opts(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))).filter(|sum| *sum > 0)
}

#[cfg(test)]
mod tests;
