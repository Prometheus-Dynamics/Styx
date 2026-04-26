use std::sync::{Arc, Mutex};

use styx_core::controls::{ControlId, ControlValue};
use styx_core::prelude::*;

use crate::capture_api::{CaptureError, SimulationDeviceConfig, SimulationOutputMode};
use crate::prelude::{Interval, Mode};

const CTRL_SIM_TRANSLATION_X: ControlId = ControlId(0xF300_0000);
const CTRL_SIM_TRANSLATION_Y: ControlId = ControlId(0xF300_0001);
const CTRL_SIM_TRANSLATION_Z: ControlId = ControlId(0xF300_0002);
const CTRL_SIM_ROTATION_ROLL: ControlId = ControlId(0xF300_0003);
const CTRL_SIM_ROTATION_PITCH: ControlId = ControlId(0xF300_0004);
const CTRL_SIM_ROTATION_YAW: ControlId = ControlId(0xF300_0005);
const CTRL_SIM_FOCAL_LENGTH: ControlId = ControlId(0xF300_0006);
const CTRL_SIM_APERTURE_F_STOP: ControlId = ControlId(0xF300_0007);
const CTRL_SIM_FOCUS_DISTANCE: ControlId = ControlId(0xF300_0008);
const CTRL_SIM_SENSOR_WIDTH: ControlId = ControlId(0xF300_0009);
const CTRL_SIM_SENSOR_HEIGHT: ControlId = ControlId(0xF300_000A);
const CTRL_SIM_NEAR_PLANE: ControlId = ControlId(0xF300_000B);
const CTRL_SIM_FAR_PLANE: ControlId = ControlId(0xF300_000C);
const CTRL_SIM_OUTPUT_MODE: ControlId = ControlId(0xF300_000D);

pub(crate) fn control_id_translation_x() -> ControlId {
    CTRL_SIM_TRANSLATION_X
}
pub(crate) fn control_id_translation_y() -> ControlId {
    CTRL_SIM_TRANSLATION_Y
}
pub(crate) fn control_id_translation_z() -> ControlId {
    CTRL_SIM_TRANSLATION_Z
}
pub(crate) fn control_id_rotation_roll() -> ControlId {
    CTRL_SIM_ROTATION_ROLL
}
pub(crate) fn control_id_rotation_pitch() -> ControlId {
    CTRL_SIM_ROTATION_PITCH
}
pub(crate) fn control_id_rotation_yaw() -> ControlId {
    CTRL_SIM_ROTATION_YAW
}
pub(crate) fn control_id_focal_length() -> ControlId {
    CTRL_SIM_FOCAL_LENGTH
}
pub(crate) fn control_id_aperture_f_stop() -> ControlId {
    CTRL_SIM_APERTURE_F_STOP
}
pub(crate) fn control_id_focus_distance() -> ControlId {
    CTRL_SIM_FOCUS_DISTANCE
}
pub(crate) fn control_id_sensor_width() -> ControlId {
    CTRL_SIM_SENSOR_WIDTH
}
pub(crate) fn control_id_sensor_height() -> ControlId {
    CTRL_SIM_SENSOR_HEIGHT
}
pub(crate) fn control_id_near_plane() -> ControlId {
    CTRL_SIM_NEAR_PLANE
}
pub(crate) fn control_id_far_plane() -> ControlId {
    CTRL_SIM_FAR_PLANE
}
pub(crate) fn control_id_output_mode() -> ControlId {
    CTRL_SIM_OUTPUT_MODE
}

#[derive(Debug, Clone)]
pub struct SimulationControlState {
    pub output_mode: SimulationOutputMode,
    pub translation_m: [f32; 3],
    pub rotation_deg: [f32; 3],
    pub focal_length_mm: f32,
    pub aperture_f_stop: f32,
    pub focus_distance_m: f32,
    pub sensor_width_mm: f32,
    pub sensor_height_mm: f32,
    pub near_m: f32,
    pub far_m: f32,
}

pub(crate) type SimulationControlStateHandle = Arc<Mutex<SimulationControlState>>;

