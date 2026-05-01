use std::f32::consts::PI;
use std::hash::{Hash, Hasher};

use bevy::app::App;
use bevy::asset::{Asset, Assets, uuid_handle};
use bevy::camera::{Exposure, PhysicalCameraParameters};
use bevy::light::NotShadowCaster;
use bevy::pbr::{Material, MaterialPlugin, MeshMaterial3d, StandardMaterial};
use bevy::prelude::*;
use bevy::reflect::TypePath;
use bevy::render::render_resource::{AsBindGroup, Extent3d};
use bevy::shader::ShaderRef;

use crate::simulation::{SimulationDeviceConfig, SimulationOutputMode};

const PREPASS_OUTPUT_SHADER_HANDLE: Handle<Shader> =
    uuid_handle!("73545978-7072-6570-6173-735f6f757400");
const PREPASS_OUTPUT_SHADER: &str = r#"
#import bevy_pbr::forward_io::VertexOutput
#import bevy_pbr::prepass_utils::{prepass_depth, prepass_normal}

@group(2) @binding(0) var<uniform> material: vec4<f32>;

fn linearize_depth(depth: f32, near_m: f32, far_m: f32) -> f32 {
    let z_ndc = depth * 2.0 - 1.0;
    return (2.0 * near_m * far_m) / max(far_m + near_m - z_ndc * (far_m - near_m), 0.0001);
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    if u32(material.x) == 1u {
        let depth = prepass_depth(in.position, 0u);
        let linear_depth = linearize_depth(depth, material.y, material.z);
        return vec4<f32>(linear_depth, linear_depth, linear_depth, 1.0);
    }

#ifdef NORMAL_PREPASS
    let normal = prepass_normal(in.position, 0u);
    let rgb = normal * 0.5 + vec3<f32>(0.5, 0.5, 0.5);
    return vec4<f32>(rgb, 1.0);
#else
    return vec4<f32>(0.0, 0.0, 0.0, 1.0);
#endif
}
"#;

#[derive(Resource, Debug, Clone)]
pub(super) struct SimulationViewState {
    pub output_mode: SimulationOutputMode,
    pub near_m: f32,
    pub far_m: f32,
    pub base_clear_color: Color,
}

#[derive(Resource, Clone)]
pub(super) struct SimulationSceneKey(pub String);

#[derive(Component)]
struct SimulationPrepassOverlay;

#[derive(Component, Clone)]
struct OriginalStandardMaterial(Handle<StandardMaterial>);

#[derive(Component, Clone)]
struct SegmentationStandardMaterial(Handle<StandardMaterial>);

#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
struct PrepassOutputMaterial {
    #[uniform(0)]
    settings: [f32; 4],
}

impl Material for PrepassOutputMaterial {
    fn fragment_shader() -> ShaderRef {
        PREPASS_OUTPUT_SHADER_HANDLE.into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Opaque
    }
}

pub(super) struct SimulationVisualizationPlugin;

impl Plugin for SimulationVisualizationPlugin {
    fn build(&self, app: &mut App) {
        let _ = app.world_mut().resource_mut::<Assets<Shader>>().insert(
            PREPASS_OUTPUT_SHADER_HANDLE.id(),
            Shader::from_wgsl(PREPASS_OUTPUT_SHADER, file!()),
        );

        app.add_plugins(MaterialPlugin::<PrepassOutputMaterial>::default())
            .add_systems(
                Update,
                (
                    register_segmentation_materials,
                    apply_segmentation_materials,
                    update_visualization_entities,
                )
                    .chain(),
            );
    }
}

type SegmentationMaterialQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static MeshMaterial3d<StandardMaterial>,
        Option<&'static Name>,
    ),
    Added<MeshMaterial3d<StandardMaterial>>,
>;

fn register_segmentation_materials(
    mut commands: Commands,
    scene_key: Res<SimulationSceneKey>,
    query: SegmentationMaterialQuery,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (entity, material, name) in query.iter() {
        let segmentation_id = stable_segmentation_id(&scene_key.0, name);
        let segmentation_handle = materials.add(StandardMaterial {
            base_color: segmentation_color_for_id(segmentation_id),
            unlit: true,
            perceptual_roughness: 1.0,
            metallic: 0.0,
            reflectance: 0.0,
            ..default()
        });
        commands.entity(entity).insert((
            OriginalStandardMaterial(material.0.clone()),
            SegmentationStandardMaterial(segmentation_handle),
        ));
    }
}

fn apply_segmentation_materials(
    view_state: Res<SimulationViewState>,
    mut query: Query<(
        &mut MeshMaterial3d<StandardMaterial>,
        &OriginalStandardMaterial,
        &SegmentationStandardMaterial,
    )>,
) {
    let use_segmentation = matches!(view_state.output_mode, SimulationOutputMode::Segmentation);
    for (mut current, original, segmentation) in query.iter_mut() {
        let next = if use_segmentation {
            &segmentation.0
        } else {
            &original.0
        };
        if current.0 != *next {
            current.0 = next.clone();
        }
    }
}

