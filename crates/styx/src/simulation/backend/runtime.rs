use std::path::Path;

use bevy::app::App;
use bevy::asset::{AssetPlugin, Assets, RenderAssetUsages};
use bevy::camera::{Exposure, ImageRenderTarget, RenderTarget, Viewport};
use bevy::image::{Image, TextureFormatPixelInfo};
use bevy::light::GlobalAmbientLight;
use bevy::prelude::*;
use bevy::render::RenderApp;
use bevy::render::render_resource::{TextureDimension, TextureFormat, TextureUsages};
use bevy::render::renderer::RenderDevice;
use bevy::render::view::Msaa;
use bevy::window::{ExitCondition, WindowPlugin};
use bevy::winit::WinitPlugin;

use crate::capture_api::CaptureError;
use crate::simulation::{SimulationDeviceConfig, SimulationOutputMode};

use super::readback::{
    ImageCopier, ImageCopyPlugin, MainWorldReceiver, ReadbackKind, ReadbackPacket,
};
use super::state::{SimulationControlState, rgba_to_rgb};
use super::visualization::{
    SimulationSceneKey, SimulationViewState, SimulationVisualizationPlugin,
    perspective_projection_from_config, physical_camera_params_from_config, render_extent,
    spawn_overlay_entities, sync_camera_entity, transform_from_pose,
};

pub(super) struct BevySimulationRuntime {
    app: App,
    sensor_entity: Entity,
    depth_sensor_entity: Entity,
    output_width: u32,
    output_height: u32,
}

