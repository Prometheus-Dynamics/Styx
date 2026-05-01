mod file_controls;
mod file_media;

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use styx_core::prelude::*;

use super::handle::enqueue_capture_frame;
use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, StyxConfig, WorkerHandle,
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
    config: &StyxConfig,
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
    let capture_tunables = config.capture_tunables();
    let queue_depth = capture_tunables.queue_depth;
    let (tx, rx) = styx_core::queue::bounded(queue_depth);
    let (stop_tx, stop_rx) = mpsc::channel();
    let interval = interval.unwrap_or_else(|| Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(fps.max(1)).unwrap(),
    });
    let frame_delay_ms = interval_to_delay_ms(interval);
    let queue_send_timeout = Duration::from_millis(capture_tunables.queue_send_timeout_ms);
    let mode_clone = mode.clone();

    let mut rgb_cache: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let mut rgb_cache_order: VecDeque<PathBuf> = VecDeque::new();
    let mut rgb_cache_bytes = 0usize;
    let rgb_cache_limit = capture_tunables.file_image_cache_bytes;

    let worker_state = control_state.clone();
    let worker_fn = move || {
        tracing::debug!(backend = "file", "capture worker started");
        let output_res = mode_clone.format.resolution;
        let pool_limits = capture_tunables.pool_limits(
            4,
            (output_res.width.get() * output_res.height.get() * 3) as usize,
            8,
        );
        #[cfg(target_os = "linux")]
        let pool = match SharedBufferPool::with_limits(
            pool_limits.min,
            pool_limits.bytes,
            pool_limits.spare,
        ) {
            Ok(pool) => pool,
            Err(_) => return,
        };
        #[cfg(not(target_os = "linux"))]
        let pool = BufferPool::with_limits(pool_limits.min, pool_limits.bytes, pool_limits.spare);
        let mut timestamp_ns: u64 = 0;

        loop {
            for (path_idx, path) in paths.iter().enumerate() {
                if file_stop_requested(&stop_rx, Duration::ZERO) {
                    tracing::debug!(backend = "file", "capture worker stopped");
                    return;
                }
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
                            capture_tunables,
                        },
                        &stop_rx,
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

                    let mut uncached_rgb = None;
                    if !rgb_cache.contains_key(path)
                        && let Ok((rgb, src_res)) = decode_rgb(path, None)
                        && let Ok(rgb_mode) = rgb24_to_mode(&rgb, src_res, output_res)
                    {
                        if rgb_cache_limit == 0 {
                            uncached_rgb = Some(rgb_mode);
                        } else {
                            insert_rgb_cache(
                                &mut rgb_cache,
                                &mut rgb_cache_order,
                                &mut rgb_cache_bytes,
                                rgb_cache_limit,
                                path.clone(),
                                rgb_mode,
                            );
                        }
                    }

                    if let Some(rgb) = rgb_cache.get(path).or(uncached_rgb.as_ref()) {
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
                            if enqueue_capture_frame(&tx, frame, "file", queue_send_timeout) {
                                return;
                            }
                            timestamp_ns = timestamp_ns
                                .saturating_add(frame_delay_ms.saturating_mul(1_000_000));
                            if file_stop_requested(&stop_rx, Duration::from_millis(frame_delay_ms))
                            {
                                tracing::debug!(backend = "file", "capture worker stopped");
                                return;
                            }
                        }
                        continue;
                    }
                }

                if file_stop_requested(&stop_rx, Duration::from_millis(frame_delay_ms)) {
                    tracing::debug!(backend = "file", "capture worker stopped");
                    return;
                }
            }

            if !loop_forever {
                break;
            }
        }
        tracing::debug!(backend = "file", "capture worker stopped");
    };

    let worker = WorkerHandle::Thread(thread::spawn(worker_fn));

    Ok(CaptureHandle {
        backend: BackendKind::File,
        control: ControlPlane::File {
            state: control_state,
        },
        descriptor,
        mode,
        interval: Some(interval),
        rx,
        stop_tx: Some(stop_tx),
        worker: Some(worker),
        aux_workers: Vec::new(),
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        #[cfg(feature = "libcamera")]
        libcamera_stop_when_idle: false,
        metrics: StageMetrics::default(),
        external_backings: Vec::new(),
        worker_error: std::sync::Arc::new(std::sync::Mutex::new(None)),
        control_error: std::sync::Arc::new(std::sync::Mutex::new(None)),
    })
}

fn file_stop_requested(stop_rx: &mpsc::Receiver<()>, wait: Duration) -> bool {
    if wait.is_zero() {
        stop_rx.try_recv().is_ok()
    } else {
        stop_rx.recv_timeout(wait).is_ok()
    }
}

fn insert_rgb_cache(
    cache: &mut HashMap<PathBuf, Vec<u8>>,
    order: &mut VecDeque<PathBuf>,
    bytes: &mut usize,
    limit: usize,
    path: PathBuf,
    rgb: Vec<u8>,
) {
    let rgb_len = rgb.len();
    if rgb_len > limit {
        return;
    }
    if let Some(old) = cache.remove(&path) {
        *bytes = bytes.saturating_sub(old.len());
        order.retain(|queued| queued != &path);
    }
    while bytes.saturating_add(rgb_len) > limit {
        let Some(old_path) = order.pop_front() else {
            break;
        };
        if let Some(old) = cache.remove(&old_path) {
            *bytes = bytes.saturating_sub(old.len());
        }
    }
    *bytes = bytes.saturating_add(rgb_len);
    order.push_back(path.clone());
    cache.insert(path, rgb);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_cache_evicts_to_configured_limit() {
        let mut cache = HashMap::new();
        let mut order = VecDeque::new();
        let mut bytes = 0usize;

        insert_rgb_cache(
            &mut cache,
            &mut order,
            &mut bytes,
            6,
            PathBuf::from("a.png"),
            vec![1; 4],
        );
        insert_rgb_cache(
            &mut cache,
            &mut order,
            &mut bytes,
            6,
            PathBuf::from("b.png"),
            vec![2; 4],
        );

        assert!(!cache.contains_key(&PathBuf::from("a.png")));
        assert!(cache.contains_key(&PathBuf::from("b.png")));
        assert_eq!(bytes, 4);
    }
}
