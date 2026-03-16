use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(feature = "file-backend-video")]
use ffmpeg_next::{
    format,
    frame::Video as FfFrame,
    media::Type as StreamType,
    software::scaling::{context::Context as ScalingContext, flag::Flags},
    util::format::pixel::Pixel as PixelFormat,
};
use std::num::NonZeroU32;
use styx_core::prelude::*;

#[cfg(feature = "file-backend-video")]
use crate::capture_api::ffmpeg_util::{blit_rgb24_frame, open_preferred_video_decoder};
use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, WorkerHandle,
};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};
use styx_core::controls::{ControlId, ControlValue};

const CTRL_FILE_VIDEO_PLAYBACK_SPEED_BASE: u32 = 0xF200_0000;
const CTRL_FILE_VIDEO_START_FRAME_BASE: u32 = 0xF210_0000;
const CTRL_FILE_VIDEO_STOP_FRAME_BASE: u32 = 0xF220_0000;
const CTRL_FILE_IMAGE_DURATION_FRAMES_BASE: u32 = 0xF230_0000;
const CTRL_FILE_CONTROL_INDEX_LIMIT: u32 = 0x0001_0000;

fn make_indexed_control_id(base: u32, index: usize) -> ControlId {
    let idx = u32::try_from(index)
        .unwrap_or(u32::MAX)
        .min(CTRL_FILE_CONTROL_INDEX_LIMIT.saturating_sub(1));
    ControlId(base.saturating_add(idx))
}

