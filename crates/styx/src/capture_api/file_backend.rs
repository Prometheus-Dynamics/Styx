use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use std::sync::{Arc, Mutex};

#[cfg(feature = "file-backend-video")]
use ffmpeg_next::{
    codec, format,
    frame::Video as FfFrame,
    media::Type as StreamType,
    software::scaling::{context::Context as ScalingContext, flag::Flags},
    util::format::pixel::Pixel as PixelFormat,
};
use std::num::NonZeroU32;
use styx_core::prelude::*;

#[cfg(feature = "file-backend-video")]
use crate::capture_api::ffmpeg_util::blit_rgb24_frame;
use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, WorkerHandle,
};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};
use styx_core::controls::{ControlId, ControlValue};

pub(super) fn start_file(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    descriptor: CaptureDescriptor,
) -> Result<CaptureHandle, CaptureError> {
    let (paths, fps, loop_forever) = match &backend.handle {
        BackendHandle::File {
            paths,
            fps,
            loop_forever,
        } => (paths.clone(), *fps, *loop_forever),
        _ => return Err(CaptureError::Backend("file list missing".into())),
    };
    if paths.is_empty() {
        return Err(CaptureError::Backend("no files provided".into()));
    }

    let (duration_override, image_fps, video_fps, start_ms, stop_ms) =
        parse_controls(&controls, fps);
    let control_state = Arc::new(Mutex::new(FileControlState {
        duration_override_ms: duration_override,
        image_fps: image_fps.max(1),
        video_fps,
        start_ms,
        stop_ms,
    }));
    let queue_depth = crate::capture_api::capture_queue_depth();
    let (tx, rx) = styx_core::queue::bounded(queue_depth);
    let interval = interval.unwrap_or_else(|| Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(fps.max(1)).unwrap(),
    });
    let mode_clone = mode.clone();
    let preloaded_bytes: HashMap<PathBuf, Vec<u8>> = paths
        .iter()
        .filter_map(|p| fs::read(p).ok().map(|bytes| (p.clone(), bytes)))
        .collect();
    let mut rgb_cache: HashMap<PathBuf, (Vec<u8>, Resolution)> = HashMap::new();
    for (path, bytes) in &preloaded_bytes {
        if let Ok((rgb, res)) = decode_rgb(path, Some(bytes.as_slice())) {
            rgb_cache.insert(path.clone(), (rgb, res));
        }
    }

    let worker_state = control_state.clone();
    let worker_fn = move || {
        let (pool_min, pool_bytes, pool_spare) = crate::capture_api::capture_pool_limits(
            4,
            (mode_clone.format.resolution.width.get()
                * mode_clone.format.resolution.height.get()
                * 4) as usize,
            8,
        );
        let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);
        let mut elapsed: u64 = 0;
        loop {
            for path in &paths {
                let (delay_ms, start_ms, stop_ms, video_fps, duration_override_ms) = match worker_state.lock() {
                    Ok(state) => (
                        effective_delay_ms(&state),
                        state.start_ms,
                        state.stop_ms,
                        state.video_fps,
                        state.duration_override_ms,
                    ),
                    Err(_) => (16, 0, 0, 0, None),
                };
                let image_delay = Duration::from_millis(delay_ms);
                if elapsed < start_ms as u64 {
                    elapsed = elapsed.saturating_add(delay_ms);
                    continue;
                }
                if stop_ms > 0 && elapsed >= stop_ms as u64 {
                    return;
                }
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    #[cfg(feature = "file-backend-video")]
                    if is_video_ext(ext) {
                        if decode_video(path, &tx, duration_override_ms, video_fps).is_ok() {
                            elapsed = elapsed.saturating_add(delay_ms);
                            thread::sleep(image_delay);
                            continue;
                        }
                    }
                }
                let timestamp = elapsed.saturating_mul(1_000_000);
                if let Some((rgb, res)) = rgb_cache.get(path) {
                    let frame = build_frame_from_rgb(rgb, *res, &mode_clone, &pool, timestamp);
                    if let SendOutcome::Closed = tx.send(frame) {
                        return;
                    }
                    thread::sleep(image_delay);
                    elapsed = elapsed.saturating_add(delay_ms);
                    continue;
                }
                let bytes = preloaded_bytes.get(path).map(|b| b.as_slice());
                if let Ok((rgb, res)) = decode_rgb(path, bytes) {
                    rgb_cache.insert(path.clone(), (rgb.clone(), res));
                    let frame = build_frame_from_rgb(&rgb, res, &mode_clone, &pool, timestamp);
                    if let SendOutcome::Closed = tx.send(frame) {
                        return;
                    }
                }
                thread::sleep(image_delay);
                elapsed = elapsed.saturating_add(delay_ms);
            }
            if !loop_forever {
                break;
            }
        }
    };

    let worker = {
        #[cfg(feature = "async")]
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            WorkerHandle::Async(handle.spawn_blocking(worker_fn))
        } else {
            WorkerHandle::Thread(thread::spawn(worker_fn))
        }
        #[cfg(not(feature = "async"))]
        {
            WorkerHandle::Thread(thread::spawn(worker_fn))
        }
    };

    Ok(CaptureHandle {
        backend: BackendKind::File,
        control: ControlPlane::File { state: control_state },
        descriptor,
        mode,
        interval: Some(interval),
        rx,
        stop_tx: None,
        worker: Some(worker),
        metrics: StageMetrics::default(),
    })
}

