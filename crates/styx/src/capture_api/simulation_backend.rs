mod readback;
mod runtime;
mod state;
mod visualization;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use styx_core::controls::{ControlId, ControlValue};
use styx_core::prelude::*;

use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, WorkerHandle,
};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};

pub(crate) use state::{
    SimulationControlStateHandle, apply_simulation_control, control_id_aperture_f_stop,
    control_id_far_plane, control_id_focal_length, control_id_focus_distance,
    control_id_near_plane, control_id_output_mode, control_id_rotation_pitch,
    control_id_rotation_roll, control_id_rotation_yaw, control_id_sensor_height,
    control_id_sensor_width, control_id_translation_x, control_id_translation_y,
    control_id_translation_z, read_simulation_control,
};

use runtime::BevySimulationRuntime;
use state::{build_frame_from_depth, build_frame_from_rgb, interval_to_delay_ms, parse_controls};

pub(super) fn start_simulation(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    controls: Vec<(ControlId, ControlValue)>,
    descriptor: CaptureDescriptor,
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
    let queue_depth = crate::capture_api::capture_queue_depth();
    let (tx, rx) = styx_core::queue::bounded(queue_depth);
    let interval = interval.unwrap_or_else(|| Interval {
        numerator: std::num::NonZeroU32::new(1).unwrap(),
        denominator: std::num::NonZeroU32::new(config.sensor.fps.max(1)).unwrap(),
    });
    let frame_delay_ms = interval_to_delay_ms(interval);
    let mode_clone = mode.clone();
    let state_for_worker = state.clone();

    let worker_fn = move || {
        let output_res = mode_clone.format.resolution;
        let rgb_frame_len = (output_res.width.get() as usize)
            .saturating_mul(output_res.height.get() as usize)
            .saturating_mul(3);
        let depth_frame_len = (output_res.width.get() as usize)
            .saturating_mul(output_res.height.get() as usize)
            .saturating_mul(4);
        let (pool_min, pool_bytes, pool_spare) =
            crate::capture_api::capture_pool_limits(4, rgb_frame_len.max(depth_frame_len), 8);
        let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);
        let mut runtime = match BevySimulationRuntime::new(&scene_path, &config) {
            Ok(runtime) => runtime,
            Err(_) => return,
        };
        let mut timestamp_ns = 0u64;
        let mut latest_rgb = vec![0u8; rgb_frame_len];
        let mut latest_depth = vec![0u8; depth_frame_len];

        loop {
            let snapshot = match state_for_worker.lock() {
                Ok(guard) => guard.clone(),
                Err(_) => break,
            };
            runtime.sync_state(&snapshot);
            runtime.update();
            runtime.drain_latest(&snapshot, &mut latest_rgb, &mut latest_depth);

            let frame = match snapshot.output_mode {
                crate::capture_api::SimulationOutputMode::Depth => {
                    build_frame_from_depth(&latest_depth, output_res, &pool, timestamp_ns)
                }
                _ => build_frame_from_rgb(&latest_rgb, &mode_clone, &pool, timestamp_ns),
            };
            if let SendOutcome::Closed = tx.send(frame) {
                return;
            }
            timestamp_ns = timestamp_ns.saturating_add(frame_delay_ms.saturating_mul(1_000_000));
            thread::sleep(Duration::from_millis(frame_delay_ms));
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
        backend: BackendKind::Simulation,
        control: ControlPlane::Simulation { state },
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
