use std::f32::consts::PI;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use bevy::app::App;
use bevy::asset::AssetPlugin;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::image::{Image, TextureFormatPixelInfo};
use bevy::prelude::*;
use bevy::render::camera::{Exposure, PhysicalCameraParameters, RenderTarget, Viewport};
use bevy::render::render_asset::{RenderAssetUsages, RenderAssets};
use bevy::render::render_graph::{self, RenderGraph, RenderGraphContext, RenderLabel};
use bevy::render::render_resource::{
    Buffer, BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Extent3d, ImageCopyBuffer,
    ImageDataLayout, Maintain, MapMode, TextureDimension, TextureFormat, TextureUsages,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue};
use bevy::render::texture::GpuImage;
use bevy::render::{Extract, ExtractSchedule, Render, RenderApp, RenderSet};
use bevy::window::{ExitCondition, WindowPlugin};
use bevy::winit::WinitPlugin;
use crossbeam_channel::{Receiver, Sender};
use styx_core::controls::{ControlId, ControlValue};
use styx_core::prelude::*;

use crate::capture_api::{
    CaptureDescriptor, CaptureError, CaptureHandle, ControlPlane, SimulationDeviceConfig,
    SimulationOutputMode, WorkerHandle,
};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};
use crate::{BackendHandle, BackendKind, ProbedBackend};

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

#[derive(Resource, Deref)]
struct MainWorldReceiver(Receiver<Vec<u8>>);

#[derive(Resource, Deref)]
struct RenderWorldSender(Sender<Vec<u8>>);

#[derive(Clone, Default, Resource, Deref, DerefMut)]
struct ImageCopiers(Vec<ImageCopier>);

#[derive(Clone, Component)]
struct ImageCopier {
    buffer: Buffer,
    src_image: Handle<Image>,
}

impl ImageCopier {
    fn new(src_image: Handle<Image>, size: Extent3d, render_device: &RenderDevice) -> Self {
        let padded_bytes_per_row = RenderDevice::align_copy_bytes_per_row(size.width as usize) * 4;
        let buffer = render_device.create_buffer(&BufferDescriptor {
            label: None,
            size: padded_bytes_per_row as u64 * size.height as u64,
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { buffer, src_image }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, RenderLabel)]
struct ImageCopyNodeLabel;

#[derive(Default)]
struct ImageCopyNode;

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
        return Err(CaptureError::Backend("simulation scene has no parent directory".into()));
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
        let frame_len = (output_res.width.get() as usize)
            .saturating_mul(output_res.height.get() as usize)
            .saturating_mul(3);
        let (pool_min, pool_bytes, pool_spare) =
            crate::capture_api::capture_pool_limits(4, frame_len, 8);
        let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);
        let mut runtime = match BevySimulationRuntime::new(&scene_path, &config) {
            Ok(runtime) => runtime,
            Err(_) => return,
        };
        let mut timestamp_ns = 0u64;
        let mut latest_rgb = vec![0u8; frame_len];

