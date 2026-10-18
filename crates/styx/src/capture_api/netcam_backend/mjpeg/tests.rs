use super::*;

struct TestPool;

impl MjpegFramePool for TestPool {
    fn frame_from_jpeg(
        &self,
        meta: FrameMeta,
        layout: PlaneLayout,
        buf: &[u8],
    ) -> Option<FrameLease> {
        let pool = BufferPool::with_limits(1, layout.len.max(1), 1);
        let mut lease = pool.lease();
        lease.resize(layout.len);
        lease.as_mut_slice().copy_from_slice(buf);
        Some(FrameLease::single_plane(
            meta,
            lease,
            layout.len,
            layout.stride,
        ))
    }
}

#[test]
fn build_mjpeg_frame_respects_configured_size_limit() {
    let mut frame_idx = 0;
    let start = Instant::now();
    let under_limit = build_mjpeg_frame(
        &TestPool,
        &[0xff, 0xd8, 0xff],
        MjpegFrameEmit {
            width: 1,
            height: 1,
            start: &start,
            clock: ClockSource::Native,
            frame_idx: &mut frame_idx,
            stream: "test",
            send_timeout: Duration::from_millis(1),
            max_jpeg_bytes: 4,
            retry_metrics: Default::default(),
        },
    );
    assert!(under_limit.is_some());
    assert_eq!(frame_idx, 1);

    let oversized = build_mjpeg_frame(
        &TestPool,
        &[0xff, 0xd8, 0xff, 0x00],
        MjpegFrameEmit {
            width: 1,
            height: 1,
            start: &start,
            clock: ClockSource::Native,
            frame_idx: &mut frame_idx,
            stream: "test",
            send_timeout: Duration::from_millis(1),
            max_jpeg_bytes: 4,
            retry_metrics: Default::default(),
        },
    );
    assert!(oversized.is_none());
    assert_eq!(frame_idx, 1);
}

#[test]
fn mjpeg_packet_pool_uses_compressed_packet_sizing() {
    assert_eq!(mjpeg_packet_pool_bytes(640, 480, 32 << 20), 230_400);
    assert_eq!(
        mjpeg_packet_pool_bytes(1280, 800, 32 << 20),
        crate::frame_sizing::MAX_COMPRESSED_PACKET_POOL_BYTES
    );
    assert_eq!(mjpeg_packet_pool_bytes(1280, 800, 128 * 1024), 128 * 1024);
}

#[test]
fn mjpeg_packet_pool_limits_do_not_use_raw_rgb_or_generic_pool_bytes() {
    let limits = mjpeg_packet_pool_limits(Default::default(), 1280, 800, 32 << 20);
    assert_eq!(limits.min, 4);
    assert_eq!(
        limits.bytes,
        crate::frame_sizing::MAX_COMPRESSED_PACKET_POOL_BYTES
    );
    assert_eq!(limits.spare, 8);
    assert!(limits.bytes < 1280 * 800 * 3);
    assert!(limits.bytes < crate::capture_api::DEFAULT_POOL_BYTES);
}
