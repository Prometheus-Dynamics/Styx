use std::num::NonZeroU32;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

#[cfg(feature = "netcam-video")]
use ffmpeg_next::{
    format,
    frame::Video as FfFrame,
    media::Type as StreamType,
    software::scaling::{context::Context as ScalingContext, flag::Flags},
    util::format::pixel::Pixel as PixelFormat,
};
#[cfg(all(feature = "netcam", feature = "async"))]
use futures_util::TryStreamExt;
#[cfg(feature = "netcam")]
use reqwest::blocking::Client as BlockingClient;
use styx_core::prelude::*;
#[cfg(all(feature = "netcam", feature = "async"))]
use tokio_util::io::StreamReader;

#[cfg(all(feature = "netcam-video", not(target_os = "linux")))]
use crate::capture_api::ffmpeg_util::blit_rgba_frame;
#[cfg(all(feature = "netcam-video", target_os = "linux"))]
use crate::capture_api::ffmpeg_util::blit_shared_rgba_frame;
#[cfg(feature = "netcam-video")]
use crate::capture_api::ffmpeg_util::open_preferred_video_decoder;
use crate::capture_api::handle::record_worker_error;
use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, StyxConfig, WorkerHandle,
};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};

mod mjpeg;
#[cfg(all(feature = "netcam", feature = "async"))]
use mjpeg::async_mjpeg_loop;
use mjpeg::{MjpegLoopContext, mjpeg_loop, parse_boundary};
#[cfg(test)]
mod tests;

#[cfg(all(feature = "netcam", feature = "async"))]
struct AsyncNetcamWorker {
    url: String,
    width: u32,
    height: u32,
    fps: u32,
    tunables: crate::capture_api::NetcamTunables,
    capture_tunables: crate::capture_api::CaptureTunables,
    stop: Arc<AtomicBool>,
    worker_error: Arc<Mutex<Option<CaptureError>>>,
}