fn decode_indexed_control_id(id: ControlId, base: u32) -> Option<usize> {
    let end = base.saturating_add(CTRL_FILE_CONTROL_INDEX_LIMIT);
    if id.0 >= base && id.0 < end {
        return usize::try_from(id.0 - base).ok();
    }
    None
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_id_file_video_playback_speed(index: usize) -> ControlId {
    make_indexed_control_id(CTRL_FILE_VIDEO_PLAYBACK_SPEED_BASE, index)
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_id_file_video_start_frame(index: usize) -> ControlId {
    make_indexed_control_id(CTRL_FILE_VIDEO_START_FRAME_BASE, index)
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_id_file_video_stop_frame(index: usize) -> ControlId {
    make_indexed_control_id(CTRL_FILE_VIDEO_STOP_FRAME_BASE, index)
}

pub(crate) fn control_id_file_image_duration_frames(index: usize) -> ControlId {
    make_indexed_control_id(CTRL_FILE_IMAGE_DURATION_FRAMES_BASE, index)
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_name_file_video_playback_speed(name: &str) -> String {
    format!("file.video.{name}.playback_speed")
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_name_file_video_start_frame(name: &str) -> String {
    format!("file.video.{name}.start_frame")
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_name_file_video_stop_frame(name: &str) -> String {
    format!("file.video.{name}.stop_frame")
}

pub(crate) fn control_name_file_image_duration_frames(name: &str) -> String {
    format!("file.image.{name}.duration_frames")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileMediaKind {
    Image,
    #[cfg(feature = "file-backend-video")]
    Video,
    Unknown,
}

#[derive(Debug, Clone)]
pub(crate) struct FileMediaInfo {
    pub name: String,
    pub kind: FileMediaKind,
    pub resolution: Option<Resolution>,
    #[cfg(feature = "file-backend-video")]
    pub frame_count: Option<u32>,
}

pub(crate) fn inspect_file_media(path: &PathBuf) -> FileMediaInfo {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    if is_video_path(path) {
        #[cfg(feature = "file-backend-video")]
        {
            if let Ok((res, frame_count)) = probe_video_metadata(path) {
                return FileMediaInfo {
                    name,
                    kind: FileMediaKind::Video,
                    resolution: Some(res),
                    frame_count,
                };
            }
        }

        return FileMediaInfo {
            name,
            kind: FileMediaKind::Unknown,
            resolution: None,
            #[cfg(feature = "file-backend-video")]
            frame_count: None,
        };
    }

    if let Ok((w, h)) = image::image_dimensions(path)
        && let Some(res) = Resolution::new(w, h)
    {
        return FileMediaInfo {
            name,
            kind: FileMediaKind::Image,
            resolution: Some(res),
            #[cfg(feature = "file-backend-video")]
            frame_count: None,
        };
    }

    FileMediaInfo {
        name,
        kind: FileMediaKind::Unknown,
        resolution: None,
        #[cfg(feature = "file-backend-video")]
        frame_count: None,
    }
}

fn is_video_path(path: &PathBuf) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => is_video_ext(ext),
        None => false,
    }
}

fn is_video_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "flv" | "ts" | "m2ts"
    )
}

#[cfg(feature = "file-backend-video")]
fn probe_video_metadata(path: &PathBuf) -> Result<(Resolution, Option<u32>), CaptureError> {
    ffmpeg_next::init().map_err(|e| CaptureError::Backend(e.to_string()))?;
    let ictx = format::input(path).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let stream = ictx
        .streams()
        .best(StreamType::Video)
        .ok_or_else(|| CaptureError::Backend("no video stream".into()))?;

    let decoder = open_preferred_video_decoder(&stream.parameters(), false)
        .map_err(|e| CaptureError::Backend(e.to_string()))?;

    let res = Resolution::new(decoder.width() as u32, decoder.height() as u32)
        .ok_or_else(|| CaptureError::Backend("invalid video resolution".into()))?;

    let frame_count = stream
        .frames()
        .try_into()
        .ok()
        .filter(|count: &u32| *count > 0);

    Ok((res, frame_count))
}

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

    let media_infos: Vec<FileMediaInfo> = paths.iter().map(inspect_file_media).collect();

    let mut image_slot_by_path: Vec<Option<usize>> = vec![None; paths.len()];
    #[cfg(feature = "file-backend-video")]
    let mut video_slot_by_path: Vec<Option<usize>> = vec![None; paths.len()];
    let mut image_count = 0usize;
    #[cfg(feature = "file-backend-video")]
    let mut video_count = 0usize;
    for (idx, info) in media_infos.iter().enumerate() {
        match info.kind {
            FileMediaKind::Image => {
                image_slot_by_path[idx] = Some(image_count);
                image_count = image_count.saturating_add(1);
            }
            #[cfg(feature = "file-backend-video")]
            FileMediaKind::Video => {
                video_slot_by_path[idx] = Some(video_count);
                video_count = video_count.saturating_add(1);
            }
            FileMediaKind::Unknown => {}
        }
    }

    #[cfg(feature = "file-backend-video")]
    let mut video_frame_max: Vec<Option<u32>> = Vec::with_capacity(video_count);
    #[cfg(feature = "file-backend-video")]
    for (idx, slot) in video_slot_by_path.iter().enumerate() {
        if slot.is_some() {
            video_frame_max.push(
                media_infos[idx]
                    .frame_count
                    .map(|count| count.saturating_sub(1)),
            );
        }
    }

    let control_state = Arc::new(Mutex::new(parse_controls(&controls, image_count, {
        #[cfg(feature = "file-backend-video")]
        {
            video_frame_max
        }
        #[cfg(not(feature = "file-backend-video"))]
        {
            Vec::new()
        }
    })));
    let queue_depth = crate::capture_api::capture_queue_depth();
    let (tx, rx) = styx_core::queue::bounded(queue_depth);
    let interval = interval.unwrap_or_else(|| Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(fps.max(1)).unwrap(),
    });
    let frame_delay_ms = interval_to_delay_ms(interval);
    let mode_clone = mode.clone();

    let preloaded_bytes: HashMap<PathBuf, Vec<u8>> = paths
        .iter()
        .filter_map(|p| fs::read(p).ok().map(|bytes| (p.clone(), bytes)))
        .collect();
    let mut rgb_cache: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    for (idx, path) in paths.iter().enumerate() {
        if image_slot_by_path[idx].is_none() {
            continue;
        }
        let bytes = preloaded_bytes.get(path).map(|b| b.as_slice());
        if let Ok((rgb, src_res)) = decode_rgb(path, bytes)
            && let Ok(rgb_mode) = rgb24_to_mode(&rgb, src_res, mode_clone.format.resolution)
        {
            rgb_cache.insert(path.clone(), rgb_mode);
        }
    }

    let worker_state = control_state.clone();
    let worker_fn = move || {
        let output_res = mode_clone.format.resolution;
        let (pool_min, pool_bytes, pool_spare) = crate::capture_api::capture_pool_limits(
            4,
            (output_res.width.get() * output_res.height.get() * 3) as usize,
            8,
        );
        let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);
        let mut timestamp_ns: u64 = 0;

        loop {
            for (path_idx, path) in paths.iter().enumerate() {
                #[cfg(feature = "file-backend-video")]
                if let Some(video_idx) = video_slot_by_path[path_idx] {
                    let (speed, start_frame, stop_frame) = match worker_state.lock() {
                        Ok(state) => (
                            state
                                .video_playback_speed
                                .get(video_idx)
                                .copied()
                                .unwrap_or(1.0),
                            state.video_start_frame.get(video_idx).copied().unwrap_or(0),
                            state.video_stop_frame.get(video_idx).copied().unwrap_or(0),
                        ),
                        Err(_) => (1.0, 0, 0),
                    };
                    if let Ok(result) = decode_video(
                        path,
                        &tx,
                        &mode_clone,
                        timestamp_ns,
                        frame_delay_ms,
                        speed,
                        start_frame,
                        stop_frame,
                    ) {
                        match result {
                            VideoDecodeResult::Advanced(next_ts) => {
                                timestamp_ns = next_ts;
                                continue;
                            }
                            VideoDecodeResult::QueueClosed => return,
                        }
                    }
                }

                if let Some(image_idx) = image_slot_by_path[path_idx] {
                    let duration_frames = match worker_state.lock() {
                        Ok(state) => state
                            .image_duration_frames
                            .get(image_idx)
                            .copied()
                            .unwrap_or(1)
                            .max(1),
                        Err(_) => 1,
                    };

                    if !rgb_cache.contains_key(path) {
                        let bytes = preloaded_bytes.get(path).map(|b| b.as_slice());
                        if let Ok((rgb, src_res)) = decode_rgb(path, bytes)
                            && let Ok(rgb_mode) = rgb24_to_mode(&rgb, src_res, output_res)
                        {
                            rgb_cache.insert(path.clone(), rgb_mode);
                        }
                    }

                    if let Some(rgb) = rgb_cache.get(path) {
                        for _ in 0..duration_frames {
                            let frame = build_frame_from_rgb(rgb, &mode_clone, &pool, timestamp_ns);
                            if let SendOutcome::Closed = tx.send(frame) {
                                return;
                            }
                            timestamp_ns = timestamp_ns
                                .saturating_add(frame_delay_ms.saturating_mul(1_000_000));
                            thread::sleep(Duration::from_millis(frame_delay_ms));
                        }
                        continue;
                    }
                }

                // Unknown/unreadable file: back off one frame interval to avoid a tight loop.
                thread::sleep(Duration::from_millis(frame_delay_ms));
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
        control: ControlPlane::File {
            state: control_state,
        },
        descriptor,
        mode,
        interval: Some(interval),
        rx,
        stop_tx: None,
        worker: Some(worker),
        metrics: StageMetrics::default(),
        external_backings: Vec::new(),
    })
}

fn interval_to_delay_ms(interval: Interval) -> u64 {
    let num = u64::from(interval.numerator.get());
    let den = u64::from(interval.denominator.get()).max(1);
    ((1_000u64.saturating_mul(num)).saturating_add(den / 2) / den).max(1)
}

fn decode_rgb(path: &PathBuf, bytes: Option<&[u8]>) -> Result<(Vec<u8>, Resolution), CaptureError> {
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
    let pixels = decoder
        .decode()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let info = decoder
        .info()
        .ok_or_else(|| CaptureError::Backend("jpeg metadata missing".into()))?;
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
        other => Err(CaptureError::Backend(format!(
            "unsupported jpeg pixel format: {other:?}"
        ))),
    }
}

