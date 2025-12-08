use std::io::{BufRead, BufReader};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

const NETCAM_MAX_JPEG_BYTES: usize = 32 << 20; // 32 MiB safety cap.

#[cfg(feature = "netcam-video")]
use ffmpeg_next::{
    codec, format,
    frame::Video as FfFrame,
    media::Type as StreamType,
    software::scaling::{context::Context as ScalingContext, flag::Flags},
    util::format::pixel::Pixel as PixelFormat,
};
#[cfg(feature = "async")]
use futures_core::Stream;
#[cfg(feature = "async")]
use futures_util::TryStreamExt;
use reqwest::blocking::Client as BlockingClient;
use styx_core::prelude::*;
#[cfg(feature = "async")]
use tokio_util::bytes::Bytes;
#[cfg(feature = "async")]
use tokio_util::io::StreamReader;

#[cfg(feature = "netcam-video")]
use crate::capture_api::ffmpeg_util::blit_rgba_frame;
use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, WorkerHandle,
};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};

/// Basic MJPEG-over-HTTP backend. Expects `multipart/x-mixed-replace` with JPEG parts.
pub(super) fn start_netcam(
    backend: &ProbedBackend,
    mode: Mode,
    _interval: Option<Interval>,
    descriptor: CaptureDescriptor,
) -> Result<CaptureHandle, CaptureError> {
    let (url, width, height, fps) = match &backend.handle {
        BackendHandle::Netcam {
            url,
            width,
            height,
            fps,
        } => (url.clone(), *width, *height, *fps),
        _ => return Err(CaptureError::Backend("netcam url missing".into())),
    };

    let queue_depth = crate::capture_api::capture_queue_depth();
    let (tx_raw, rx) = styx_core::queue::bounded(queue_depth);
    let tx = Arc::new(tx_raw);
    let url_for_thread = url.clone();
    let tx_for_thread = tx.clone();
    let tunables = crate::capture_api::netcam_tunables();
    let worker_fn = move || {
        let client = match BlockingClient::builder()
            .timeout(Duration::from_secs(tunables.request_timeout_secs))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let start = Instant::now();
        let mut frame_idx: u64 = 0;
        let mut backoff = Duration::from_millis(tunables.backoff_start_ms);
        let mut consecutive_failures: u32 = 0;
        loop {
            // First try MJPEG.
            if let Ok(resp) = client.get(&url_for_thread).send() {
                if let Some(boundary) = resp
                    .headers()
                    .get("content-type")
                    .and_then(|h| h.to_str().ok())
                    .and_then(parse_boundary)
                {
                    if mjpeg_loop(
                        resp,
                        tx_for_thread.as_ref(),
                        &boundary,
                        width,
                        height,
                        fps,
                        &start,
                        &mut frame_idx,
                    ) {
                        return;
                    }
                }
            }
            // Fallback to FFmpeg for H264/H265/other container streams.
            #[cfg(feature = "netcam-video")]
            {
                if ffmpeg_loop(
                    &url_for_thread,
                    tx_for_thread.as_ref(),
                    &start,
                    &mut frame_idx,
                )
                .is_ok()
                {
                    consecutive_failures = 0;
                    continue;
                }
            }
            std::thread::sleep(backoff);
            consecutive_failures = consecutive_failures.saturating_add(1);
            backoff = (backoff * 2).min(Duration::from_millis(tunables.backoff_max_ms));
            if consecutive_failures >= 5 {
                // Periodically reset backoff to avoid long stalls when the source recovers.
                backoff = Duration::from_millis(tunables.backoff_start_ms);
                consecutive_failures = 0;
            }
        }
    };
    let worker = {
        #[cfg(feature = "async")]
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            WorkerHandle::Async(handle.spawn(async_netcam_worker(
                url,
                width,
                height,
                fps,
                tx.clone(),
                tunables,
            )))
        } else {
            WorkerHandle::Thread(std::thread::spawn(worker_fn))
        }
        #[cfg(not(feature = "async"))]
        {
            WorkerHandle::Thread(std::thread::spawn(worker_fn))
        }
    };

    Ok(CaptureHandle {
        backend: BackendKind::Netcam,
        control: ControlPlane::None,
        descriptor,
        mode,
        interval: Some(Interval {
            numerator: NonZeroU32::new(1).unwrap(),
            denominator: NonZeroU32::new(fps.max(1)).unwrap(),
        }),
        rx,
        stop_tx: None,
        worker: Some(worker),
        metrics: StageMetrics::default(),
    })
}