/// Basic MJPEG-over-HTTP backend. Expects `multipart/x-mixed-replace` with JPEG parts.
pub(super) fn start_netcam(
    backend: &ProbedBackend,
    mode: Mode,
    _interval: Option<Interval>,
    descriptor: CaptureDescriptor,
    config: &StyxConfig,
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

    let capture_tunables = config.capture_tunables();
    let queue_depth = capture_tunables.queue_depth;
    let (tx_raw, rx) = styx_core::queue::bounded(queue_depth);
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_watcher = Arc::clone(&stop);
    let stop_watcher = std::thread::spawn(move || {
        let _ = stop_rx.recv();
        stop_for_watcher.store(true, Ordering::Release);
    });
    let tx = Arc::new(tx_raw);
    #[cfg(feature = "netcam")]
    let url_for_thread = url.clone();
    #[cfg(feature = "netcam")]
    let tx_for_thread = tx.clone();
    #[cfg(feature = "netcam")]
    let stop_for_thread = Arc::clone(&stop);
    let worker_error = Arc::new(Mutex::new(None));
    #[cfg(feature = "netcam")]
    let worker_error_for_thread = Arc::clone(&worker_error);
    let tunables = config.netcam_tunables();
    #[cfg(feature = "netcam")]
    let worker_fn = move || {
        tracing::debug!(
            backend = "netcam",
            mode = "sync",
            url = %url_for_thread,
            width,
            height,
            fps,
            request_timeout_secs = tunables.request_timeout_secs,
            connect_timeout_ms = tunables.connect_timeout_ms,
            read_timeout_ms = tunables.read_timeout_ms,
            "starting netcam worker"
        );
        let client = match BlockingClient::builder()
            .timeout(Duration::from_secs(tunables.request_timeout_secs))
            .connect_timeout(Duration::from_millis(tunables.connect_timeout_ms))
            .build()
        {
            Ok(c) => c,
            Err(err) => {
                let capture_err = CaptureError::Backend(format!("netcam client failed: {err}"));
                record_worker_error(&worker_error_for_thread, &capture_err);
                tracing::warn!(backend = "netcam", error = %err, "failed to create netcam client");
                return;
            }
        };
        let start = Instant::now();
        let mut frame_idx: u64 = 0;
        let mut backoff = Duration::from_millis(tunables.backoff_start_ms);
        let mut consecutive_failures: u32 = 0;
        loop {
            if netcam_stopped(&stop_for_thread) {
                tracing::debug!(backend = "netcam", "netcam worker stopped");
                return;
            }
            // First try MJPEG.
            match client.get(&url_for_thread).send() {
                Ok(resp) => {
                    let boundary = resp
                        .headers()
                        .get("content-type")
                        .and_then(|h| h.to_str().ok())
                        .and_then(parse_boundary);
                    if let Some(boundary) = boundary {
                        tracing::debug!(
                            backend = "netcam",
                            stream = "mjpeg",
                            width,
                            height,
                            fps,
                            "connected"
                        );
                        if mjpeg_loop(
                            resp,
                            tx_for_thread.as_ref(),
                            MjpegLoopContext {
                                boundary: &boundary,
                                width,
                                height,
                                fps,
                                start: &start,
                                frame_idx: &mut frame_idx,
                                stop: &stop_for_thread,
                                capture_tunables,
                                netcam_tunables: tunables,
                            },
                        ) {
                            return;
                        }
                    } else {
                        tracing::debug!(
                            backend = "netcam",
                            "netcam response was not multipart mjpeg"
                        );
                    }
                }
                Err(err) => {
                    let capture_err =
                        CaptureError::Backend(format!("netcam request failed: {err}"));
                    record_worker_error(&worker_error_for_thread, &capture_err);
                    tracing::warn!(backend = "netcam", error = %err, "netcam request failed");
                }
            }
            // Fallback to FFmpeg for H264/H265/other container streams.
            #[cfg(feature = "netcam-video")]
            {
                match ffmpeg_loop(
                    &url_for_thread,
                    tx_for_thread.as_ref(),
                    &start,
                    &mut frame_idx,
                    Arc::clone(&stop_for_thread),
                    capture_tunables,
                    tunables,
                ) {
                    Ok(()) => {
                        consecutive_failures = 0;
                        continue;
                    }
                    Err(err) => {
                        record_worker_error(&worker_error_for_thread, &err);
                        tracing::warn!(
                            backend = "netcam",
                            stream = "ffmpeg",
                            error = %err,
                            "netcam ffmpeg fallback failed"
                        );
                    }
                }
            }
            tracing::debug!(
                backend = "netcam",
                backoff_ms = backoff.as_millis() as u64,
                "netcam retry backoff"
            );
            if sleep_until_netcam_stop(
                &stop_for_thread,
                backoff,
                Duration::from_millis(tunables.stop_poll_ms),
            ) {
                return;
            }
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
        #[cfg(all(feature = "netcam", feature = "async"))]
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            WorkerHandle::Async(handle.spawn(async_netcam_worker(
                AsyncNetcamWorker {
                    url,
                    width,
                    height,
                    fps,
                    tunables,
                    capture_tunables,
                    stop: Arc::clone(&stop),
                    worker_error: Arc::clone(&worker_error),
                },
                tx.clone(),
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
        stop_tx: Some(stop_tx),
        worker: Some(worker),
        aux_workers: vec![WorkerHandle::Thread(stop_watcher)],
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        #[cfg(feature = "libcamera")]
        libcamera_stop_when_idle: false,
        metrics: StageMetrics::default(),
        external_backings: Vec::new(),
        worker_error,
        control_error: std::sync::Arc::new(std::sync::Mutex::new(None)),
    })
}

#[cfg(all(feature = "netcam", feature = "async"))]
async fn async_netcam_worker(
    worker: AsyncNetcamWorker,
    tx: Arc<styx_core::queue::BoundedTx<FrameLease>>,
) {
    let AsyncNetcamWorker {
        url,
        width,
        height,
        fps,
        tunables,
        capture_tunables,
        stop,
        worker_error,
    } = worker;
    tracing::debug!(
        backend = "netcam",
        mode = "async",
        url = %url,
        width,
        height,
        fps,
        request_timeout_secs = tunables.request_timeout_secs,
        connect_timeout_ms = tunables.connect_timeout_ms,
        read_timeout_ms = tunables.read_timeout_ms,
        "starting netcam worker"
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(tunables.request_timeout_secs))
        .connect_timeout(Duration::from_millis(tunables.connect_timeout_ms))
        .read_timeout(Duration::from_millis(tunables.read_timeout_ms))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            let capture_err = CaptureError::Backend(format!("netcam client failed: {err}"));
            record_worker_error(&worker_error, &capture_err);
            tracing::warn!(backend = "netcam", error = %err, "failed to create netcam client");
            return;
        }
    };

    let start = Instant::now();
    let mut frame_idx: u64 = 0;
    let mut backoff = Duration::from_millis(tunables.backoff_start_ms);
    let mut consecutive_failures: u32 = 0;
    loop {
        if netcam_stopped(&stop) {
            tracing::debug!(backend = "netcam", "netcam worker stopped");
            return;
        }
        let response = tokio::select! {
            response = client.get(&url).send() => response,
            _ = async_wait_for_netcam_stop(
                &stop,
                Duration::from_millis(tunables.stop_poll_ms),
            ) => return,
        };
        match response {
            Ok(resp) => {
                let boundary = resp
                    .headers()
                    .get("content-type")
                    .and_then(|h| h.to_str().ok())
                    .and_then(parse_boundary);
                if let Some(boundary) = boundary {
                    tracing::debug!(
                        backend = "netcam",
                        stream = "mjpeg",
                        width,
                        height,
                        fps,
                        "connected"
                    );
                    let stream = resp.bytes_stream().map_err(std::io::Error::other);
                    let reader = StreamReader::new(stream);
                    let mut reader = tokio::io::BufReader::new(reader);
                    if async_mjpeg_loop(
                        &mut reader,
                        tx.as_ref(),
                        MjpegLoopContext {
                            boundary: &boundary,
                            width,
                            height,
                            fps,
                            start: &start,
                            frame_idx: &mut frame_idx,
                            stop: &stop,
                            capture_tunables,
                            netcam_tunables: tunables,
                        },
                    )
                    .await
                    {
                        return;
                    } else {
                        continue;
                    }
                } else {
                    tracing::debug!(
                        backend = "netcam",
                        "netcam response was not multipart mjpeg"
                    );
                }
            }
            Err(err) => {
                let capture_err = CaptureError::Backend(format!("netcam request failed: {err}"));
                record_worker_error(&worker_error, &capture_err);
                tracing::warn!(backend = "netcam", error = %err, "netcam request failed");
            }
        }
        #[cfg(feature = "netcam-video")]
        {
            match tokio::task::spawn_blocking({
                let url = url.clone();
                let tx = tx.clone();
                let stop = Arc::clone(&stop);
                let mut frame_idx = frame_idx;
                move || {
                    ffmpeg_loop(
                        &url,
                        &tx,
                        &start,
                        &mut frame_idx,
                        Arc::clone(&stop),
                        capture_tunables,
                        tunables,
                    )
                }
            })
            .await
            .unwrap_or(Err(CaptureError::Backend("ffmpeg join error".into())))
            {
                Ok(()) => {
                    consecutive_failures = 0;
                    continue;
                }
                Err(err) => {
                    record_worker_error(&worker_error, &err);
                    tracing::warn!(
                        backend = "netcam",
                        stream = "ffmpeg",
                        error = %err,
                        "netcam ffmpeg fallback failed"
                    );
                }
            }
        }
        tracing::debug!(
            backend = "netcam",
            backoff_ms = backoff.as_millis() as u64,
            "netcam retry backoff"
        );
        if async_sleep_until_netcam_stop(
            &stop,
            backoff,
            Duration::from_millis(tunables.stop_poll_ms),
        )
        .await
        {
            return;
        }
        consecutive_failures = consecutive_failures.saturating_add(1);
        backoff = (backoff * 2).min(Duration::from_millis(tunables.backoff_max_ms));
        if consecutive_failures >= 5 {
            backoff = Duration::from_millis(tunables.backoff_start_ms);
            consecutive_failures = 0;
        }
    }
}

pub(super) fn netcam_stopped(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire)
}