fn decode_rgb(
    path: &PathBuf,
    bytes: Option<&[u8]>,
) -> Result<(Vec<u8>, Resolution), CaptureError> {
    let owned;
    let data = if let Some(b) = bytes {
        b
    } else {
        owned = fs::read(path).map_err(|e| CaptureError::Backend(e.to_string()))?;
        owned.as_slice()
    };
    if is_jpeg_path(path) {
        if let Ok(result) = decode_jpeg_rgb(data) {
            return Ok(result);
        }
    }
    let img = image::load_from_memory(data).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    let res =
        Resolution::new(w, h).ok_or_else(|| CaptureError::Backend("invalid image dims".into()))?;
    Ok((rgb.into_raw(), res))
}

fn is_jpeg_path(path: &PathBuf) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg"),
        None => false,
    }
}

fn decode_jpeg_rgb(data: &[u8]) -> Result<(Vec<u8>, Resolution), CaptureError> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(data));
    let pixels = decoder.decode().map_err(|e| CaptureError::Backend(e.to_string()))?;
    let info = decoder.info().ok_or_else(|| CaptureError::Backend("jpeg metadata missing".into()))?;
    let res = Resolution::new(info.width as u32, info.height as u32)
        .ok_or_else(|| CaptureError::Backend("invalid jpeg dims".into()))?;
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => Ok((pixels, res)),
        jpeg_decoder::PixelFormat::L8 => {
            let mut rgb = Vec::with_capacity(pixels.len().saturating_mul(3));
            for &g in &pixels {
                rgb.push(g);
                rgb.push(g);
                rgb.push(g);
            }
            Ok((rgb, res))
        }
        other => Err(CaptureError::Backend(format!("unsupported jpeg pixel format: {other:?}"))),
    }
}

fn build_frame_from_rgb(
    rgb: &[u8],
    res: Resolution,
    mode: &Mode,
    pool: &BufferPool,
    timestamp: u64,
) -> FrameLease {
    let layout = plane_layout_from_dims(res.width, res.height, 3);
    let mut lease = pool.lease();
    lease.resize(layout.len);
    let dst = lease.as_mut_slice();
    let copy_len = dst.len().min(rgb.len());
    dst[..copy_len].copy_from_slice(&rgb[..copy_len]);
    FrameLease::single_plane(
        FrameMeta::new(
            MediaFormat::new(FourCc::new(*b"RG24"), res, mode.format.color),
            timestamp,
        ),
        lease,
        layout.len,
        layout.stride,
    )
}

#[cfg(feature = "file-backend-video")]
fn is_video_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "flv" | "ts" | "m2ts"
    )
}

