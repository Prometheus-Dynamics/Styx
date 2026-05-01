use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use styx_core::prelude::*;

#[cfg(all(feature = "netcam", feature = "async"))]
use futures_core::Stream;
#[cfg(feature = "netcam")]
use std::io::{BufRead, BufReader};
#[cfg(all(feature = "netcam", feature = "async"))]
use tokio_util::bytes::Bytes;
#[cfg(all(feature = "netcam", feature = "async"))]
use tokio_util::io::StreamReader;

#[cfg(all(feature = "netcam", feature = "async"))]
use super::async_sleep_until_netcam_stop;
#[cfg(all(feature = "netcam", feature = "async"))]
use super::enqueue_netcam_frame_async;
#[cfg(feature = "netcam")]
use super::sleep_until_netcam_stop;
use super::{enqueue_netcam_frame, netcam_stopped};
use crate::metrics::CaptureRetryMetrics;

mod parser;

use parser::{MjpegBodyProgress, MjpegFrameParser, MjpegHeader};

pub(super) struct MjpegLoopContext<'a> {
    pub(super) boundary: &'a str,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) fps: u32,
    pub(super) start: &'a Instant,
    pub(super) frame_idx: &'a mut u64,
    pub(super) stop: &'a AtomicBool,
    pub(super) capture_tunables: crate::capture_api::CaptureTunables,
    pub(super) netcam_tunables: crate::capture_api::NetcamTunables,
    pub(super) retry_metrics: CaptureRetryMetrics,
}

struct MjpegFrameEmit<'a> {
    width: u32,
    height: u32,
    start: &'a Instant,
    frame_idx: &'a mut u64,
    stream: &'static str,
    send_timeout: Duration,
    max_jpeg_bytes: usize,
    retry_metrics: CaptureRetryMetrics,
}

#[cfg(all(feature = "netcam", feature = "async"))]
pub(super) async fn async_mjpeg_loop<S>(
    reader: &mut tokio::io::BufReader<StreamReader<S, Bytes>>,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    ctx: MjpegLoopContext<'_>,
) -> bool
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let MjpegLoopContext {
        boundary,
        width,
        height,
        fps,
        start,
        frame_idx,
        stop,
        capture_tunables,
        netcam_tunables,
        retry_metrics,
    } = ctx;
    let max_jpeg_bytes = netcam_tunables.max_jpeg_bytes;

    let expected_pixels = expected_pixels(width, height);
    let pool_limits = capture_tunables.pool_limits(4, expected_pixels.saturating_mul(3), 8);
    #[cfg(target_os = "linux")]
    let pool = match SharedBufferPool::with_limits(
        pool_limits.min,
        pool_limits.bytes,
        pool_limits.spare,
    ) {
        Ok(pool) => pool,
        Err(_) => return false,
    };
    #[cfg(not(target_os = "linux"))]
    let pool = BufferPool::with_limits(pool_limits.min, pool_limits.bytes, pool_limits.spare);
    let mut parser = MjpegFrameParser::new(boundary, max_jpeg_bytes, expected_pixels * 3);
    loop {
        if netcam_stopped(stop) {
            return true;
        }
        if parser.needs_boundary_line() {
            if reader
                .read_until(b'\n', parser.line_buffer())
                .await
                .ok()
                .filter(|&n| n > 0)
                .is_none()
            {
                tracing::debug!(backend = "netcam", stream = "mjpeg", "stream ended");
                break;
            }
            if !parser.accept_boundary_line() {
                continue;
            }
        }
        parser.begin_part();
        let mut content_length: Option<usize> = None;
        loop {
            if reader
                .read_until(b'\n', parser.line_buffer())
                .await
                .ok()
                .filter(|&n| n > 0)
                .is_none()
            {
                break;
            }
            match parser.header() {
                MjpegHeader::End => break,
                MjpegHeader::ContentLength(length) => content_length = Some(length.into_len()),
                MjpegHeader::Other => {}
            }
        }
        parser.clear_frame();
        match content_length {
            Some(len) => {
                if len > max_jpeg_bytes {
                    tracing::warn!(
                        backend = "netcam",
                        stream = "mjpeg-async",
                        content_length = len,
                        max_bytes = max_jpeg_bytes,
                        parser_event = "oversized_content_length",
                        "dropping oversized mjpeg frame"
                    );
                    let drained = drain_async(reader, len).await;
                    tracing::warn!(
                        backend = "netcam",
                        stream = "mjpeg-async",
                        content_length = len,
                        drained_bytes = drained,
                        remaining_bytes = len.saturating_sub(drained),
                        parser_event = "oversized_content_length_drained",
                        "drained oversized mjpeg frame bytes"
                    );
                    continue;
                }
                let target = len;
                while parser.frame_bytes().len() < target {
                    if netcam_stopped(stop) {
                        return true;
                    }
                    let take = match reader.fill_buf().await {
                        Ok(data) => parser.append_content_length_chunk(data, target),
                        Err(_) => None,
                    };
                    let Some(take) = take else { break };
                    if take == 0 {
                        break;
                    }
                    reader.consume(take);
                }
                if parser.frame_bytes().len() < target {
                    tracing::warn!(
                        backend = "netcam",
                        stream = "mjpeg-async",
                        expected_bytes = target,
                        received_bytes = parser.frame_bytes().len(),
                        parser_event = "content_length_frame_truncated",
                        "mjpeg content-length frame ended before all bytes were read"
                    );
                }
            }
            None => loop {
                if netcam_stopped(stop) {
                    return true;
                }
                let (take, outcome) = match reader.fill_buf().await {
                    Ok(data) => parser.append_boundary_chunk(data, "mjpeg-async"),
                    Err(_) => (0, MjpegBodyProgress::End),
                };
                reader.consume(take);
                match outcome {
                    MjpegBodyProgress::Continue => {}
                    MjpegBodyProgress::DroppedOversized
                    | MjpegBodyProgress::HitBoundary
                    | MjpegBodyProgress::End => break,
                }
            },
        }
        if emit_mjpeg_frame_async(
            tx,
            &pool,
            parser.frame_bytes(),
            MjpegFrameEmit {
                width,
                height,
                start,
                frame_idx,
                stream: "mjpeg-async",
                send_timeout: Duration::from_millis(netcam_tunables.send_timeout_ms),
                max_jpeg_bytes,
                retry_metrics: retry_metrics.clone(),
            },
        )
        .await
        {
            return true;
        }
        if fps > 0
            && async_sleep_until_netcam_stop(
                stop,
                mjpeg_frame_delay(fps),
                Duration::from_millis(netcam_tunables.stop_poll_ms),
            )
            .await
        {
            return true;
        }
    }
    false
}