pub(crate) fn apply_simulation_control(
    state: &SimulationControlStateHandle,
    id: ControlId,
    value: ControlValue,
) -> Result<(), CaptureError> {
    let mut guard = state
        .lock()
        .map_err(|_| CaptureError::control_apply("simulation control lock poisoned"))?;
    apply_control_to_state(&mut guard, id, value)
}

pub(crate) fn read_simulation_control(
    state: &SimulationControlStateHandle,
    id: ControlId,
) -> Result<ControlValue, CaptureError> {
    let guard = state
        .lock()
        .map_err(|_| CaptureError::control_apply("simulation control lock poisoned"))?;
    match id {
        CTRL_SIM_OUTPUT_MODE => Ok(ControlValue::Uint(match guard.output_mode {
            SimulationOutputMode::Rgb => 0,
            SimulationOutputMode::Depth => 1,
            SimulationOutputMode::Normals => 2,
            SimulationOutputMode::Segmentation => 3,
        })),
        CTRL_SIM_TRANSLATION_X => Ok(ControlValue::Float(guard.translation_m[0])),
        CTRL_SIM_TRANSLATION_Y => Ok(ControlValue::Float(guard.translation_m[1])),
        CTRL_SIM_TRANSLATION_Z => Ok(ControlValue::Float(guard.translation_m[2])),
        CTRL_SIM_ROTATION_ROLL => Ok(ControlValue::Float(guard.rotation_deg[0])),
        CTRL_SIM_ROTATION_PITCH => Ok(ControlValue::Float(guard.rotation_deg[1])),
        CTRL_SIM_ROTATION_YAW => Ok(ControlValue::Float(guard.rotation_deg[2])),
        CTRL_SIM_FOCAL_LENGTH => Ok(ControlValue::Float(guard.focal_length_mm)),
        CTRL_SIM_APERTURE_F_STOP => Ok(ControlValue::Float(guard.aperture_f_stop)),
        CTRL_SIM_FOCUS_DISTANCE => Ok(ControlValue::Float(guard.focus_distance_m)),
        CTRL_SIM_SENSOR_WIDTH => Ok(ControlValue::Float(guard.sensor_width_mm)),
        CTRL_SIM_SENSOR_HEIGHT => Ok(ControlValue::Float(guard.sensor_height_mm)),
        CTRL_SIM_NEAR_PLANE => Ok(ControlValue::Float(guard.near_m)),
        CTRL_SIM_FAR_PLANE => Ok(ControlValue::Float(guard.far_m)),
        _ => Err(CaptureError::ControlUnsupported),
    }
}

pub(super) fn parse_controls(
    config: &SimulationDeviceConfig,
    controls: &[(ControlId, ControlValue)],
) -> SimulationControlState {
    let mut state = SimulationControlState {
        output_mode: config.output_mode,
        translation_m: config.pose.translation_m,
        rotation_deg: config.pose.rotation_deg,
        focal_length_mm: config.lens.focal_length_mm,
        aperture_f_stop: config.lens.aperture_f_stop,
        focus_distance_m: config.lens.focus_distance_m,
        sensor_width_mm: config.sensor.sensor_width_mm,
        sensor_height_mm: config.sensor.sensor_height_mm,
        near_m: config.sensor.near_m,
        far_m: config.sensor.far_m,
    };
    for (id, value) in controls {
        let _ = apply_control_to_state(&mut state, *id, value.clone());
    }
    state
}