#[cfg(feature = "file-backend-video")]
fn decode_video(
    path: &PathBuf,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    override_duration_ms: Option<u64>,
    override_fps: u32,
) -> Result<(), CaptureError> {
    ffmpeg_next::init().map_err(|e| CaptureError::Backend(e.to_string()))?;
    let mut ictx = format::input(&path).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let stream_idx = ictx
        .streams()
        .best(StreamType::Video)
        .ok_or_else(|| CaptureError::Backend("no video stream".into()))?
        .index();
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
    let layout = plane_layout_from_dims(res.width, res.height, 3);
    let (pool_min, pool_bytes, pool_spare) =
        crate::capture_api::capture_pool_limits(4, layout.len, 8);
    let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);
    let mut delay_ms = override_duration_ms.unwrap_or(0);
    let rate = stream.avg_frame_rate();
    if rate.numerator() > 0 && rate.denominator() > 0 {
        let src_ms = ((rate.denominator() as f64 / rate.numerator() as f64) * 1000.0)
            .max(1.0)
            .round() as u64;
        delay_ms = if override_fps > 0 {
            (1_000u64 / override_fps.max(1) as u64).max(1)
        } else {
            delay_ms.max(src_ms)
        };
    } else if delay_ms == 0 {
        delay_ms = 1_000u64 / override_fps.max(1) as u64;
    }
    if delay_ms == 0 {
        delay_ms = 16; // ~60fps fallback
    }
    let mut timestamp_ns: u64 = 0;

    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_idx {
            continue;
        }
        decoder
            .send_packet(&packet)
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            scaler
                .run(&decoded, &mut rgb)
                .map_err(|e| CaptureError::Backend(e.to_string()))?;
            let frame = blit_rgb24_frame(&rgb, res, layout, &pool, timestamp_ns);
            timestamp_ns = timestamp_ns.saturating_add(delay_ms.saturating_mul(1_000_000));
            if let SendOutcome::Closed = tx.send(frame) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    decoder.send_eof().ok();
    while decoder.receive_frame(&mut decoded).is_ok() {
        scaler.run(&decoded, &mut rgb).ok();
        let frame = blit_rgb24_frame(&rgb, res, layout, &pool, timestamp_ns);
        timestamp_ns = timestamp_ns.saturating_add(delay_ms.saturating_mul(1_000_000));
        if let SendOutcome::Closed = tx.send(frame) {
            return Ok(());
        }
    }
    Ok(())
}

// Custom file-backend control IDs.
pub const CTRL_FILE_DURATION_MS: ControlId = ControlId(0xF100);
pub const CTRL_FILE_START_MS: ControlId = ControlId(0xF101);
pub const CTRL_FILE_STOP_MS: ControlId = ControlId(0xF102);
pub const CTRL_FILE_IMAGE_FPS: ControlId = ControlId(0xF103);
pub const CTRL_FILE_VIDEO_FPS: ControlId = ControlId(0xF104);

#[derive(Debug, Clone)]
pub struct FileControlState {
    pub duration_override_ms: Option<u64>,
    pub image_fps: u32,
    pub video_fps: u32,
    pub start_ms: u32,
    pub stop_ms: u32,
}

pub(crate) type FileControlStateHandle = Arc<Mutex<FileControlState>>;

fn effective_delay_ms(state: &FileControlState) -> u64 {
    if let Some(ms) = state.duration_override_ms {
        return ms.max(1);
    }
    let fps = state.image_fps.max(1);
    (1_000u64 / fps as u64).max(1)
}