pub(super) fn parse_boundary(content_type: &str) -> Option<String> {
    for part in content_type.split(';').map(|s| s.trim()) {
        if let Some(b) = part.strip_prefix("boundary=") {
            return Some(format!("--{}", b.trim_matches('"')));
        }
    }
    None
}

#[cfg(feature = "netcam")]
pub(super) fn mjpeg_loop(
    resp: reqwest::blocking::Response,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    ctx: MjpegLoopContext<'_>,
) -> bool {
    let MjpegLoopContext {
        boundary,
        width,
        height,
        fps,
        start,
        frame_idx,
        stop,
        capture_tunables,
        netcam_tunables,
        retry_metrics,
    } = ctx;
    let max_jpeg_bytes = netcam_tunables.max_jpeg_bytes;
    let mut reader = BufReader::new(resp);
    let expected_pixels = expected_pixels(width, height);
    let pool_limits = capture_tunables.pool_limits(4, expected_pixels.saturating_mul(3), 8);
    #[cfg(target_os = "linux")]
    let pool = match SharedBufferPool::with_limits(
        pool_limits.min,
        pool_limits.bytes,
        pool_limits.spare,
    ) {
        Ok(pool) => pool,
        Err(_) => return false,
    };
    #[cfg(not(target_os = "linux"))]
    let pool = BufferPool::with_limits(pool_limits.min, pool_limits.bytes, pool_limits.spare);
    let mut parser = MjpegFrameParser::new(boundary, max_jpeg_bytes, expected_pixels * 3);
    loop {
        if netcam_stopped(stop) {
            return true;
        }
        if parser.needs_boundary_line() {
            if reader
                .read_until(b'\n', parser.line_buffer())
                .ok()
                .filter(|&n| n > 0)
                .is_none()
            {
                tracing::debug!(backend = "netcam", stream = "mjpeg", "stream ended");
                break;
            }
            if !parser.accept_boundary_line() {
                continue;
            }
        }
        parser.begin_part();
        let mut content_length: Option<usize> = None;
        loop {
            if reader
                .read_until(b'\n', parser.line_buffer())
                .ok()
                .filter(|&n| n > 0)
                .is_none()
            {
                break;
            }
            match parser.header() {
                MjpegHeader::End => break,
                MjpegHeader::ContentLength(length) => content_length = Some(length.into_len()),
                MjpegHeader::Other => {}
            }
        }
        parser.clear_frame();
        match content_length {
            Some(len) => {
                if len > max_jpeg_bytes {
                    tracing::warn!(
                        backend = "netcam",
                        stream = "mjpeg-sync",
                        content_length = len,
                        max_bytes = max_jpeg_bytes,
                        parser_event = "oversized_content_length",
                        "dropping oversized mjpeg frame"
                    );
                    let drained = drain_sync(&mut reader, len);
                    tracing::warn!(
                        backend = "netcam",
                        stream = "mjpeg-sync",
                        content_length = len,
                        drained_bytes = drained,
                        remaining_bytes = len.saturating_sub(drained),
                        parser_event = "oversized_content_length_drained",
                        "drained oversized mjpeg frame bytes"
                    );
                    continue;
                }
                let target = len;
                while parser.frame_bytes().len() < target {
                    if netcam_stopped(stop) {
                        return true;
                    }
                    let chunk = match reader.fill_buf() {
                        Ok(data) => data,
                        Err(_) => break,
                    };
                    let chunk_len = chunk.len();
                    if chunk_len == 0 {
                        break;
                    }
                    let Some(take) = parser.append_content_length_chunk(chunk, target) else {
                        break;
                    };
                    reader.consume(take);
                }
                if parser.frame_bytes().len() < target {
                    tracing::warn!(
                        backend = "netcam",
                        stream = "mjpeg-sync",
                        expected_bytes = target,
                        received_bytes = parser.frame_bytes().len(),
                        parser_event = "content_length_frame_truncated",
                        "mjpeg content-length frame ended before all bytes were read"
                    );
                }
            }
            None => loop {
                if netcam_stopped(stop) {
                    return true;
                }
                let (take, outcome) = match reader.fill_buf() {
                    Ok(data) => parser.append_boundary_chunk(data, "mjpeg-sync"),
                    Err(_) => (0, MjpegBodyProgress::End),
                };
                reader.consume(take);
                match outcome {
                    MjpegBodyProgress::Continue => {}
                    MjpegBodyProgress::DroppedOversized
                    | MjpegBodyProgress::HitBoundary
                    | MjpegBodyProgress::End => break,
                }
            },
        }
        if emit_mjpeg_frame(
            tx,
            &pool,
            parser.frame_bytes(),
            MjpegFrameEmit {
                width,
                height,
                start,
                frame_idx,
                stream: "mjpeg-sync",
                send_timeout: Duration::from_millis(netcam_tunables.send_timeout_ms),
                max_jpeg_bytes,
                retry_metrics: retry_metrics.clone(),
            },
        ) {
            return true;
        }
        if fps > 0
            && sleep_until_netcam_stop(
                stop,
                mjpeg_frame_delay(fps),
                Duration::from_millis(netcam_tunables.stop_poll_ms),
            )
        {
            return true;
        }
    }
    false
}

