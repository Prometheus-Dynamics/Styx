use std::fs;
use std::path::{Path, PathBuf};
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
use styx_core::prelude::*;

use crate::capture_api::CaptureError;
#[cfg(feature = "file-backend-video")]
use crate::capture_api::ffmpeg_util::{blit_rgb24_frame, open_preferred_video_decoder};
use crate::prelude::{Interval, Mode};

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

fn is_video_path(path: &Path) -> bool {
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
fn probe_video_metadata(path: &Path) -> Result<(Resolution, Option<u32>), CaptureError> {
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

pub(crate) fn interval_to_delay_ms(interval: Interval) -> u64 {
    let num = u64::from(interval.numerator.get());
    let den = u64::from(interval.denominator.get()).max(1);
    ((1_000u64.saturating_mul(num)).saturating_add(den / 2) / den).max(1)
}

pub(crate) fn decode_rgb(
    path: &Path,
    bytes: Option<&[u8]>,
) -> Result<(Vec<u8>, Resolution), CaptureError> {
    let owned;
    let data = if let Some(b) = bytes {
        b
    } else {
        owned = fs::read(path).map_err(|e| CaptureError::Backend(e.to_string()))?;
        owned.as_slice()
    };
    if is_jpeg_path(path)
        && let Ok(result) = decode_jpeg_rgb(data)
    {
        return Ok(result);
    }
    let img = image::load_from_memory(data).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    let res =
        Resolution::new(w, h).ok_or_else(|| CaptureError::Backend("invalid image dims".into()))?;
    Ok((rgb.into_raw(), res))
}

fn is_jpeg_path(path: &Path) -> bool {
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

pub(crate) fn rgb24_to_mode(
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

pub(crate) fn build_frame_from_rgb(
    rgb: &[u8],
    mode: &Mode,
    pool: &BufferPool,
    timestamp: u64,
) -> FrameLease {
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
        )
        .with_capture_instant(std::time::Instant::now())
        .with_transition(ResidencyTransition {
            from: FrameResidency::HostOwned,
            to: FrameResidency::HostOwned,
            reason: ResidencyTransitionReason::FileReplay,
            copied: false,
        }),
        lease,
        layout.len,
        layout.stride,
    )
}

#[cfg(feature = "file-backend-video")]
pub(crate) struct VideoDecodeOptions {
    pub timestamp_ns: u64,
    pub fallback_frame_interval_ms: u64,
    pub playback_speed: f32,
    pub start_frame: u32,
    pub stop_frame: u32,
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn decode_video(
    path: &Path,
    tx: &styx_core::queue::BoundedTx<FrameLease>,
    mode: &Mode,
    options: VideoDecodeOptions,
) -> Result<VideoDecodeResult, CaptureError> {
    let VideoDecodeOptions {
        mut timestamp_ns,
        fallback_frame_interval_ms,
        playback_speed,
        start_frame,
        stop_frame,
    } = options;
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
        thread::sleep(Duration::from_millis(delay_ms));
        timestamp_ns = timestamp_ns.saturating_add(delay_ms.saturating_mul(1_000_000));
    }

    Ok(VideoDecodeResult::Advanced(timestamp_ns))
}

#[cfg(feature = "file-backend-video")]
pub(crate) enum VideoDecodeResult {
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
