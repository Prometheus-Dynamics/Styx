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

const NETCAM_MAX_JPEG_BYTES: usize = 32 << 20; // 32 MiB safety cap.

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
}

struct MjpegFrameEmit<'a> {
    width: u32,
    height: u32,
    start: &'a Instant,
    frame_idx: &'a mut u64,
    stream: &'static str,
    send_timeout: Duration,
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
    } = ctx;

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
    let mut line = Vec::with_capacity(1024);
    let mut buf =
        Vec::with_capacity((expected_pixels.saturating_mul(3)).min(NETCAM_MAX_JPEG_BYTES));
    let parser = MjpegMultipartParser::new(boundary);
    loop {
        if netcam_stopped(stop) {
            return true;
        }
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .await
            .ok()
            .filter(|&n| n > 0)
            .is_none()
        {
            tracing::debug!(backend = "netcam", stream = "mjpeg", "stream ended");
            break;
        }
        if !parser.is_boundary_line(&line) {
            continue;
        }
        let mut content_length: Option<usize> = None;
        loop {
            line.clear();
            if reader
                .read_until(b'\n', &mut line)
                .await
                .ok()
                .filter(|&n| n > 0)
                .is_none()
            {
                break;
            }
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                break;
            }
            if let Some(length) = parser.content_length(&line) {
                content_length = Some(length);
            }
        }
        buf.clear();
        match content_length {
            Some(len) => {
                let target = len.min(NETCAM_MAX_JPEG_BYTES);
                while buf.len() < target {
                    if netcam_stopped(stop) {
                        return true;
                    }
                    let take = match reader.fill_buf().await {
                        Ok(data) => parser.append_content_length_chunk(data, target, &mut buf),
                        Err(_) => None,
                    };
                    let Some(take) = take else { break };
                    if take == 0 {
                        break;
                    }
                    reader.consume(take);
                }
            }
            None => loop {
                if netcam_stopped(stop) {
                    return true;
                }
                let outcome = match reader.fill_buf().await {
                    Ok(data) => parser.append_until_boundary_chunk(data, &mut buf),
                    Err(_) => None,
                };
                let Some((take, hit_boundary)) = outcome else {
                    break;
                };
                reader.consume(take);
                if hit_boundary {
                    break;
                }
            },
        }
        if emit_mjpeg_frame_async(
            tx,
            &pool,
            &buf,
            MjpegFrameEmit {
                width,
                height,
                start,
                frame_idx,
                stream: "mjpeg-async",
                send_timeout: Duration::from_millis(netcam_tunables.send_timeout_ms),
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

struct MjpegMultipartParser<'a> {
    boundary: &'a [u8],
}

impl<'a> MjpegMultipartParser<'a> {
    fn new(boundary: &'a str) -> Self {
        Self {
            boundary: boundary.as_bytes(),
        }
    }

    fn is_boundary_line(&self, line: &[u8]) -> bool {
        line.starts_with(self.boundary)
    }

    fn content_length(&self, line: &[u8]) -> Option<usize> {
        let rest = line
            .strip_prefix(b"Content-Length:")
            .or_else(|| line.strip_prefix(b"content-length:"))?;
        std::str::from_utf8(rest)
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .map(|value| value.min(NETCAM_MAX_JPEG_BYTES))
    }

    fn append_content_length_chunk(
        &self,
        data: &[u8],
        target: usize,
        buf: &mut Vec<u8>,
    ) -> Option<usize> {
        if data.is_empty() || buf.len() >= target {
            return None;
        }
        let need = target - buf.len();
        let take = data.len().min(need);
        buf.extend_from_slice(&data[..take]);
        Some(take)
    }

    fn append_until_boundary_chunk(&self, data: &[u8], buf: &mut Vec<u8>) -> Option<(usize, bool)> {
        if data.is_empty() {
            return None;
        }
        if let Some(idx) = find_subslice(data, self.boundary) {
            buf.extend_from_slice(&data[..idx]);
            Some((idx, true))
        } else {
            let take = data
                .len()
                .min(NETCAM_MAX_JPEG_BYTES.saturating_sub(buf.len()));
            if take == 0 {
                None
            } else {
                buf.extend_from_slice(&data[..take]);
                Some((take, false))
            }
        }
    }
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
    } = ctx;
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
    let mut line = Vec::with_capacity(1024);
    let mut buf =
        Vec::with_capacity((expected_pixels.saturating_mul(3)).min(NETCAM_MAX_JPEG_BYTES));
    let parser = MjpegMultipartParser::new(boundary);
    loop {
        if netcam_stopped(stop) {
            return true;
        }
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .ok()
            .filter(|&n| n > 0)
            .is_none()
        {
            tracing::debug!(backend = "netcam", stream = "mjpeg", "stream ended");
            break;
        }
        if !parser.is_boundary_line(&line) {
            continue;
        }
        let mut content_length: Option<usize> = None;
        loop {
            line.clear();
            if reader
                .read_until(b'\n', &mut line)
                .ok()
                .filter(|&n| n > 0)
                .is_none()
            {
                break;
            }
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                break;
            }
            if let Some(length) = parser.content_length(&line) {
                content_length = Some(length);
            }
        }
        buf.clear();
        match content_length {
            Some(len) => {
                let target = len.min(NETCAM_MAX_JPEG_BYTES);
                while buf.len() < target {
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
                    let Some(take) = parser.append_content_length_chunk(chunk, target, &mut buf)
                    else {
                        break;
                    };
                    reader.consume(take);
                }
            }
            None => loop {
                if netcam_stopped(stop) {
                    return true;
                }
                match reader.fill_buf() {
                    Ok([]) => break,
                    Ok(data) => match parser.append_until_boundary_chunk(data, &mut buf) {
                        Some((take, hit_boundary)) => {
                            reader.consume(take);
                            if hit_boundary {
                                break;
                            }
                            continue;
                        }
                        None => break,
                    },
                    Err(_) => break,
                };
            },
        }
        if emit_mjpeg_frame(
            tx,
            &pool,
            &buf,
            MjpegFrameEmit {
                width,
                height,
                start,
                frame_idx,
                stream: "mjpeg-sync",
                send_timeout: Duration::from_millis(netcam_tunables.send_timeout_ms),
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
    let Some(frame) = build_mjpeg_frame(pool, buf, emit) else {
        return false;
    };
    enqueue_netcam_frame(tx, frame, stream, send_timeout)
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
    let Some(frame) = build_mjpeg_frame(pool, buf, emit) else {
        return false;
    };
    enqueue_netcam_frame_async(tx, frame, stream, send_timeout).await
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
    } = emit;
    if buf.is_empty() {
        return None;
    }
    if buf.len() >= NETCAM_MAX_JPEG_BYTES {
        tracing::warn!(
            backend = "netcam",
            stream = "mjpeg",
            max_bytes = NETCAM_MAX_JPEG_BYTES,
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

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_length_header_parser_is_shared_and_capped() {
        let parser = MjpegMultipartParser::new("--frame");
        assert_eq!(parser.content_length(b"Content-Length: 42\r\n"), Some(42));
        assert_eq!(parser.content_length(b"content-length: 7\n"), Some(7));
        assert_eq!(
            parser.content_length(b"Content-Length: 999999999\r\n"),
            Some(NETCAM_MAX_JPEG_BYTES)
        );
        assert_eq!(parser.content_length(b"Content-Type: image/jpeg\r\n"), None);
    }

    #[test]
    fn boundary_chunk_parser_reports_take_and_hit_boundary() {
        let parser = MjpegMultipartParser::new("--frame");
        let mut buf = Vec::new();
        assert_eq!(
            parser.append_until_boundary_chunk(b"abc--frame", &mut buf),
            Some((3, true))
        );
        assert_eq!(buf, b"abc");

        let mut buf = Vec::new();
        assert_eq!(
            parser.append_until_boundary_chunk(b"abcdef", &mut buf),
            Some((6, false))
        );
        assert_eq!(buf, b"abcdef");
    }
}