impl BevySimulationRuntime {
    pub(super) fn new(
        scene_path: &Path,
        config: &SimulationDeviceConfig,
    ) -> Result<Self, CaptureError> {
        let asset_root = scene_path.parent().ok_or_else(|| {
            CaptureError::Backend("simulation scene has no parent directory".into())
        })?;
        let scene_asset_path = scene_asset_path(scene_path)?;
        let render_width = config.sensor.width.max(1);
        let render_height = config.sensor.height.max(1);
        let base_clear_color = Color::srgba(
            config.clear_color_rgba[0],
            config.clear_color_rgba[1],
            config.clear_color_rgba[2],
            config.clear_color_rgba[3],
        );

        let mut app = App::new();
        app.insert_resource(ClearColor(base_clear_color));
        app.insert_resource(SimulationSceneKey(scene_path.display().to_string()));
        app.insert_resource(SimulationViewState {
            output_mode: config.output_mode,
            near_m: config.sensor.near_m,
            far_m: config.sensor.far_m,
            base_clear_color,
        });
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
        app.add_plugins((ImageCopyPlugin, SimulationVisualizationPlugin));
        app.finish();
        app.cleanup();

        let size = render_extent(config);
        let image_handle = {
            let mut images = app.world_mut().resource_mut::<Assets<Image>>();
            let mut render_target_image = Image::new_fill(
                size,
                TextureDimension::D2,
                &[0; 4],
                TextureFormat::bevy_default(),
                RenderAssetUsages::default(),
            );
            render_target_image.texture_descriptor.usage |= TextureUsages::COPY_SRC
                | TextureUsages::RENDER_ATTACHMENT
                | TextureUsages::TEXTURE_BINDING;
            images.add(render_target_image)
        };
        let depth_image_handle = {
            let mut images = app.world_mut().resource_mut::<Assets<Image>>();
            let mut render_target_image = Image::new_fill(
                size,
                TextureDimension::D2,
                &[0; 16],
                TextureFormat::Rgba32Float,
                RenderAssetUsages::default(),
            );
            render_target_image.texture_descriptor.usage |= TextureUsages::COPY_SRC
                | TextureUsages::RENDER_ATTACHMENT
                | TextureUsages::TEXTURE_BINDING;
            images.add(render_target_image)
        };

        let (image_copier, depth_image_copier) = {
            let render_device = app.sub_app(RenderApp).world().resource::<RenderDevice>();
            (
                ImageCopier::new(
                    image_handle.clone(),
                    size,
                    4,
                    ReadbackKind::Color,
                    render_device,
                ),
                ImageCopier::new(
                    depth_image_handle.clone(),
                    size,
                    16,
                    ReadbackKind::Depth,
                    render_device,
                ),
            )
        };
        app.world_mut().spawn(image_copier);
        app.world_mut().spawn(depth_image_copier);

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

        app.world_mut().insert_resource(GlobalAmbientLight {
            color: Color::srgb(0.8, 0.82, 0.85),
            brightness: 500.0,
            affects_lightmapped_meshes: true,
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
        let sensor_transform =
            transform_from_pose(config.pose.translation_m, config.pose.rotation_deg);
        let sensor_entity = app
            .world_mut()
            .spawn((
                Name::new("simulation_sensor"),
                Camera3d::default(),
                Camera {
                    viewport: Some(Viewport {
                        physical_position: UVec2::ZERO,
                        physical_size: UVec2::new(render_width, render_height),
                        ..default()
                    }),
                    ..default()
                },
                RenderTarget::Image(ImageRenderTarget::from(image_handle.clone())),
                Msaa::Off,
                bevy::core_pipeline::prepass::DepthPrepass,
                bevy::core_pipeline::prepass::NormalPrepass,
                bevy::core_pipeline::tonemapping::Tonemapping::None,
                projection,
                Exposure::from_physical_camera(physical_camera_params_from_config(config)),
                sensor_transform,
                GlobalTransform::default(),
            ))
            .id();
        let depth_sensor_entity = app
            .world_mut()
            .spawn((
                Name::new("simulation_depth_sensor"),
                Camera3d::default(),
                Camera {
                    viewport: Some(Viewport {
                        physical_position: UVec2::ZERO,
                        physical_size: UVec2::new(render_width, render_height),
                        ..default()
                    }),
                    is_active: matches!(config.output_mode, SimulationOutputMode::Depth),
                    ..default()
                },
                RenderTarget::Image(ImageRenderTarget::from(depth_image_handle.clone())),
                Msaa::Off,
                bevy::core_pipeline::prepass::DepthPrepass,
                bevy::core_pipeline::tonemapping::Tonemapping::None,
                perspective_projection_from_config(config),
                Exposure::from_physical_camera(physical_camera_params_from_config(config)),
                sensor_transform,
                GlobalTransform::default(),
            ))
            .id();

        spawn_overlay_entities(&mut app, sensor_entity, depth_sensor_entity, config);

        Ok(Self {
            app,
            sensor_entity,
            depth_sensor_entity,
            output_width: render_width,
            output_height: render_height,
        })
    }

    pub(super) fn sync_state(&mut self, state: &SimulationControlState) {
        if let Some(mut view_state) = self
            .app
            .world_mut()
            .get_resource_mut::<SimulationViewState>()
        {
            view_state.output_mode = state.output_mode;
            view_state.near_m = state.near_m;
            view_state.far_m = state.far_m;
        }

        sync_camera_entity(
            &mut self.app,
            self.sensor_entity,
            state,
            self.output_width,
            self.output_height,
            true,
        );
        sync_camera_entity(
            &mut self.app,
            self.depth_sensor_entity,
            state,
            self.output_width,
            self.output_height,
            matches!(state.output_mode, SimulationOutputMode::Depth),
        );
    }

    pub(super) fn update(&mut self) {
        self.app.update();
    }

    pub(super) fn drain_latest(
        &mut self,
        _state: &SimulationControlState,
        latest_rgb: &mut Vec<u8>,
        latest_depth: &mut Vec<u8>,
    ) {
        let receiver = self.app.world().resource::<MainWorldReceiver>();
        while let Ok(packet) = receiver.try_recv() {
            match packet {
                ReadbackPacket::Color(buffer) => {
                    let rgba = shrink_padded_rgba(&buffer, self.output_width, self.output_height);
                    rgba_to_rgb(&rgba, self.output_width, self.output_height, latest_rgb);
                }
                ReadbackPacket::Depth(buffer) => {
                    rgba32float_depth_to_meters(
                        &buffer,
                        self.output_width,
                        self.output_height,
                        latest_depth,
                    );
                }
            }
        }
    }
}

fn shrink_padded_rgba(buffer: &[u8], width: u32, height: u32) -> Vec<u8> {
    let row_bytes = width as usize * TextureFormat::bevy_default().pixel_size().unwrap_or(4);
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

fn rgba32float_depth_to_meters(buffer: &[u8], width: u32, height: u32, out: &mut Vec<u8>) {
    let dst_row_bytes = width as usize * 4;
    let src_row_bytes = width as usize * 16;
    let aligned_src_row_bytes = RenderDevice::align_copy_bytes_per_row(src_row_bytes);
    let needed = dst_row_bytes.saturating_mul(height as usize);
    if out.len() != needed {
        out.resize(needed, 0);
    }

    for (row_index, row) in buffer
        .chunks(aligned_src_row_bytes)
        .take(height as usize)
        .enumerate()
    {
        let src = &row[..src_row_bytes.min(row.len())];
        let dst = &mut out[row_index * dst_row_bytes..(row_index + 1) * dst_row_bytes];
        for (src_px, dst_px) in src.chunks_exact(16).zip(dst.chunks_exact_mut(4)) {
            dst_px.copy_from_slice(&src_px[..4]);
        }
    }
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
