//! Process memory: smaps, mappings and file-descriptor inventory.

#[allow(unused_imports)]
use super::*;

#[cfg(target_os = "linux")]
pub(super) fn collect_process_memory(warnings: &mut Vec<String>) -> ProcessMemoryStats {
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
pub(super) fn collect_process_memory(warnings: &mut Vec<String>) -> ProcessMemoryStats {
    let reason = "process memory telemetry is only supported on linux".to_string();
    warnings.push(reason.clone());
    ProcessMemoryStats {
        available: false,
        unavailable_reason: Some(reason),
        ..ProcessMemoryStats::default()
    }
}

#[cfg(target_os = "linux")]
pub(super) fn collect_mapping_stats(warnings: &mut Vec<String>) -> Vec<MappingCategoryStats> {
    match std::fs::read_to_string("/proc/self/smaps") {
        Ok(contents) => mapping_category_stats(parse_smaps(&contents)),
        Err(err) => {
            warnings.push(format!("smaps unavailable: {err}"));
            Vec::new()
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn collect_mapping_stats(_warnings: &mut Vec<String>) -> Vec<MappingCategoryStats> {
    Vec::new()
}

#[cfg(target_os = "linux")]
pub(super) fn collect_fd_inventory(warnings: &mut Vec<String>) -> FdInventoryStats {
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
    let mut dmabuf_by_key: HashMap<String, ProcessDmabufFdEntry> = HashMap::new();
    let mut dmabuf_fd_count = 0u64;
    let mut total = 0u64;

    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let target = target.to_string_lossy().to_string();
        let class = classify_fd_target(&target);
        *class_counts.entry(class).or_default() += 1;
        if class == FdClass::DmaBufOrDmaHeap {
            dmabuf_fd_count = dmabuf_fd_count.saturating_add(1);
            let fd_name = entry.file_name().to_string_lossy().to_string();
            if let Ok(contents) = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd_name}"))
                && let Some(dmabuf) = parse_proc_fdinfo_dmabuf(&contents, &fd_name, &target)
            {
                dmabuf_by_key.entry(dmabuf.key.clone()).or_insert(dmabuf);
            }
        }
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
        dmabuf: process_dmabuf_stats(dmabuf_fd_count, dmabuf_by_key),
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn collect_fd_inventory(warnings: &mut Vec<String>) -> FdInventoryStats {
    let reason = "fd inventory telemetry is only supported on linux".to_string();
    warnings.push(reason.clone());
    FdInventoryStats {
        available: false,
        unavailable_reason: Some(reason),
        ..FdInventoryStats::default()
    }
}

pub(super) fn process_dmabuf_stats(
    fd_count: u64,
    buffers: HashMap<String, ProcessDmabufFdEntry>,
) -> ProcessDmabufFdStats {
    let mut by_exporter: HashMap<String, ProcessDmabufExporterStats> = HashMap::new();
    let mut total_bytes = 0u64;
    let unique_buffers = buffers.len() as u64;

    for entry in buffers.into_values() {
        total_bytes = total_bytes.saturating_add(entry.bytes);
        let exporter = by_exporter
            .entry(entry.exporter.clone())
            .or_insert_with(|| ProcessDmabufExporterStats {
                exporter: entry.exporter,
                buffers: 0,
                bytes: 0,
            });
        exporter.buffers = exporter.buffers.saturating_add(1);
        exporter.bytes = exporter.bytes.saturating_add(entry.bytes);
    }

    let mut exporters = by_exporter.into_values().collect::<Vec<_>>();
    exporters.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| b.buffers.cmp(&a.buffers))
            .then_with(|| a.exporter.cmp(&b.exporter))
    });

    ProcessDmabufFdStats {
        fd_count,
        unique_buffers,
        total_bytes,
        exporters,
    }
}

pub(super) fn parse_proc_fdinfo_dmabuf(
    contents: &str,
    fd_name: &str,
    target: &str,
) -> Option<ProcessDmabufFdEntry> {
    let mut size = None;
    let mut exporter = None;
    let mut ino = None;

    for line in contents.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "size" => size = parse_dma_bufinfo_size(value),
            "exp_name" => exporter = Some(value.to_string()),
            "ino" => ino = Some(value.to_string()),
            _ => {}
        }
    }

    let bytes = size?;
    let exporter = exporter.unwrap_or_else(|| "unknown".to_string());
    let key = ino
        .filter(|value| !value.is_empty())
        .map(|value| format!("ino:{value}"))
        .unwrap_or_else(|| format!("fd:{fd_name}:{target}"));

    Some(ProcessDmabufFdEntry {
        key,
        exporter,
        bytes,
    })
}

pub(super) fn parse_smaps_rollup(contents: &str) -> ProcessMemoryStats {
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

pub(super) fn parse_smaps(contents: &str) -> Vec<SmapsEntry> {
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

pub(super) fn mapping_category_stats(entries: Vec<SmapsEntry>) -> Vec<MappingCategoryStats> {
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

pub(super) fn parse_kib_line(line: &str) -> Option<(&str, u64)> {
    let (key, rest) = line.split_once(':')?;
    let mut parts = rest.split_whitespace();
    let value = parts.next()?.parse::<u64>().ok()?;
    Some((key, value.saturating_mul(1024)))
}

pub(super) fn is_smaps_header(line: &str) -> bool {
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

pub(super) fn smaps_header_name(line: &str) -> String {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() <= 5 {
        String::new()
    } else {
        parts[5..].join(" ")
    }
}

pub(super) fn classify_mapping_name(name: &str) -> MappingCategory {
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

pub(super) fn classify_fd_target(target: &str) -> FdClass {
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