fn mjpeg_frame_delay(fps: u32) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(fps.max(1))).max(Duration::from_nanos(1))
}

fn expected_pixels(width: u32, height: u32) -> usize {
    if width > 0 && height > 0 {
        width as usize * height as usize
    } else {
        1280usize * 720usize
    }
}

#[cfg(all(feature = "netcam", feature = "async"))]
async fn drain_async<S>(
    reader: &mut tokio::io::BufReader<StreamReader<S, Bytes>>,
    mut remaining: usize,
) -> usize
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut drained = 0usize;
    while remaining > 0 {
        let take = match reader.fill_buf().await {
            Ok([]) | Err(_) => break,
            Ok(data) => data.len().min(remaining),
        };
        reader.consume(take);
        drained = drained.saturating_add(take);
        remaining -= take;
    }
    drained
}

#[cfg(feature = "netcam")]
fn drain_sync<R: BufRead>(reader: &mut R, mut remaining: usize) -> usize {
    let mut drained = 0usize;
    while remaining > 0 {
        let take = match reader.fill_buf() {
            Ok([]) | Err(_) => break,
            Ok(data) => data.len().min(remaining),
        };
        reader.consume(take);
        drained = drained.saturating_add(take);
        remaining -= take;
    }
    drained
}

fn emit_mjpeg_frame<P>(
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    pool: &P,
    buf: &[u8],
    emit: MjpegFrameEmit<'_>,
) -> bool
where
    P: MjpegFramePool,
{
    let stream = emit.stream;
    let send_timeout = emit.send_timeout;
    let retry_metrics = emit.retry_metrics.clone();
    let Some(frame) = build_mjpeg_frame(pool, buf, emit) else {
        return false;
    };
    enqueue_netcam_frame(tx, frame, stream, send_timeout, &retry_metrics)
}