#[cfg(feature = "netcam")]
pub(super) fn sleep_until_netcam_stop(
    stop: &AtomicBool,
    duration: Duration,
    stop_poll: Duration,
) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if netcam_stopped(stop) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep((deadline - now).min(stop_poll));
    }
}

#[cfg(all(feature = "netcam", feature = "async"))]
pub(super) async fn async_sleep_until_netcam_stop(
    stop: &AtomicBool,
    duration: Duration,
    stop_poll: Duration,
) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if netcam_stopped(stop) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep((deadline - now).min(stop_poll)).await;
    }
}

#[cfg(all(feature = "netcam", feature = "async"))]
async fn async_wait_for_netcam_stop(stop: &AtomicBool, stop_poll: Duration) {
    loop {
        if netcam_stopped(stop) {
            return;
        }
        tokio::time::sleep(stop_poll).await;
    }
}

pub(super) fn enqueue_netcam_frame(
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    frame: FrameLease,
    stream: &'static str,
    timeout: Duration,
) -> bool {
    match tx.send_timeout(frame, timeout) {
        SendWaitOutcome::Ok => false,
        SendWaitOutcome::Closed(_frame) => {
            tracing::debug!(backend = "netcam", stream, "netcam output queue closed");
            true
        }
        SendWaitOutcome::Timeout(_frame) => {
            tracing::debug!(
                backend = "netcam",
                stream,
                drop_reason = "capture_queue_send_timeout",
                timeout_ms = timeout.as_millis() as u64,
                "dropping netcam frame because output queue is full"
            );
            false
        }
    }
}

#[cfg(all(feature = "netcam", feature = "async"))]
pub(super) async fn enqueue_netcam_frame_async(
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    frame: FrameLease,
    stream: &'static str,
    timeout: Duration,
) -> bool {
    match tokio::time::timeout(timeout, tx.send_async(frame)).await {
        Ok(SendOutcome::Ok) => false,
        Ok(SendOutcome::Closed) => {
            tracing::debug!(backend = "netcam", stream, "netcam output queue closed");
            true
        }
        Ok(SendOutcome::Full) => {
            tracing::debug!(
                backend = "netcam",
                stream,
                drop_reason = "capture_queue_send_full",
                "dropping netcam frame because output queue is full"
            );
            false
        }
        Err(_) => {
            tracing::debug!(
                backend = "netcam",
                stream,
                drop_reason = "capture_queue_send_timeout",
                timeout_ms = timeout.as_millis() as u64,
                "dropping netcam frame because output queue is full"
            );
            false
        }
    }
}