fn rgb24_to_mode(
    rgb: &[u8],
    source_res: Resolution,
    target_res: Resolution,
) -> Result<Vec<u8>, CaptureError> {
    if source_res == target_res {
        return Ok(rgb.to_vec());
    }

    let src = image::RgbImage::from_raw(
        source_res.width.get(),
        source_res.height.get(),
        rgb.to_vec(),
    )
    .ok_or_else(|| CaptureError::Backend("invalid source RGB buffer".into()))?;
    let resized = image::imageops::resize(
        &src,
        target_res.width.get(),
        target_res.height.get(),
        image::imageops::FilterType::Triangle,
    );
    Ok(resized.into_raw())
}

fn build_frame_from_rgb(rgb: &[u8], mode: &Mode, pool: &BufferPool, timestamp: u64) -> FrameLease {
    let res = mode.format.resolution;
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
fn decode_video(
    path: &PathBuf,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    mode: &Mode,
    mut timestamp_ns: u64,
    fallback_frame_interval_ms: u64,
    playback_speed: f32,
    start_frame: u32,
    stop_frame: u32,
) -> Result<VideoDecodeResult, CaptureError> {
    ffmpeg_next::init().map_err(|e| CaptureError::Backend(e.to_string()))?;
    let mut ictx = format::input(path).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let stream_idx = ictx
        .streams()
        .best(StreamType::Video)
        .ok_or_else(|| CaptureError::Backend("no video stream".into()))?
        .index();
    let stream = ictx
        .stream(stream_idx)
        .ok_or_else(|| CaptureError::Backend("stream missing".into()))?;

    let mut decoder = open_preferred_video_decoder(&stream.parameters(), false)
        .map_err(|e| CaptureError::Backend(e.to_string()))?;

    let output_res = mode.format.resolution;
    let mut scaler = ScalingContext::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        PixelFormat::RGBA,
        output_res.width.get(),
        output_res.height.get(),
        Flags::BILINEAR,
    )
    .map_err(|e| CaptureError::Backend(e.to_string()))?;

    let mut decoded = FfFrame::empty();
    let mut rgb = FfFrame::empty();
    rgb.set_format(PixelFormat::RGBA);
    rgb.set_width(output_res.width.get());
    rgb.set_height(output_res.height.get());
    unsafe {
        rgb.alloc(
            PixelFormat::RGBA,
            output_res.width.get(),
            output_res.height.get(),
        );
    }

    let layout = plane_layout_from_dims(output_res.width, output_res.height, 3);
    let (pool_min, pool_bytes, pool_spare) =
        crate::capture_api::capture_pool_limits(4, layout.len, 8);
    let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);

    let rate = stream.avg_frame_rate();
    let src_delay_ms = if rate.numerator() > 0 && rate.denominator() > 0 {
        ((rate.denominator() as f64 / rate.numerator() as f64) * 1000.0)
            .max(1.0)
            .round() as u64
    } else {
        fallback_frame_interval_ms.max(1)
    };

    let speed = playback_speed.clamp(0.05, 16.0) as f64;
    let delay_ms = ((src_delay_ms as f64) / speed).round() as u64;
    let delay_ms = delay_ms.max(1);

    let mut frame_index: u32 = 0;
    let mut emitted_frames: u32 = 0;

    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_idx {
            continue;
        }
        decoder
            .send_packet(&packet)
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            match push_video_frame(
                &decoded,
                &mut scaler,
                &mut rgb,
                tx,
                output_res,
                layout,
                &pool,
                &mut timestamp_ns,
                delay_ms,
                &mut frame_index,
                &mut emitted_frames,
                start_frame,
                stop_frame,
            )? {
                VideoFramePushResult::Continue => {}
                VideoFramePushResult::StopFrame => {
                    return Ok(VideoDecodeResult::Advanced(timestamp_ns));
                }
                VideoFramePushResult::QueueClosed => return Ok(VideoDecodeResult::QueueClosed),
            }
        }
    }

    decoder.send_eof().ok();
    while decoder.receive_frame(&mut decoded).is_ok() {
        match push_video_frame(
            &decoded,
            &mut scaler,
            &mut rgb,
            tx,
            output_res,
            layout,
            &pool,
            &mut timestamp_ns,
            delay_ms,
            &mut frame_index,
            &mut emitted_frames,
            start_frame,
            stop_frame,
        )? {
            VideoFramePushResult::Continue => {}
            VideoFramePushResult::StopFrame => {
                return Ok(VideoDecodeResult::Advanced(timestamp_ns));
            }
            VideoFramePushResult::QueueClosed => return Ok(VideoDecodeResult::QueueClosed),
        }
    }

    if emitted_frames == 0 {
        // Invalid/empty ranges can produce zero output frames; avoid a tight decode spin.
        thread::sleep(Duration::from_millis(delay_ms));
        timestamp_ns = timestamp_ns.saturating_add(delay_ms.saturating_mul(1_000_000));
    }

    Ok(VideoDecodeResult::Advanced(timestamp_ns))
}