pub(crate) fn apply_file_control(
    state: &FileControlStateHandle,
    id: ControlId,
    value: ControlValue,
) -> Result<(), CaptureError> {
    let mut guard = state.lock().map_err(|_| CaptureError::ControlApply("file control lock poisoned".into()))?;
    match (id, value) {
        (CTRL_FILE_DURATION_MS, ControlValue::Uint(v)) => {
            guard.duration_override_ms = Some(v.max(1) as u64);
        }
        (CTRL_FILE_DURATION_MS, ControlValue::Int(v)) if v > 0 => {
            guard.duration_override_ms = Some(v as u64);
        }
        (CTRL_FILE_DURATION_MS, ControlValue::None) => {
            guard.duration_override_ms = None;
        }
        (CTRL_FILE_START_MS, ControlValue::Uint(v)) => guard.start_ms = v,
        (CTRL_FILE_START_MS, ControlValue::Int(v)) if v >= 0 => guard.start_ms = v as u32,
        (CTRL_FILE_STOP_MS, ControlValue::Uint(v)) => guard.stop_ms = v,
        (CTRL_FILE_STOP_MS, ControlValue::Int(v)) if v >= 0 => guard.stop_ms = v as u32,
        (CTRL_FILE_IMAGE_FPS, ControlValue::Uint(v)) => guard.image_fps = v.max(1),
        (CTRL_FILE_IMAGE_FPS, ControlValue::Int(v)) if v > 0 => guard.image_fps = v as u32,
        (CTRL_FILE_VIDEO_FPS, ControlValue::Uint(v)) => guard.video_fps = v,
        (CTRL_FILE_VIDEO_FPS, ControlValue::Int(v)) if v >= 0 => guard.video_fps = v as u32,
        _ => return Err(CaptureError::ControlUnsupported),
    }
    Ok(())
}

pub(crate) fn read_file_control(
    state: &FileControlStateHandle,
    id: ControlId,
) -> Result<ControlValue, CaptureError> {
    let guard = state.lock().map_err(|_| CaptureError::ControlApply("file control lock poisoned".into()))?;
    match id {
        CTRL_FILE_DURATION_MS => {
            let ms = guard.duration_override_ms.unwrap_or_else(|| effective_delay_ms(&guard));
            Ok(ControlValue::Uint(ms.min(u64::from(u32::MAX)) as u32))
        }
        CTRL_FILE_START_MS => Ok(ControlValue::Uint(guard.start_ms)),
        CTRL_FILE_STOP_MS => Ok(ControlValue::Uint(guard.stop_ms)),
        CTRL_FILE_IMAGE_FPS => Ok(ControlValue::Uint(guard.image_fps.max(1))),
        CTRL_FILE_VIDEO_FPS => Ok(ControlValue::Uint(guard.video_fps)),
        _ => Err(CaptureError::ControlUnsupported),
    }
}

fn parse_controls(
    controls: &[(ControlId, ControlValue)],
    default_fps: u32,
) -> (Option<u64>, u32, u32, u32, u32) {
    let mut duration_override: Option<u64> = None;
    let mut image_fps: u32 = 60;
    let mut video_fps: u32 = 0; // 0 = use source fps
    let mut start_ms: u32 = 0;
    let mut stop_ms: u32 = 0;
    for (id, val) in controls {
        match (*id, val) {
            (CTRL_FILE_DURATION_MS, ControlValue::Uint(v)) => {
                duration_override = Some((*v).max(1) as u64)
            }
            (CTRL_FILE_DURATION_MS, ControlValue::Int(v)) if *v > 0 => {
                duration_override = Some(*v as u64)
            }
            (CTRL_FILE_START_MS, ControlValue::Uint(v)) => start_ms = *v,
            (CTRL_FILE_START_MS, ControlValue::Int(v)) if *v >= 0 => start_ms = *v as u32,
            (CTRL_FILE_STOP_MS, ControlValue::Uint(v)) => stop_ms = *v,
            (CTRL_FILE_STOP_MS, ControlValue::Int(v)) if *v >= 0 => stop_ms = *v as u32,
            (CTRL_FILE_IMAGE_FPS, ControlValue::Uint(v)) => image_fps = (*v).max(1),
            (CTRL_FILE_IMAGE_FPS, ControlValue::Int(v)) if *v > 0 => image_fps = *v as u32,
            (CTRL_FILE_VIDEO_FPS, ControlValue::Uint(v)) => video_fps = *v,
            (CTRL_FILE_VIDEO_FPS, ControlValue::Int(v)) if *v >= 0 => video_fps = *v as u32,
            _ => {}
        }
    }
    // If no overrides provided, fall back to provided default fps.
    if duration_override.is_none() && image_fps == 0 {
        image_fps = default_fps.max(1);
    }
    (duration_override, image_fps, video_fps, start_ms, stop_ms)
}
