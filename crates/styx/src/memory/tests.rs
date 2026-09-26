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
fn parses_proc_fdinfo_dmabuf_and_deduplicates_by_inode() {
    let first = parse_proc_fdinfo_dmabuf(
        "\
pos: 0
flags: 02
mnt_id: 16
ino: 1234
size: 4096
exp_name: pisp
",
        "10",
        "anon_inode:[dmabuf]",
    )
    .expect("first dmabuf fdinfo");
    let second = parse_proc_fdinfo_dmabuf(
        "\
ino: 1234
size: 4096
exp_name: pisp
",
        "11",
        "anon_inode:[dmabuf]",
    )
    .expect("second dmabuf fdinfo");
    let third = parse_proc_fdinfo_dmabuf(
        "\
ino: 1235
size: 0x2000
exp_name: system
",
        "12",
        "anon_inode:[dmabuf]",
    )
    .expect("third dmabuf fdinfo");

    let mut buffers = HashMap::new();
    buffers.entry(first.key.clone()).or_insert(first);
    buffers.entry(second.key.clone()).or_insert(second);
    buffers.entry(third.key.clone()).or_insert(third);

    let stats = process_dmabuf_stats(3, buffers);
    assert_eq!(stats.fd_count, 3);
    assert_eq!(stats.unique_buffers, 2);
    assert_eq!(stats.total_bytes, 4096 + 8192);
    assert_eq!(stats.exporters[0].exporter, "system");
    assert_eq!(stats.exporters[0].bytes, 8192);
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
        decoder_pool: None,
        encoder_pool: None,
        #[cfg(target_os = "linux")]
        shared_decode_pool: None,
        #[cfg(target_os = "linux")]
        shared_encode_pool: None,
    };
    let graph = GraphTelemetryStats {
        copied_bytes: 1024,
        transport_bytes: 2048,
        current_queue_bytes: 512,
        ..GraphTelemetryStats::default()
    };

    assert_eq!(
        known_memory_bytes(Some(&styx), Some(&graph)),
        4096 + 1024 + 2048 + 512
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