#[cfg(feature = "file-backend-video")]
enum VideoDecodeResult {
    Advanced(u64),
    QueueClosed,
}

#[cfg(feature = "file-backend-video")]
enum VideoFramePushResult {
    Continue,
    StopFrame,
    QueueClosed,
}

#[cfg(feature = "file-backend-video")]
#[allow(clippy::too_many_arguments)]
fn push_video_frame(
    decoded: &FfFrame,
    scaler: &mut ScalingContext,
    rgb: &mut FfFrame,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    output_res: Resolution,
    layout: PlaneLayout,
    pool: &BufferPool,
    timestamp_ns: &mut u64,
    delay_ms: u64,
    frame_index: &mut u32,
    emitted_frames: &mut u32,
    start_frame: u32,
    stop_frame: u32,
) -> Result<VideoFramePushResult, CaptureError> {
    if stop_frame > 0 && *frame_index > stop_frame {
        return Ok(VideoFramePushResult::StopFrame);
    }

    scaler
        .run(decoded, rgb)
        .map_err(|e| CaptureError::Backend(e.to_string()))?;

    if *frame_index >= start_frame {
        let frame = blit_rgb24_frame(rgb, output_res, layout, pool, *timestamp_ns);
        if let SendOutcome::Closed = tx.send(frame) {
            return Ok(VideoFramePushResult::QueueClosed);
        }
        *emitted_frames = emitted_frames.saturating_add(1);
        *timestamp_ns = timestamp_ns.saturating_add(delay_ms.saturating_mul(1_000_000));
        thread::sleep(Duration::from_millis(delay_ms));
    }

    *frame_index = frame_index.saturating_add(1);
    if stop_frame > 0 && *frame_index > stop_frame {
        Ok(VideoFramePushResult::StopFrame)
    } else {
        Ok(VideoFramePushResult::Continue)
    }
}