fn update_visualization_entities(
    view_state: Res<SimulationViewState>,
    mut clear_color: ResMut<ClearColor>,
    mut overlay_query: Query<
        (&MeshMaterial3d<PrepassOutputMaterial>, &mut Visibility),
        With<SimulationPrepassOverlay>,
    >,
    mut materials: ResMut<Assets<PrepassOutputMaterial>>,
) {
    let show_normals = matches!(view_state.output_mode, SimulationOutputMode::Normals);

    *clear_color = ClearColor(match view_state.output_mode {
        SimulationOutputMode::Rgb => view_state.base_clear_color,
        _ => Color::BLACK,
    });

    for (material_handle, mut visibility) in overlay_query.iter_mut() {
        *visibility = if show_normals {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if let Some(material) = materials.get_mut(&material_handle.0) {
            material.settings[0] = 2.0;
            material.settings[1] = view_state.near_m;
            material.settings[2] = view_state.far_m;
        }
    }
}

fn segmentation_color_for_id(id: u32) -> Color {
    let hashed = id.wrapping_mul(0x45d9f3b).rotate_left(13);
    let bytes = hashed.to_le_bytes();
    let r = bytes[0].max(32);
    let g = bytes[1].max(32);
    let b = bytes[2].max(32);
    Color::srgb_u8(r, g, b)
}

fn stable_segmentation_id(scene_key: &str, name: Option<&Name>) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    scene_key.hash(&mut hasher);
    name.map(|value| value.as_str())
        .unwrap_or("unnamed")
        .hash(&mut hasher);
    let bytes = hasher.finish().to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).max(1)
}

pub(super) fn transform_from_pose(translation_m: [f32; 3], rotation_deg: [f32; 3]) -> Transform {
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

pub(super) fn sync_camera_entity(
    app: &mut App,
    entity_id: Entity,
    state: &super::state::SimulationControlState,
    output_width: u32,
    output_height: u32,
    is_active: bool,
) {
    if let Ok(mut entity) = app.world_mut().get_entity_mut(entity_id) {
        if let Some(mut camera) = entity.get_mut::<Camera>() {
            camera.is_active = is_active;
        }
        if let Some(mut transform) = entity.get_mut::<Transform>() {
            *transform = transform_from_pose(state.translation_m, state.rotation_deg);
        }
        if let Some(mut projection) = entity.get_mut::<Projection>() {
            *projection = Projection::Perspective(PerspectiveProjection {
                fov: fov_radians(state.sensor_height_mm, state.focal_length_mm),
                aspect_ratio: output_width as f32 / output_height.max(1) as f32,
                near: state.near_m,
                far: state.far_m,
                ..default()
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

pub(super) fn perspective_projection_from_config(config: &SimulationDeviceConfig) -> Projection {
    Projection::Perspective(PerspectiveProjection {
        fov: fov_radians(config.sensor.sensor_height_mm, config.lens.focal_length_mm),
        aspect_ratio: config.sensor.width.max(1) as f32 / config.sensor.height.max(1) as f32,
        near: config.sensor.near_m,
        far: config.sensor.far_m,
        ..default()
    })
}

pub(super) fn physical_camera_params_from_config(
    config: &SimulationDeviceConfig,
) -> PhysicalCameraParameters {
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
    2.0 * (sensor_height / (2.0 * focal_length))
        .atan()
        .clamp(0.01, PI - 0.01)
}

pub(super) fn spawn_overlay_entities(
    app: &mut App,
    sensor_entity: Entity,
    depth_sensor_entity: Entity,
    config: &SimulationDeviceConfig,
) {
    let overlay_material = {
        let mut materials = app
            .world_mut()
            .resource_mut::<Assets<PrepassOutputMaterial>>();
        materials.add(PrepassOutputMaterial {
            settings: [2.0, config.sensor.near_m, config.sensor.far_m, 0.0],
        })
    };
    let depth_overlay_material = {
        let mut materials = app
            .world_mut()
            .resource_mut::<Assets<PrepassOutputMaterial>>();
        materials.add(PrepassOutputMaterial {
            settings: [1.0, config.sensor.near_m, config.sensor.far_m, 0.0],
        })
    };
    let overlay_mesh = {
        let mut meshes = app.world_mut().resource_mut::<Assets<Mesh>>();
        meshes.add(Rectangle::new(100.0, 100.0))
    };
    let overlay_entity = app
        .world_mut()
        .spawn((
            Name::new("simulation_prepass_overlay"),
            Mesh3d(overlay_mesh.clone()),
            MeshMaterial3d(overlay_material),
            Transform::from_xyz(0.0, 0.0, -0.2),
            Visibility::Hidden,
            NotShadowCaster,
            SimulationPrepassOverlay,
        ))
        .id();
    app.world_mut()
        .entity_mut(sensor_entity)
        .add_child(overlay_entity);
    let depth_overlay_entity = app
        .world_mut()
        .spawn((
            Name::new("simulation_depth_overlay"),
            Mesh3d(overlay_mesh),
            MeshMaterial3d(depth_overlay_material),
            Transform::from_xyz(0.0, 0.0, -0.2),
            Visibility::Visible,
            NotShadowCaster,
        ))
        .id();
    app.world_mut()
        .entity_mut(depth_sensor_entity)
        .add_child(depth_overlay_entity);
}

pub(super) fn render_extent(config: &SimulationDeviceConfig) -> Extent3d {
    Extent3d {
        width: config.sensor.width.max(1),
        height: config.sensor.height.max(1),
        ..default()
    }
}