#[cfg(all(feature = "netcam", feature = "async"))]
async fn emit_mjpeg_frame_async<P>(
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    pool: &P,
    buf: &[u8],
    emit: MjpegFrameEmit<'_>,
) -> bool
where
    P: MjpegFramePool,
{
    let stream = emit.stream;
    let send_timeout = emit.send_timeout;
    let retry_metrics = emit.retry_metrics.clone();
    let Some(frame) = build_mjpeg_frame(pool, buf, emit) else {
        return false;
    };
    enqueue_netcam_frame_async(tx, frame, stream, send_timeout, &retry_metrics).await
}

fn build_mjpeg_frame<P>(pool: &P, buf: &[u8], emit: MjpegFrameEmit<'_>) -> Option<FrameLease>
where
    P: MjpegFramePool,
{
    let MjpegFrameEmit {
        width,
        height,
        start,
        frame_idx,
        stream: _,
        send_timeout: _,
        max_jpeg_bytes,
        retry_metrics: _,
    } = emit;
    if buf.is_empty() {
        return None;
    }
    if buf.len() >= max_jpeg_bytes {
        tracing::warn!(
            backend = "netcam",
            stream = "mjpeg",
            max_bytes = max_jpeg_bytes,
            "dropping oversized mjpeg frame"
        );
        return None;
    }
    let res = Resolution::new(width, height)
        .or_else(|| jpeg_dimensions(buf).and_then(|(w, h)| Resolution::new(w, h)))
        .or_else(|| Resolution::new(1, 1))
        .unwrap();
    let layout = PlaneLayout {
        offset: 0,
        len: buf.len(),
        stride: buf.len(),
    };
    let timestamp = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let meta = FrameMeta::new(
        MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb),
        timestamp,
    )
    .with_capture_instant(std::time::Instant::now())
    .with_transition(ResidencyTransition {
        from: FrameResidency::CompressedPacket,
        to: FrameResidency::CompressedPacket,
        reason: ResidencyTransitionReason::NetcamIngress,
        copied: false,
    });
    let frame = pool.frame_from_jpeg(meta, layout, buf)?;
    *frame_idx = frame_idx.saturating_add(1);
    Some(frame)
}

trait MjpegFramePool {
    fn frame_from_jpeg(
        &self,
        meta: FrameMeta,
        layout: PlaneLayout,
        buf: &[u8],
    ) -> Option<FrameLease>;
}

#[cfg(target_os = "linux")]
impl MjpegFramePool for SharedBufferPool {
    fn frame_from_jpeg(
        &self,
        meta: FrameMeta,
        layout: PlaneLayout,
        buf: &[u8],
    ) -> Option<FrameLease> {
        let mut lease = self.lease().ok()?;
        lease.try_resize(buf.len()).ok()?;
        lease.as_mut_slice().copy_from_slice(buf);
        FrameLease::single_plane_shared(meta, lease, layout.len, layout.stride).ok()
    }
}

#[cfg(test)]
mod tests {
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
}

#[cfg(not(target_os = "linux"))]
impl MjpegFramePool for BufferPool {
    fn frame_from_jpeg(
        &self,
        meta: FrameMeta,
        layout: PlaneLayout,
        buf: &[u8],
    ) -> Option<FrameLease> {
        let mut lease = self.lease();
        lease.resize(buf.len());
        lease.as_mut_slice().copy_from_slice(buf);
        Some(FrameLease::single_plane(
            meta,
            lease,
            layout.len,
            layout.stride,
        ))
    }
}

fn jpeg_dimensions(buf: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0usize;
    while i + 4 < buf.len() {
        if buf[i] != 0xFF {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < buf.len() && buf[j] == 0xFF {
            j += 1;
        }
        if j >= buf.len() {
            break;
        }
        let marker = buf[j];
        let is_sof = matches!(
            marker,
            0xC0 | 0xC1
                | 0xC2
                | 0xC3
                | 0xC5
                | 0xC6
                | 0xC7
                | 0xC9
                | 0xCA
                | 0xCB
                | 0xCD
                | 0xCE
                | 0xCF
        );
        let has_length = !matches!(marker, 0xD8 | 0xD9) && !(0xD0..=0xD7).contains(&marker);
        if !has_length {
            i = j + 1;
            continue;
        }
        if j + 2 >= buf.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([buf[j + 1], buf[j + 2]]) as usize;
        if seg_len < 2 {
            break;
        }
        if is_sof {
            if j + 2 + 1 + 4 >= buf.len() {
                break;
            }
            let h = u16::from_be_bytes([buf[j + 4], buf[j + 5]]) as u32;
            let w = u16::from_be_bytes([buf[j + 6], buf[j + 7]]) as u32;
            if w > 0 && h > 0 {
                return Some((w, h));
            }
        }
        i = j + 1 + seg_len;
    }
    None
}