#[derive(Debug, Clone)]
pub struct FileControlState {
    pub image_duration_frames: Vec<u32>,
    pub video_playback_speed: Vec<f32>,
    pub video_start_frame: Vec<u32>,
    pub video_stop_frame: Vec<u32>,
    pub video_frame_max: Vec<Option<u32>>,
}

pub(crate) type FileControlStateHandle = Arc<Mutex<FileControlState>>;

pub(crate) fn apply_file_control(
    state: &FileControlStateHandle,
    id: ControlId,
    value: ControlValue,
) -> Result<(), CaptureError> {
    let mut guard = state
        .lock()
        .map_err(|_| CaptureError::control_apply("file control lock poisoned"))?;
    apply_control_to_state(&mut guard, id, value)
}

pub(crate) fn read_file_control(
    state: &FileControlStateHandle,
    id: ControlId,
) -> Result<ControlValue, CaptureError> {
    let guard = state
        .lock()
        .map_err(|_| CaptureError::control_apply("file control lock poisoned"))?;

    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_PLAYBACK_SPEED_BASE) {
        return guard
            .video_playback_speed
            .get(index)
            .copied()
            .map(ControlValue::Float)
            .ok_or(CaptureError::ControlUnsupported);
    }
    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_START_FRAME_BASE) {
        return guard
            .video_start_frame
            .get(index)
            .copied()
            .map(ControlValue::Uint)
            .ok_or(CaptureError::ControlUnsupported);
    }
    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_STOP_FRAME_BASE) {
        return guard
            .video_stop_frame
            .get(index)
            .copied()
            .map(ControlValue::Uint)
            .ok_or(CaptureError::ControlUnsupported);
    }
    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_IMAGE_DURATION_FRAMES_BASE) {
        return guard
            .image_duration_frames
            .get(index)
            .copied()
            .map(ControlValue::Uint)
            .ok_or(CaptureError::ControlUnsupported);
    }

    Err(CaptureError::ControlUnsupported)
}

fn parse_controls(
    controls: &[(ControlId, ControlValue)],
    image_count: usize,
    video_frame_max: Vec<Option<u32>>,
) -> FileControlState {
    let video_count = video_frame_max.len();
    let mut state = FileControlState {
        image_duration_frames: vec![1; image_count],
        video_playback_speed: vec![1.0; video_count],
        video_start_frame: vec![0; video_count],
        video_stop_frame: vec![0; video_count],
        video_frame_max,
    };

    for (id, val) in controls {
        let _ = apply_control_to_state(&mut state, *id, val.clone());
    }

    state
}