#[cfg(feature = "async")]
async fn async_netcam_worker(
    url: String,
    width: u32,
    height: u32,
    fps: u32,
    tx: Arc<styx_core::queue::BoundedTx<FrameLease>>,
    tunables: crate::capture_api::NetcamTunables,
) {
    use tokio::time::sleep;

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(tunables.request_timeout_secs))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };

    let start = Instant::now();
    let mut frame_idx: u64 = 0;
    let mut backoff = Duration::from_millis(tunables.backoff_start_ms);
    let mut consecutive_failures: u32 = 0;
    loop {
        if let Ok(resp) = client.get(&url).send().await {
            if let Some(boundary) = resp
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .and_then(parse_boundary)
            {
                let stream = resp
                    .bytes_stream()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
                let reader = StreamReader::new(stream);
                let mut reader = tokio::io::BufReader::new(reader);
                if async_mjpeg_loop(
                    &mut reader,
                    tx.as_ref(),
                    &boundary,
                    width,
                    height,
                    fps,
                    &start,
                    &mut frame_idx,
                )
                .await
                {
                    return;
                } else {
                    continue;
                }
            }
        }
        #[cfg(feature = "netcam-video")]
        {
            if let Ok(()) = tokio::task::spawn_blocking({
                let url = url.clone();
                let tx = tx.clone();
                let start = start.clone();
                let mut frame_idx = frame_idx;
                move || ffmpeg_loop(&url, &tx, &start, &mut frame_idx)
            })
            .await
            .unwrap_or(Err(CaptureError::Backend("ffmpeg join error".into())))
            {
                consecutive_failures = 0;
                continue;
            }
        }
        sleep(backoff).await;
        consecutive_failures = consecutive_failures.saturating_add(1);
        backoff = (backoff * 2).min(Duration::from_millis(tunables.backoff_max_ms));
        if consecutive_failures >= 5 {
            backoff = Duration::from_millis(tunables.backoff_start_ms);
            consecutive_failures = 0;
        }
    }
}

