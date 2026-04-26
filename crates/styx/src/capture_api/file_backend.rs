mod file_controls;
mod file_media;

use std::collections::HashMap;
use std::fs;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use styx_core::prelude::*;

use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, WorkerHandle,
};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};

pub(crate) use file_controls::*;
#[cfg(target_os = "linux")]
use file_media::build_shared_frame_from_rgb;
pub(crate) use file_media::*;

pub(super) fn start_file(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    controls: Vec<(
        styx_core::controls::ControlId,
        styx_core::controls::ControlValue,
    )>,
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
        #[cfg(target_os = "linux")]
        let pool = match SharedBufferPool::with_limits(pool_min, pool_bytes, pool_spare) {
            Ok(pool) => pool,
            Err(_) => return,
        };
        #[cfg(not(target_os = "linux"))]
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
                        VideoDecodeOptions {
                            timestamp_ns,
                            fallback_frame_interval_ms: frame_delay_ms,
                            playback_speed: speed,
                            start_frame,
                            stop_frame,
                        },
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
                            #[cfg(target_os = "linux")]
                            let frame = match build_shared_frame_from_rgb(
                                rgb,
                                &mode_clone,
                                &pool,
                                timestamp_ns,
                            ) {
                                Ok(frame) => frame,
                                Err(_) => return,
                            };
                            #[cfg(not(target_os = "linux"))]
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
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        metrics: StageMetrics::default(),
        external_backings: Vec::new(),
    })
}