fn apply_control_to_state(
    state: &mut FileControlState,
    id: ControlId,
    value: ControlValue,
) -> Result<(), CaptureError> {
    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_PLAYBACK_SPEED_BASE) {
        let slot = state
            .video_playback_speed
            .get_mut(index)
            .ok_or(CaptureError::ControlUnsupported)?;
        *slot = match value {
            ControlValue::Float(v) if v > 0.0 => v,
            ControlValue::Uint(v) if v > 0 => v as f32,
            ControlValue::Int(v) if v > 0 => v as f32,
            _ => return Err(CaptureError::ControlUnsupported),
        };
        return Ok(());
    }

    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_START_FRAME_BASE) {
        let frame_max = state.video_frame_max.get(index).copied().flatten();
        let slot = state
            .video_start_frame
            .get_mut(index)
            .ok_or(CaptureError::ControlUnsupported)?;
        let mut next = match value {
            ControlValue::Uint(v) => v,
            ControlValue::Int(v) if v >= 0 => v as u32,
            _ => return Err(CaptureError::ControlUnsupported),
        };
        if let Some(max) = frame_max {
            next = next.min(max);
        }
        *slot = next;
        if let Some(stop_slot) = state.video_stop_frame.get_mut(index) {
            if let Some(max) = frame_max {
                *stop_slot = (*stop_slot).min(max);
            }
            if *slot > *stop_slot {
                *stop_slot = *slot;
            }
        }
        return Ok(());
    }

    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_STOP_FRAME_BASE) {
        let frame_max = state.video_frame_max.get(index).copied().flatten();
        let slot = state
            .video_stop_frame
            .get_mut(index)
            .ok_or(CaptureError::ControlUnsupported)?;
        let mut next = match value {
            ControlValue::Uint(v) => v,
            ControlValue::Int(v) if v >= 0 => v as u32,
            _ => return Err(CaptureError::ControlUnsupported),
        };
        if let Some(max) = frame_max {
            next = next.min(max);
        }
        *slot = next;
        if let Some(start_slot) = state.video_start_frame.get_mut(index) {
            if let Some(max) = frame_max {
                *start_slot = (*start_slot).min(max);
            }
            if *slot < *start_slot {
                *start_slot = *slot;
            }
        }
        return Ok(());
    }

    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_IMAGE_DURATION_FRAMES_BASE) {
        let slot = state
            .image_duration_frames
            .get_mut(index)
            .ok_or(CaptureError::ControlUnsupported)?;
        *slot = match value {
            ControlValue::Uint(v) if v > 0 => v,
            ControlValue::Int(v) if v > 0 => v as u32,
            _ => return Err(CaptureError::ControlUnsupported),
        };
        return Ok(());
    }

    Err(CaptureError::ControlUnsupported)
}

#[cfg(all(test, feature = "file-backend-video"))]
mod tests {
    use super::*;

    fn state_with_one_video() -> FileControlState {
        FileControlState {
            image_duration_frames: vec![],
            video_playback_speed: vec![1.0],
            video_start_frame: vec![0],
            video_stop_frame: vec![0],
            video_frame_max: vec![None],
        }
    }

    #[test]
    fn start_frame_updates_stop_when_crossing() {
        let mut state = state_with_one_video();
        apply_control_to_state(
            &mut state,
            control_id_file_video_stop_frame(0),
            ControlValue::Uint(100),
        )
        .expect("set stop");
        apply_control_to_state(
            &mut state,
            control_id_file_video_start_frame(0),
            ControlValue::Uint(150),
        )
        .expect("set start");

        assert_eq!(state.video_start_frame[0], 150);
        assert_eq!(state.video_stop_frame[0], 150);
    }

    #[test]
    fn stop_frame_updates_start_when_crossing() {
        let mut state = state_with_one_video();
        apply_control_to_state(
            &mut state,
            control_id_file_video_start_frame(0),
            ControlValue::Uint(200),
        )
        .expect("set start");
        apply_control_to_state(
            &mut state,
            control_id_file_video_stop_frame(0),
            ControlValue::Uint(120),
        )
        .expect("set stop");

        assert_eq!(state.video_start_frame[0], 120);
        assert_eq!(state.video_stop_frame[0], 120);
    }

    #[test]
    fn frame_window_clamps_to_known_max() {
        let mut state = FileControlState {
            image_duration_frames: vec![],
            video_playback_speed: vec![1.0],
            video_start_frame: vec![0],
            video_stop_frame: vec![0],
            video_frame_max: vec![Some(42)],
        };

        apply_control_to_state(
            &mut state,
            control_id_file_video_start_frame(0),
            ControlValue::Uint(1000),
        )
        .expect("set start");
        apply_control_to_state(
            &mut state,
            control_id_file_video_stop_frame(0),
            ControlValue::Uint(2000),
        )
        .expect("set stop");

        assert_eq!(state.video_start_frame[0], 42);
        assert_eq!(state.video_stop_frame[0], 42);
    }
}