        loop {
            let snapshot = match state_for_worker.lock() {
                Ok(guard) => guard.clone(),
                Err(_) => break,
            };
            runtime.sync_state(&snapshot);
            runtime.update();
            if let Some(rgba) = runtime.drain_latest_rgba() {
                rgba_to_rgb(&rgba, output_res.width.get(), output_res.height.get(), &mut latest_rgb);
                apply_output_mode(
                    &mut latest_rgb,
                    output_res.width.get(),
                    output_res.height.get(),
                    snapshot.output_mode,
                );
            }

            let frame = build_frame_from_rgb(&latest_rgb, &mode_clone, &pool, timestamp_ns);
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

fn parse_controls(
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
    match id {
        CTRL_SIM_OUTPUT_MODE => {
            state.output_mode = match value {
                ControlValue::Uint(0) | ControlValue::Int(0) => SimulationOutputMode::Rgb,
                ControlValue::Uint(1) | ControlValue::Int(1) => SimulationOutputMode::Depth,
                ControlValue::Uint(2) | ControlValue::Int(2) => SimulationOutputMode::Normals,
                ControlValue::Uint(3) | ControlValue::Int(3) => SimulationOutputMode::Segmentation,
                _ => return Err(CaptureError::ControlUnsupported),
            };
            return Ok(());
        }
        _ => {}
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

fn interval_to_delay_ms(interval: Interval) -> u64 {
    let num = u64::from(interval.numerator.get());
    let den = u64::from(interval.denominator.get()).max(1);
    ((1_000u64.saturating_mul(num)).saturating_add(den / 2) / den).max(1)
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

fn rgba_to_rgb(rgba: &[u8], width: u32, height: u32, out: &mut Vec<u8>) {
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

struct BevySimulationRuntime {
    app: App,
    sensor_entity: Entity,
    output_width: u32,
    output_height: u32,
}

impl BevySimulationRuntime {
    fn new(scene_path: &Path, config: &SimulationDeviceConfig) -> Result<Self, CaptureError> {
        let asset_root = scene_path
            .parent()
            .ok_or_else(|| CaptureError::Backend("simulation scene has no parent directory".into()))?;
        let scene_asset_path = scene_asset_path(scene_path)?;
        let render_width = config.sensor.width.max(1);
        let render_height = config.sensor.height.max(1);

        let mut app = App::new();
        app.insert_resource(ClearColor(Color::srgba(
            config.clear_color_rgba[0],
            config.clear_color_rgba[1],
            config.clear_color_rgba[2],
            config.clear_color_rgba[3],
        )));
        app.add_plugins(
            DefaultPlugins
                .set(AssetPlugin {
                    file_path: asset_root.to_string_lossy().to_string(),
                    ..default()
                })
                .set(ImagePlugin::default_nearest())
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: ExitCondition::DontExit,
                    ..default()
                })
                .disable::<WinitPlugin>(),
        );
        app.add_plugins(ImageCopyPlugin);
        app.finish();
        app.cleanup();

        let size = Extent3d {
            width: render_width,
            height: render_height,
            ..default()
        };
        let image_handle = {
            let mut images = app.world_mut().resource_mut::<Assets<Image>>();
            let mut render_target_image = Image::new_fill(
                size,
                TextureDimension::D2,
                &[0; 4],
                TextureFormat::bevy_default(),
                RenderAssetUsages::default(),
            );
            render_target_image.texture_descriptor.usage |=
                TextureUsages::COPY_SRC
                    | TextureUsages::RENDER_ATTACHMENT
                    | TextureUsages::TEXTURE_BINDING;
            images.add(render_target_image)
        };

        let image_copier = {
            let render_device = app.sub_app(RenderApp).world().resource::<RenderDevice>();
            ImageCopier::new(image_handle.clone(), size, &render_device)
        };
        app.world_mut().spawn(image_copier);

        let scene_handle = {
            let asset_server = app.world().resource::<AssetServer>();
            asset_server.load(scene_asset_path)
        };
        app.world_mut().spawn((
            Name::new("simulation_scene"),
            SceneRoot(scene_handle),
            Transform::default(),
            GlobalTransform::default(),
        ));

        app.world_mut().insert_resource(AmbientLight {
            color: Color::srgb(0.8, 0.82, 0.85),
            brightness: 500.0,
        });
        app.world_mut().spawn((
            Name::new("simulation_key_light"),
            DirectionalLight {
                illuminance: 10_000.0,
                shadows_enabled: true,
                ..default()
            },
            Transform::from_xyz(5.0, 8.0, 5.0).looking_at(Vec3::ZERO, Vec3::Y),
        ));

        let projection = perspective_projection_from_config(config);
        let sensor_entity = app
            .world_mut()
            .spawn((
                Name::new("simulation_sensor"),
                Camera3d::default(),
                Camera {
                    target: RenderTarget::Image(image_handle.clone()),
                    viewport: Some(Viewport {
                        physical_position: UVec2::ZERO,
                        physical_size: UVec2::new(render_width, render_height),
                        ..default()
                    }),
                    ..default()
                },
                Tonemapping::None,
                projection,
                Exposure::from_physical_camera(physical_camera_params_from_config(config)),
                transform_from_pose(config.pose.translation_m, config.pose.rotation_deg),
                GlobalTransform::default(),
            ))
            .id();

        Ok(Self {
            app,
            sensor_entity,
            output_width: render_width,
            output_height: render_height,
        })
    }

    fn sync_state(&mut self, state: &SimulationControlState) {
        if let Ok(mut entity) = self.app.world_mut().get_entity_mut(self.sensor_entity) {
            if let Some(mut transform) = entity.get_mut::<Transform>() {
                *transform = transform_from_pose(state.translation_m, state.rotation_deg);
            }
            if let Some(mut projection) = entity.get_mut::<Projection>() {
                *projection = Projection::Perspective(PerspectiveProjection {
                    fov: fov_radians(state.sensor_height_mm, state.focal_length_mm),
                    aspect_ratio: self.output_width as f32 / self.output_height.max(1) as f32,
                    near: state.near_m,
                    far: state.far_m,
                });
            }
            if let Some(mut exposure) = entity.get_mut::<Exposure>() {
                *exposure = Exposure::from_physical_camera(PhysicalCameraParameters {
                    aperture_f_stops: state.aperture_f_stop,
                    shutter_speed_s: 1.0 / 30.0,
                    sensitivity_iso: 100.0,
                    sensor_height: state.sensor_height_mm / 1000.0,
                });
            }
        }
    }

    fn update(&mut self) {
        self.app.update();
    }

    fn drain_latest_rgba(&mut self) -> Option<Vec<u8>> {
        let receiver = self.app.world().resource::<MainWorldReceiver>();
        let mut latest = None;
        while let Ok(buffer) = receiver.try_recv() {
            latest = Some(shrink_padded_rgba(
                &buffer,
                self.output_width,
                self.output_height,
            ));
        }
        latest
    }
}

fn transform_from_pose(translation_m: [f32; 3], rotation_deg: [f32; 3]) -> Transform {
    Transform::from_translation(Vec3::new(
        translation_m[0],
        translation_m[1],
        translation_m[2],
    ))
    .with_rotation(Quat::from_euler(
        EulerRot::XYZ,
        rotation_deg[0].to_radians(),
        rotation_deg[1].to_radians(),
        rotation_deg[2].to_radians(),
    ))
}

fn perspective_projection_from_config(config: &SimulationDeviceConfig) -> Projection {
    Projection::Perspective(PerspectiveProjection {
        fov: fov_radians(config.sensor.sensor_height_mm, config.lens.focal_length_mm),
        aspect_ratio: config.sensor.width.max(1) as f32 / config.sensor.height.max(1) as f32,
        near: config.sensor.near_m,
        far: config.sensor.far_m,
    })
}

fn physical_camera_params_from_config(config: &SimulationDeviceConfig) -> PhysicalCameraParameters {
    PhysicalCameraParameters {
        aperture_f_stops: config.lens.aperture_f_stop,
        shutter_speed_s: 1.0 / config.sensor.fps.max(1) as f32,
        sensitivity_iso: 100.0,
        sensor_height: config.sensor.sensor_height_mm / 1000.0,
    }
}

fn fov_radians(sensor_height_mm: f32, focal_length_mm: f32) -> f32 {
    let sensor_height = sensor_height_mm.max(0.1);
    let focal_length = focal_length_mm.max(0.1);
    2.0 * (sensor_height / (2.0 * focal_length)).atan().clamp(0.01, PI - 0.01)
}

fn shrink_padded_rgba(buffer: &[u8], width: u32, height: u32) -> Vec<u8> {
    let row_bytes = width as usize * TextureFormat::bevy_default().pixel_size();
    let aligned_row_bytes = RenderDevice::align_copy_bytes_per_row(row_bytes);
    if row_bytes == aligned_row_bytes {
        return buffer[..row_bytes.saturating_mul(height as usize).min(buffer.len())].to_vec();
    }
    buffer
        .chunks(aligned_row_bytes)
        .take(height as usize)
        .flat_map(|row| row[..row_bytes.min(row.len())].iter().copied())
        .collect()
}

fn scene_asset_path(scene_path: &Path) -> Result<String, CaptureError> {
    let file_name = scene_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| CaptureError::Backend("simulation scene must be a file".into()))?;
    let ext = scene_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "glb" | "gltf" => Ok(format!("{file_name}#Scene0")),
        "scn" | "ron" => Ok(file_name),
        _ => Err(CaptureError::Backend(format!(
            "unsupported simulation scene format: {}",
            scene_path.display()
        ))),
    }
}

struct ImageCopyPlugin;

impl Plugin for ImageCopyPlugin {
    fn build(&self, app: &mut App) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        app.insert_resource(MainWorldReceiver(receiver));

        let render_app = app.sub_app_mut(RenderApp);
        let mut graph = render_app.world_mut().resource_mut::<RenderGraph>();
        graph.add_node(ImageCopyNodeLabel, ImageCopyNode);
        graph.add_node_edge(bevy::render::graph::CameraDriverLabel, ImageCopyNodeLabel);
        render_app
            .insert_resource(RenderWorldSender(sender))
            .add_systems(ExtractSchedule, image_copy_extract)
            .add_systems(Render, receive_image_from_buffer.after(RenderSet::Render));
    }
}

fn image_copy_extract(mut commands: Commands, image_copiers: Extract<Query<&ImageCopier>>) {
    commands.insert_resource(ImageCopiers(
        image_copiers.iter().cloned().collect::<Vec<ImageCopier>>(),
    ));
}

impl render_graph::Node for ImageCopyNode {
    fn run(
        &self,
        _graph: &mut RenderGraphContext,
        render_context: &mut RenderContext,
        world: &World,
    ) -> Result<(), render_graph::NodeRunError> {
        let Some(image_copiers) = world.get_resource::<ImageCopiers>() else {
            return Ok(());
        };
        let Some(gpu_images) = world.get_resource::<RenderAssets<GpuImage>>() else {
            return Ok(());
        };

        for image_copier in image_copiers.iter() {
            let Some(src_image) = gpu_images.get(&image_copier.src_image) else {
                continue;
            };

            let mut encoder = render_context
                .render_device()
                .create_command_encoder(&CommandEncoderDescriptor::default());

            let block_dimensions = src_image.texture_format.block_dimensions();
            let block_size = src_image.texture_format.block_copy_size(None).unwrap_or(4);
            let padded_bytes_per_row = RenderDevice::align_copy_bytes_per_row(
                (src_image.size.x as usize / block_dimensions.0 as usize) * block_size as usize,
            );
            let texture_extent = Extent3d {
                width: src_image.size.x,
                height: src_image.size.y,
                depth_or_array_layers: 1,
            };

            encoder.copy_texture_to_buffer(
                src_image.texture.as_image_copy(),
                ImageCopyBuffer {
                    buffer: &image_copier.buffer,
                    layout: ImageDataLayout {
                        offset: 0,
                        bytes_per_row: Some(
                            std::num::NonZero::<u32>::new(padded_bytes_per_row as u32)
                                .unwrap()
                                .into(),
                        ),
                        rows_per_image: None,
                    },
                },
                texture_extent,
            );

            let render_queue = world.resource::<RenderQueue>();
            render_queue.submit(std::iter::once(encoder.finish()));
        }
        Ok(())
    }
}

fn receive_image_from_buffer(
    image_copiers: Res<ImageCopiers>,
    render_device: Res<RenderDevice>,
    sender: Res<RenderWorldSender>,
) {
    for image_copier in image_copiers.0.iter() {
        let buffer_slice = image_copier.buffer.slice(..);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        buffer_slice.map_async(MapMode::Read, move |result| {
            let _ = ready_tx.send(result);
        });
        let _ = render_device.poll(Maintain::Wait);
        if let Ok(Ok(())) = ready_rx.recv() {
            let _ = sender.send(buffer_slice.get_mapped_range().to_vec());
        }
        image_copier.buffer.unmap();
    }
}

fn apply_output_mode(rgb: &mut [u8], width: u32, height: u32, mode: SimulationOutputMode) {
    match mode {
        SimulationOutputMode::Rgb => {}
        SimulationOutputMode::Depth => {
            for px in rgb.chunks_exact_mut(3) {
                let depth = ((u16::from(px[0]) + u16::from(px[1]) + u16::from(px[2])) / 3) as u8;
                px[0] = depth;
                px[1] = depth;
                px[2] = depth;
            }
        }
        SimulationOutputMode::Normals => {
            if width < 3 || height < 3 {
                return;
            }
            let mut out = rgb.to_vec();
            let row_stride = width as usize * 3;
            for y in 1..(height as usize - 1) {
                for x in 1..(width as usize - 1) {
                    let idx = y * row_stride + x * 3;
                    let left = luma_at(rgb, idx - 3);
                    let right = luma_at(rgb, idx + 3);
                    let up = luma_at(rgb, idx - row_stride);
                    let down = luma_at(rgb, idx + row_stride);
                    let nx = (right - left).clamp(-1.0, 1.0) * 0.5 + 0.5;
                    let ny = (down - up).clamp(-1.0, 1.0) * 0.5 + 0.5;
                    out[idx] = (nx * 255.0) as u8;
                    out[idx + 1] = (ny * 255.0) as u8;
                    out[idx + 2] = 255;
                }
            }
            rgb.copy_from_slice(&out);
        }
        SimulationOutputMode::Segmentation => {
            for px in rgb.chunks_exact_mut(3) {
                let luma = ((u16::from(px[0]) + u16::from(px[1]) + u16::from(px[2])) / 3) as u8;
                if luma < 64 {
                    px[0] = 0;
                    px[1] = 0;
                    px[2] = 0;
                } else if luma < 128 {
                    px[0] = 255;
                    px[1] = 0;
                    px[2] = 0;
                } else if luma < 192 {
                    px[0] = 0;
                    px[1] = 255;
                    px[2] = 0;
                } else {
                    px[0] = 0;
                    px[1] = 0;
                    px[2] = 255;
                }
            }
        }
    }
}

fn luma_at(rgb: &[u8], idx: usize) -> f32 {
    let r = rgb.get(idx).copied().unwrap_or(0) as f32 / 255.0;
    let g = rgb.get(idx + 1).copied().unwrap_or(0) as f32 / 255.0;
    let b = rgb.get(idx + 2).copied().unwrap_or(0) as f32 / 255.0;
    0.2126 * r + 0.7152 * g + 0.0722 * b
}