#[cfg(feature = "netcam-video")]
fn ffmpeg_loop(
    url: &str,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    start: &Instant,
    frame_idx: &mut u64,
    stop: Arc<AtomicBool>,
    capture_tunables: crate::capture_api::CaptureTunables,
    netcam_tunables: crate::capture_api::NetcamTunables,
) -> Result<(), CaptureError> {
    ffmpeg_next::init().map_err(|e| CaptureError::Backend(e.to_string()))?;
    #[cfg(target_os = "linux")]
    let mut pool: Option<SharedBufferPool> = None;
    #[cfg(not(target_os = "linux"))]
    let mut pool: Option<BufferPool> = None;
    loop {
        if netcam_stopped(&stop) {
            return Ok(());
        }
        let interrupt_stop = Arc::clone(&stop);
        let mut ictx =
            match format::input_with_interrupt(url, move || netcam_stopped(&interrupt_stop)) {
                Ok(ctx) => ctx,
                Err(e) => {
                    tracing::warn!(
                        backend = "netcam",
                        stream = "ffmpeg",
                        error = %e,
                        "failed to open netcam video stream"
                    );
                    return Err(CaptureError::Backend(e.to_string()));
                }
            };
        let stream_idx = match ictx.streams().best(StreamType::Video).map(|s| s.index()) {
            Some(idx) => idx,
            None => return Err(CaptureError::Backend("no video stream".into())),
        };
        let stream = ictx
            .stream(stream_idx)
            .ok_or_else(|| CaptureError::Backend("stream missing".into()))?;
        let mut decoder = open_preferred_video_decoder(&stream.parameters(), true)
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
        let pool_limits = capture_tunables.pool_limits(4, layout.len, 8);
        #[cfg(target_os = "linux")]
        let pool_ref = {
            if pool.is_none() {
                pool = Some(
                    SharedBufferPool::with_limits(
                        pool_limits.min,
                        pool_limits.bytes,
                        pool_limits.spare,
                    )
                    .map_err(|e| CaptureError::Backend(e.to_string()))?,
                );
            }
            pool.as_ref().unwrap()
        };
        #[cfg(not(target_os = "linux"))]
        let pool_ref = pool.get_or_insert_with(|| {
            BufferPool::with_limits(pool_limits.min, pool_limits.bytes, pool_limits.spare)
        });
        for (stream, packet) in ictx.packets() {
            if netcam_stopped(&stop) {
                return Ok(());
            }
            if stream.index() != stream_idx {
                continue;
            }
            if decoder.send_packet(&packet).is_err() {
                break;
            }
            while decoder.receive_frame(&mut decoded).is_ok() {
                if scaler.run(&decoded, &mut rgb).is_err() {
                    continue;
                }
                let ts = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                #[cfg(target_os = "linux")]
                let frame = match blit_shared_rgba_frame(&rgb, res, layout, pool_ref, ts) {
                    Ok(frame) => frame,
                    Err(_) => continue,
                };
                #[cfg(not(target_os = "linux"))]
                let frame = blit_rgba_frame(&rgb, res, layout, pool_ref, ts);
                *frame_idx = frame_idx.saturating_add(1);
                if enqueue_netcam_frame(
                    tx,
                    frame,
                    "ffmpeg",
                    Duration::from_millis(netcam_tunables.send_timeout_ms),
                ) {
                    return Ok(());
                }
            }
        }
        decoder.send_eof().ok();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if netcam_stopped(&stop) {
                return Ok(());
            }
            if scaler.run(&decoded, &mut rgb).is_err() {
                continue;
            }
            let ts = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            #[cfg(target_os = "linux")]
            let frame = match blit_shared_rgba_frame(&rgb, res, layout, pool_ref, ts) {
                Ok(frame) => frame,
                Err(_) => continue,
            };
            #[cfg(not(target_os = "linux"))]
            let frame = blit_rgba_frame(&rgb, res, layout, pool_ref, ts);
            *frame_idx = frame_idx.saturating_add(1);
            if enqueue_netcam_frame(
                tx,
                frame,
                "ffmpeg",
                Duration::from_millis(netcam_tunables.send_timeout_ms),
            ) {
                return Ok(());
            }
        }
        tracing::debug!(
            backend = "netcam",
            stream = "ffmpeg",
            "netcam video stream ended; reconnecting"
        );
        // Loop and reconnect on exit.
    }
}