#[cfg(feature = "async")]
async fn async_mjpeg_loop<S>(
    reader: &mut tokio::io::BufReader<StreamReader<S, Bytes>>,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    boundary: &str,
    width: u32,
    height: u32,
    fps: u32,
    start: &Instant,
    frame_idx: &mut u64,
) -> bool
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let expected_pixels = if width > 0 && height > 0 {
        width as usize * height as usize
    } else {
        // Unknown resolution: pick a reasonable default for pool sizing.
        1280usize * 720usize
    };
    let (pool_min, pool_bytes, pool_spare) =
        crate::capture_api::capture_pool_limits(4, expected_pixels.saturating_mul(3), 8);
    let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);
    let mut line = Vec::with_capacity(1024);
    let mut buf = Vec::with_capacity(
        (expected_pixels.saturating_mul(3)).min(NETCAM_MAX_JPEG_BYTES),
    );
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
        if !line.starts_with(boundary.as_bytes()) {
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
            if let Some(rest) = line
                .strip_prefix(b"Content-Length:")
                .or_else(|| line.strip_prefix(b"content-length:"))
            {
                if let Some(v) = std::str::from_utf8(rest)
                    .ok()
                    .and_then(|s| s.trim().parse::<usize>().ok())
                {
                    content_length = Some(v.min(NETCAM_MAX_JPEG_BYTES));
                }
            }
        }
        buf.clear();
        match content_length {
            Some(len) => {
                let target = len.min(NETCAM_MAX_JPEG_BYTES);
                while buf.len() < target {
                    let take = match reader.fill_buf().await {
                        Ok(data) => {
                            if data.is_empty() {
                                None
                            } else {
                                let need = target - buf.len();
                                let take = data.len().min(need);
                                buf.extend_from_slice(&data[..take]);
                                Some(take)
                            }
                        }
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
                let outcome = match reader.fill_buf().await {
                    Ok(data) => {
                        if data.is_empty() {
                            None
                        } else if let Some(idx) = find_subslice(data, boundary.as_bytes()) {
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
        if buf.is_empty() {
            continue;
        }
        let res = Resolution::new(width, height)
            .or_else(|| jpeg_dimensions(&buf).and_then(|(w, h)| Resolution::new(w, h)))
            .or_else(|| Resolution::new(1, 1))
            .unwrap();
        let layout = PlaneLayout {
            offset: 0,
            len: buf.len(),
            stride: buf.len(),
        };
        let mut lease = pool.lease();
        lease.resize(buf.len());
        lease.as_mut_slice().copy_from_slice(&buf);
        let timestamp = start
            .elapsed()
            .as_nanos()
            .saturating_sub(0)
            .min(u64::MAX as u128) as u64;
        let frame = FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(FourCc::new(*b"MJPG"), res, ColorSpace::Srgb),
                timestamp,
            ),
            lease,
            layout.len,
            layout.stride,
        );
        *frame_idx = frame_idx.saturating_add(1);
        if let SendOutcome::Closed = tx.send(frame) {
            return true;
        }
        if fps > 0 {
            tokio::time::sleep(Duration::from_millis((1_000 / fps.max(1)) as u64)).await;
        }
    }
    false
}

fn parse_boundary(content_type: &str) -> Option<String> {
    for part in content_type.split(';').map(|s| s.trim()) {
        if let Some(b) = part.strip_prefix("boundary=") {
            return Some(format!("--{}", b.trim_matches('"')));
        }
    }
    None
}

fn jpeg_dimensions(buf: &[u8]) -> Option<(u32, u32)> {
    // Minimal JPEG SOF parser: find Start Of Frame marker and read dimensions.
    // Handles common SOF markers (baseline/progressive/etc).
    let mut i = 0usize;
    while i + 4 < buf.len() {
        if buf[i] != 0xFF {
            i += 1;
            continue;
        }
        // Skip fill bytes.
        let mut j = i + 1;
        while j < buf.len() && buf[j] == 0xFF {
            j += 1;
        }
        if j >= buf.len() {
            break;
        }
        let marker = buf[j];
        // SOF0..SOF15 (except DHT/DAC/etc) - include baseline/progressive lossless variants.
        let is_sof = matches!(
            marker,
            0xC0 | 0xC1 | 0xC2 | 0xC3 | 0xC5 | 0xC6 | 0xC7 | 0xC9 | 0xCA | 0xCB | 0xCD | 0xCE | 0xCF
        );
        // Standalone markers without length.
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
            // Segment layout: len(2) precision(1) height(2) width(2) ...
            if j + 2 + 1 + 4 >= buf.len() {
                break;
            }
            let h = u16::from_be_bytes([buf[j + 4], buf[j + 5]]) as u32;
            let w = u16::from_be_bytes([buf[j + 6], buf[j + 7]]) as u32;
            if w > 0 && h > 0 {
                return Some((w, h));
            }
        }
        // Skip to next segment.
        i = j + 1 + seg_len;
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn mjpeg_loop(
    resp: reqwest::blocking::Response,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    boundary: &str,
    width: u32,
    height: u32,
    fps: u32,
    start: &Instant,
    frame_idx: &mut u64,
) -> bool {
    let mut reader = BufReader::new(resp);
    let expected_pixels = if width > 0 && height > 0 {
        width as usize * height as usize
    } else {
        1280usize * 720usize
    };
    let (pool_min, pool_bytes, pool_spare) = crate::capture_api::capture_pool_limits(4, expected_pixels.saturating_mul(3), 8);
    let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);
    let mut line = Vec::with_capacity(1024);
    let mut buf = Vec::with_capacity(
        (expected_pixels.saturating_mul(3)).min(NETCAM_MAX_JPEG_BYTES),
    );
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
        if !line.starts_with(boundary.as_bytes()) {
            continue;
        }
        // Skip headers until empty line.
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
            // Track Content-Length to bound reads when provided.
            if let Some(rest) = line
                .strip_prefix(b"Content-Length:")
                .or_else(|| line.strip_prefix(b"content-length:"))
            {
                if let Some(v) = std::str::from_utf8(rest)
                    .ok()
                    .and_then(|s| s.trim().parse::<usize>().ok())
                {
                    content_length = Some(v.min(NETCAM_MAX_JPEG_BYTES));
                }
            }
        }
        // Read JPEG until next boundary.
        buf.clear();
        match content_length {
            Some(len) => {
                let target = len.min(NETCAM_MAX_JPEG_BYTES);
                while buf.len() < target {
                    let chunk = match reader.fill_buf() {
                        Ok(data) => data,
                        Err(_) => break,
                    };
                    let chunk_len = chunk.len();
                    if chunk_len == 0 {
                        break;
                    }
                    let need = target - buf.len();
                    let take = chunk_len.min(need);
                    buf.extend_from_slice(&chunk[..take]);
                    reader.consume(take);
                }
            }
            None => loop {
                match reader.fill_buf() {
                    Ok(data) if data.is_empty() => break,
                    Ok(data) => {
                        if let Some(idx) = find_subslice(data, boundary.as_bytes()) {
                            buf.extend_from_slice(&data[..idx]);
                            reader.consume(idx);
                            break;
                        } else {
                            let take = data
                                .len()
                                .min(NETCAM_MAX_JPEG_BYTES.saturating_sub(buf.len()));
                            if take == 0 {
                                break;
                            }
                            buf.extend_from_slice(&data[..take]);
                            reader.consume(take);
                            continue;
                        }
                    }
                    Err(_) => break,
                };
            },
        }
        if buf.is_empty() {
            continue;
        }
        let res = Resolution::new(width, height)
            .or_else(|| jpeg_dimensions(&buf).and_then(|(w, h)| Resolution::new(w, h)))
            .or_else(|| Resolution::new(1, 1))
            .unwrap();
        let layout = PlaneLayout {
            offset: 0,
            len: buf.len(),
            stride: buf.len(),
        };
        let mut lease = pool.lease();
        lease.resize(buf.len());
        lease.as_mut_slice().copy_from_slice(&buf);
        let timestamp = start
            .elapsed()
            .as_nanos()
            .saturating_sub(0)
            .min(u64::MAX as u128) as u64;
        let frame = FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(FourCc::new(*b"MJPG"), res, ColorSpace::Srgb),
                timestamp,
            ),
            lease,
            layout.len,
            layout.stride,
        );
        *frame_idx = frame_idx.saturating_add(1);
        if let SendOutcome::Closed = tx.send(frame) {
            return true;
        }
        if fps > 0 {
            std::thread::sleep(Duration::from_millis((1_000 / fps.max(1)) as u64));
        }
    }
    false
}

