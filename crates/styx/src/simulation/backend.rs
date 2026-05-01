mod readback;
mod runtime;
mod state;
mod visualization;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use styx_core::controls::{ControlId, ControlValue};
use styx_core::prelude::*;

use crate::capture_api::handle::enqueue_capture_frame;
use crate::capture_api::{CaptureError, CaptureHandle, ControlPlane, StyxConfig, WorkerHandle};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};
use styx_capture::CaptureDescriptor;

pub(crate) use state::{
    SimulationControlStateHandle, apply_simulation_control, control_id_aperture_f_stop,
    control_id_far_plane, control_id_focal_length, control_id_focus_distance,
    control_id_near_plane, control_id_output_mode, control_id_rotation_pitch,
    control_id_rotation_roll, control_id_rotation_yaw, control_id_sensor_height,
    control_id_sensor_width, control_id_translation_x, control_id_translation_y,
    control_id_translation_z, read_simulation_control,
};

use runtime::BevySimulationRuntime;
#[cfg(not(target_os = "linux"))]
use state::{build_frame_from_depth, build_frame_from_rgb};
#[cfg(target_os = "linux")]
use state::{build_shared_frame_from_depth, build_shared_frame_from_rgb};
use state::{interval_to_delay_ms, parse_controls};

pub(crate) fn start_simulation(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    descriptor: CaptureDescriptor,
    runtime_config: &StyxConfig,
) -> Result<CaptureHandle, CaptureError> {
    let (scene_path, config) = match &backend.handle {
        BackendHandle::Simulation { scene_path, config } => (scene_path.clone(), config.clone()),
        _ => return Err(CaptureError::Backend("simulation scene missing".into())),
    };
    if !scene_path.exists() {
        return Err(CaptureError::Backend(format!(
            "simulation scene missing: {}",
            scene_path.display()
        )));
    }
    if scene_path.parent().is_none() {
        return Err(CaptureError::Backend(
            "simulation scene has no parent directory".into(),
        ));
    }

    let state = Arc::new(Mutex::new(parse_controls(&config, &controls)));
    let capture_tunables = runtime_config.capture_tunables();
    let queue_depth = capture_tunables.queue_depth;
    let (tx, rx) = styx_core::queue::bounded(queue_depth);
    let (stop_tx, stop_rx) = mpsc::channel();
    let interval = interval.unwrap_or_else(|| Interval {
        numerator: std::num::NonZeroU32::new(1).unwrap(),
        denominator: std::num::NonZeroU32::new(config.sensor.fps.max(1)).unwrap(),
    });
    let frame_delay_ms = interval_to_delay_ms(interval);
    let queue_send_timeout = Duration::from_millis(capture_tunables.queue_send_timeout_ms);
    let mode_clone = mode.clone();
    let state_for_worker = state.clone();

    let worker_fn = move || {
        tracing::debug!(backend = "simulation", "capture worker started");
        let output_res = mode_clone.format.resolution;
        let rgb_frame_len = (output_res.width.get() as usize)
            .saturating_mul(output_res.height.get() as usize)
            .saturating_mul(3);
        let depth_frame_len = (output_res.width.get() as usize)
            .saturating_mul(output_res.height.get() as usize)
            .saturating_mul(4);
        let pool_limits = capture_tunables.pool_limits(4, rgb_frame_len.max(depth_frame_len), 8);
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
        let mut runtime = match BevySimulationRuntime::new(&scene_path, &config) {
            Ok(runtime) => runtime,
            Err(_) => return,
        };
        let mut timestamp_ns = 0u64;
        let mut latest_rgb = vec![0u8; rgb_frame_len];
        let mut latest_depth = vec![0u8; depth_frame_len];

        loop {
            if simulation_stop_requested(&stop_rx, Duration::ZERO) {
                break;
            }
            let snapshot = match state_for_worker.lock() {
                Ok(guard) => guard.clone(),
                Err(_) => break,
            };
            runtime.sync_state(&snapshot);
            runtime.update();
            runtime.drain_latest(&snapshot, &mut latest_rgb, &mut latest_depth);

            let frame = match snapshot.output_mode {
                crate::simulation::SimulationOutputMode::Depth => {
                    #[cfg(target_os = "linux")]
                    {
                        match build_shared_frame_from_depth(
                            &latest_depth,
                            output_res,
                            &pool,
                            timestamp_ns,
                        ) {
                            Ok(frame) => frame,
                            Err(_) => break,
                        }
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        build_frame_from_depth(&latest_depth, output_res, &pool, timestamp_ns)
                    }
                }
                _ => {
                    #[cfg(target_os = "linux")]
                    {
                        match build_shared_frame_from_rgb(
                            &latest_rgb,
                            &mode_clone,
                            &pool,
                            timestamp_ns,
                        ) {
                            Ok(frame) => frame,
                            Err(_) => break,
                        }
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        build_frame_from_rgb(&latest_rgb, &mode_clone, &pool, timestamp_ns)
                    }
                }
            };
            if enqueue_capture_frame(&tx, frame, "simulation", queue_send_timeout) {
                return;
            }
            timestamp_ns = timestamp_ns.saturating_add(frame_delay_ms.saturating_mul(1_000_000));
            if simulation_stop_requested(&stop_rx, Duration::from_millis(frame_delay_ms)) {
                break;
            }
        }
        tracing::debug!(backend = "simulation", "capture worker stopped");
    };

    let worker = WorkerHandle::Thread(thread::spawn(worker_fn));

    Ok(CaptureHandle {
        backend: BackendKind::Simulation,
        control: ControlPlane::Simulation { state },
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

fn simulation_stop_requested(stop_rx: &mpsc::Receiver<()>, wait: Duration) -> bool {
    if wait.is_zero() {
        stop_rx.try_recv().is_ok()
    } else {
        stop_rx.recv_timeout(wait).is_ok()
    }
}