fn apply_control_to_state(
    state: &mut SimulationControlState,
    id: ControlId,
    value: ControlValue,
) -> Result<(), CaptureError> {
    if id == CTRL_SIM_OUTPUT_MODE {
        state.output_mode = match value {
            ControlValue::Uint(0) | ControlValue::Int(0) => SimulationOutputMode::Rgb,
            ControlValue::Uint(1) | ControlValue::Int(1) => SimulationOutputMode::Depth,
            ControlValue::Uint(2) | ControlValue::Int(2) => SimulationOutputMode::Normals,
            ControlValue::Uint(3) | ControlValue::Int(3) => SimulationOutputMode::Segmentation,
            _ => return Err(CaptureError::ControlUnsupported),
        };
        return Ok(());
    }

    let float = match value {
        ControlValue::Float(v) => v,
        ControlValue::Int(v) => v as f32,
        ControlValue::Uint(v) => v as f32,
        _ => return Err(CaptureError::ControlUnsupported),
    };
    match id {
        CTRL_SIM_TRANSLATION_X => state.translation_m[0] = float,
        CTRL_SIM_TRANSLATION_Y => state.translation_m[1] = float,
        CTRL_SIM_TRANSLATION_Z => state.translation_m[2] = float,
        CTRL_SIM_ROTATION_ROLL => state.rotation_deg[0] = float,
        CTRL_SIM_ROTATION_PITCH => state.rotation_deg[1] = float,
        CTRL_SIM_ROTATION_YAW => state.rotation_deg[2] = float,
        CTRL_SIM_FOCAL_LENGTH => state.focal_length_mm = float.max(1.0),
        CTRL_SIM_APERTURE_F_STOP => state.aperture_f_stop = float.max(0.7),
        CTRL_SIM_FOCUS_DISTANCE => state.focus_distance_m = float.max(0.01),
        CTRL_SIM_SENSOR_WIDTH => state.sensor_width_mm = float.max(0.1),
        CTRL_SIM_SENSOR_HEIGHT => state.sensor_height_mm = float.max(0.1),
        CTRL_SIM_NEAR_PLANE => state.near_m = float.max(0.001),
        CTRL_SIM_FAR_PLANE => state.far_m = float.max(state.near_m + 0.001),
        _ => return Err(CaptureError::ControlUnsupported),
    }
    Ok(())
}

pub(super) fn interval_to_delay_ms(interval: Interval) -> u64 {
    let num = u64::from(interval.numerator.get());
    let den = u64::from(interval.denominator.get()).max(1);
    ((1_000u64.saturating_mul(num)).saturating_add(den / 2) / den).max(1)
}

pub(super) fn build_frame_from_rgb(
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
            MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb),
            timestamp,
        )
        .with_capture_instant(std::time::Instant::now())
        .with_transition(ResidencyTransition {
            from: FrameResidency::HostOwned,
            to: FrameResidency::HostOwned,
            reason: ResidencyTransitionReason::Capture,
            copied: false,
        }),
        lease,
        layout.len,
        layout.stride,
    )
}

pub(super) fn build_frame_from_depth(
    depth: &[u8],
    resolution: Resolution,
    pool: &BufferPool,
    timestamp: u64,
) -> FrameLease {
    let layout = plane_layout_from_dims(resolution.width, resolution.height, 4);
    let mut lease = pool.lease();
    lease.resize(layout.len);
    let dst = lease.as_mut_slice();
    let copy_len = dst.len().min(depth.len());
    dst[..copy_len].copy_from_slice(&depth[..copy_len]);
    FrameLease::single_plane(
        FrameMeta::new(
            MediaFormat::new(FourCc::new(*b"D32F"), resolution, ColorSpace::Unknown),
            timestamp,
        )
        .with_capture_instant(std::time::Instant::now())
        .with_transition(ResidencyTransition {
            from: FrameResidency::HostOwned,
            to: FrameResidency::HostOwned,
            reason: ResidencyTransitionReason::Capture,
            copied: false,
        }),
        lease,
        layout.len,
        layout.stride,
    )
}

pub(super) fn rgba_to_rgb(rgba: &[u8], width: u32, height: u32, out: &mut Vec<u8>) {
    let pixel_count = (width as usize).saturating_mul(height as usize);
    let needed = pixel_count.saturating_mul(3);
    if out.len() != needed {
        out.resize(needed, 0);
    }
    for (src, dst) in rgba.chunks_exact(4).zip(out.chunks_exact_mut(3)) {
        dst[0] = src[0];
        dst[1] = src[1];
        dst[2] = src[2];
    }
}