#[cfg(feature = "netcam-video")]
fn ffmpeg_loop(
    url: &str,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    start: &Instant,
    frame_idx: &mut u64,
) -> Result<(), CaptureError> {
    ffmpeg_next::init().map_err(|e| CaptureError::Backend(e.to_string()))?;
    let mut pool: Option<BufferPool> = None;
    loop {
        let mut ictx = match format::input(url) {
            Ok(ctx) => ctx,
            Err(e) => return Err(CaptureError::Backend(e.to_string())),
        };
        let stream_idx = match ictx.streams().best(StreamType::Video).map(|s| s.index()) {
            Some(idx) => idx,
            None => return Err(CaptureError::Backend("no video stream".into())),
        };
        let stream = ictx
            .stream(stream_idx)
            .ok_or_else(|| CaptureError::Backend("stream missing".into()))?;
        let ctx_decoder = codec::Context::from_parameters(stream.parameters())
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        let mut decoder = ctx_decoder
            .decoder()
            .video()
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        let mut scaler = ScalingContext::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            PixelFormat::RGBA,
            decoder.width(),
            decoder.height(),
            Flags::BILINEAR,
        )
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
        let mut decoded = FfFrame::empty();
        let mut rgb = FfFrame::empty();
        rgb.set_format(PixelFormat::RGBA);
        rgb.set_width(decoder.width());
        rgb.set_height(decoder.height());
        unsafe {
            rgb.alloc(PixelFormat::RGBA, decoder.width(), decoder.height());
        }
        let res = Resolution::new(decoder.width() as u32, decoder.height() as u32)
            .ok_or_else(|| CaptureError::Backend("invalid video resolution".into()))?;
        let layout = plane_layout_from_dims(res.width, res.height, 4);
        let pool_ref = pool.get_or_insert_with(|| {
            let (pool_min, pool_bytes, pool_spare) =
                crate::capture_api::capture_pool_limits(4, layout.len, 8);
            BufferPool::with_limits(pool_min, pool_bytes, pool_spare)
        });
        for (stream, packet) in ictx.packets() {
            if stream.index() != stream_idx {
                continue;
            }
            if let Err(_) = decoder.send_packet(&packet) {
                break;
            }
            while decoder.receive_frame(&mut decoded).is_ok() {
                if scaler.run(&decoded, &mut rgb).is_err() {
                    continue;
                }
                let ts = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                let frame = blit_rgba_frame(&rgb, res, layout, pool_ref, ts);
                *frame_idx = frame_idx.saturating_add(1);
                if let SendOutcome::Closed = tx.send(frame) {
                    return Ok(());
                }
            }
        }
        decoder.send_eof().ok();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if scaler.run(&decoded, &mut rgb).is_err() {
                continue;
            }
            let ts = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            let frame = blit_rgba_frame(&rgb, res, layout, pool_ref, ts);
            *frame_idx = frame_idx.saturating_add(1);
            if let SendOutcome::Closed = tx.send(frame) {
                return Ok(());
            }
        }
        // Loop and reconnect on exit.
    }
}
