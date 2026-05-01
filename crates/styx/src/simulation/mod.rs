use std::path::PathBuf;

use styx_capture::prelude::*;

pub(crate) mod backend;

use crate::{BackendHandle, BackendKind, DeviceIdentity, ProbedBackend, ProbedDevice};

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct SimulationPose {
    pub translation_m: [f32; 3],
    pub rotation_deg: [f32; 3],
}

impl Default for SimulationPose {
    fn default() -> Self {
        Self {
            translation_m: [0.0, 0.0, 3.0],
            rotation_deg: [0.0, 0.0, 0.0],
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct SimulationSensorConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub sensor_width_mm: f32,
    pub sensor_height_mm: f32,
    pub near_m: f32,
    pub far_m: f32,
}

impl Default for SimulationSensorConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            sensor_width_mm: 36.0,
            sensor_height_mm: 24.0,
            near_m: 0.05,
            far_m: 2_000.0,
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct SimulationLensConfig {
    pub focal_length_mm: f32,
    pub aperture_f_stop: f32,
    pub focus_distance_m: f32,
}

impl Default for SimulationLensConfig {
    fn default() -> Self {
        Self {
            focal_length_mm: 35.0,
            aperture_f_stop: 2.8,
            focus_distance_m: 5.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum SimulationOutputMode {
    #[default]
    Rgb,
    Depth,
    Normals,
    Segmentation,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct SimulationDeviceConfig {
    pub sensor: SimulationSensorConfig,
    pub lens: SimulationLensConfig,
    pub pose: SimulationPose,
    pub output_mode: SimulationOutputMode,
    pub clear_color_rgba: [f32; 4],
}

impl Default for SimulationDeviceConfig {
    fn default() -> Self {
        Self {
            sensor: SimulationSensorConfig::default(),
            lens: SimulationLensConfig::default(),
            pose: SimulationPose::default(),
            output_mode: SimulationOutputMode::Rgb,
            clear_color_rgba: [0.03, 0.04, 0.05, 1.0],
        }
    }
}

/// Create a synthetic simulation device that loads a scene file into a Bevy world.
pub fn make_simulation_device(
    name: &str,
    scene_path: PathBuf,
    config: SimulationDeviceConfig,
) -> ProbedDevice {
    let res = Resolution::new(config.sensor.width.max(1), config.sensor.height.max(1))
        .unwrap_or_else(|| Resolution::new(1, 1).unwrap());
    let interval = Interval::from_fps(config.sensor.fps.max(1)).expect("fps is clamped non-zero");
    let format = match config.output_mode {
        SimulationOutputMode::Depth => MediaFormat::new(FourCc::D32F, res, ColorSpace::Unknown),
        _ => MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb),
    };
    let mode = Mode::with_interval(format, interval);

    let controls = vec![
        ControlMeta {
            id: backend::control_id_output_mode(),
            name: "simulation.output.mode".into(),
            kind: ControlKind::Menu,
            access: Access::ReadWrite,
            min: ControlValue::Uint(0),
            max: ControlValue::Uint(3),
            default: ControlValue::Uint(match config.output_mode {
                SimulationOutputMode::Rgb => 0,
                SimulationOutputMode::Depth => 1,
                SimulationOutputMode::Normals => 2,
                SimulationOutputMode::Segmentation => 3,
            }),
            step: Some(ControlValue::Uint(1)),
            menu: Some(vec![
                "rgb".into(),
                "depth".into(),
                "normals".into(),
                "segmentation".into(),
            ]),
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_translation_x(),
            name: "simulation.sensor.translation_x_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-10_000.0),
            max: ControlValue::Float(10_000.0),
            default: ControlValue::Float(config.pose.translation_m[0]),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_translation_y(),
            name: "simulation.sensor.translation_y_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-10_000.0),
            max: ControlValue::Float(10_000.0),
            default: ControlValue::Float(config.pose.translation_m[1]),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_translation_z(),
            name: "simulation.sensor.translation_z_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-10_000.0),
            max: ControlValue::Float(10_000.0),
            default: ControlValue::Float(config.pose.translation_m[2]),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_rotation_roll(),
            name: "simulation.sensor.rotation_roll_deg".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-360.0),
            max: ControlValue::Float(360.0),
            default: ControlValue::Float(config.pose.rotation_deg[0]),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_rotation_pitch(),
            name: "simulation.sensor.rotation_pitch_deg".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-360.0),
            max: ControlValue::Float(360.0),
            default: ControlValue::Float(config.pose.rotation_deg[1]),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_rotation_yaw(),
            name: "simulation.sensor.rotation_yaw_deg".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(-360.0),
            max: ControlValue::Float(360.0),
            default: ControlValue::Float(config.pose.rotation_deg[2]),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_focal_length(),
            name: "simulation.lens.focal_length_mm".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(1.0),
            max: ControlValue::Float(5_000.0),
            default: ControlValue::Float(config.lens.focal_length_mm),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_aperture_f_stop(),
            name: "simulation.lens.aperture_f_stop".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.7),
            max: ControlValue::Float(64.0),
            default: ControlValue::Float(config.lens.aperture_f_stop),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_focus_distance(),
            name: "simulation.lens.focus_distance_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.01),
            max: ControlValue::Float(100_000.0),
            default: ControlValue::Float(config.lens.focus_distance_m),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_sensor_width(),
            name: "simulation.sensor.width_mm".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.1),
            max: ControlValue::Float(1_000.0),
            default: ControlValue::Float(config.sensor.sensor_width_mm),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_sensor_height(),
            name: "simulation.sensor.height_mm".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.1),
            max: ControlValue::Float(1_000.0),
            default: ControlValue::Float(config.sensor.sensor_height_mm),
            step: Some(ControlValue::Float(0.01)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_near_plane(),
            name: "simulation.sensor.near_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.001),
            max: ControlValue::Float(100.0),
            default: ControlValue::Float(config.sensor.near_m),
            step: Some(ControlValue::Float(0.001)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
        ControlMeta {
            id: backend::control_id_far_plane(),
            name: "simulation.sensor.far_m".into(),
            kind: ControlKind::Float,
            access: Access::ReadWrite,
            min: ControlValue::Float(0.01),
            max: ControlValue::Float(1_000_000.0),
            default: ControlValue::Float(config.sensor.far_m),
            step: Some(ControlValue::Float(0.1)),
            menu: None,
            metadata: ControlMetadata::default(),
        },
    ];

    let descriptor = CaptureDescriptor::new([mode]).with_controls(controls);
    let backend = ProbedBackend {
        kind: BackendKind::Simulation,
        handle: BackendHandle::Simulation {
            scene_path: scene_path.clone(),
            config: config.clone(),
        },
        descriptor,
        properties: vec![(
            "scene_path".into(),
            scene_path.to_string_lossy().to_string(),
        )],
    };
    ProbedDevice {
        identity: DeviceIdentity {
            display: name.to_string(),
            keys: vec![scene_path.to_string_lossy().to_string()],
        },
        backends: vec![backend],
    }
}
