//! Human-readable runtime memory report formatting.

#[allow(unused_imports)]
use super::*;

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
            if let Some(pool) = &styx.decoder_pool {
                writeln!(
                    f,
                    "  decoder pool: retained {} / in-use {} / chunk {}",
                    format_bytes(pool.retained_bytes as u64),
                    format_bytes(pool.in_use_bytes as u64),
                    format_bytes(pool.chunk_size as u64)
                )?;
            }
            if let Some(pool) = &styx.encoder_pool {
                writeln!(
                    f,
                    "  encoder pool: retained {} / in-use {} / chunk {}",
                    format_bytes(pool.retained_bytes as u64),
                    format_bytes(pool.in_use_bytes as u64),
                    format_bytes(pool.chunk_size as u64)
                )?;
            }
            #[cfg(target_os = "linux")]
            {
                if let Some(pool) = &styx.shared_decode_pool {
                    writeln!(
                        f,
                        "  shared decode pool: retained {} / in-use {} / free {} / chunk {}",
                        format_bytes(pool.retained_bytes as u64),
                        format_bytes(pool.in_use_bytes as u64),
                        format_bytes(pool.free_bytes as u64),
                        format_bytes(pool.chunk_size as u64)
                    )?;
                }
                if let Some(pool) = &styx.shared_encode_pool {
                    writeln!(
                        f,
                        "  shared encode pool: retained {} / in-use {} / free {} / chunk {}",
                        format_bytes(pool.retained_bytes as u64),
                        format_bytes(pool.in_use_bytes as u64),
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
            if self.fds.dmabuf.fd_count > 0 || self.fds.dmabuf.unique_buffers > 0 {
                writeln!(
                    f,
                    "  process DMA-BUF fds: {} fds / {} unique / {}",
                    self.fds.dmabuf.fd_count,
                    self.fds.dmabuf.unique_buffers,
                    format_bytes(self.fds.dmabuf.total_bytes)
                )?;
                for exporter in self.fds.dmabuf.exporters.iter().take(5) {
                    writeln!(
                        f,
                        "    exporter {}: {} buffers / {}",
                        exporter.exporter,
                        exporter.buffers,
                        format_bytes(exporter.bytes)
                    )?;
                }
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

pub(super) fn format_bytes_opt(bytes: Option<u64>) -> String {
    bytes
        .map(format_bytes)
        .unwrap_or_else(|| "unavailable".to_string())
}

pub(super) fn format_bytes(bytes: u64) -> String {
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
