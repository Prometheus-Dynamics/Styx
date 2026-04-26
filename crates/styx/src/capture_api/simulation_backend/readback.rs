use bevy::app::App;
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_graph::{self, RenderGraph, RenderGraphContext, RenderLabel};
use bevy::render::render_resource::{
    Buffer, BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Extent3d, MapMode, PollType,
    TexelCopyBufferInfo, TexelCopyBufferLayout,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue};
use bevy::render::texture::GpuImage;
use bevy::render::{Extract, ExtractSchedule, Render, RenderApp, RenderSystems};
use crossbeam_channel::{Receiver, Sender};

#[derive(Debug)]
pub(super) enum ReadbackPacket {
    Color(Vec<u8>),
    Depth(Vec<u8>),
}

#[derive(Resource, Deref)]
pub(super) struct MainWorldReceiver(pub Receiver<ReadbackPacket>);

#[derive(Resource, Deref)]
struct RenderWorldSender(Sender<ReadbackPacket>);

#[derive(Clone, Default, Resource, Deref, DerefMut)]
struct ImageCopiers(Vec<ImageCopier>);

#[derive(Clone, Copy)]
pub(super) enum ReadbackKind {
    Color,
    Depth,
}

#[derive(Clone, Component)]
pub(super) struct ImageCopier {
    buffer: Buffer,
    src_image: Handle<Image>,
    kind: ReadbackKind,
}

impl ImageCopier {
    pub(super) fn new(
        src_image: Handle<Image>,
        size: Extent3d,
        bytes_per_pixel: usize,
        kind: ReadbackKind,
        render_device: &RenderDevice,
    ) -> Self {
        let padded_bytes_per_row =
            RenderDevice::align_copy_bytes_per_row(size.width as usize * bytes_per_pixel);
        let buffer = render_device.create_buffer(&BufferDescriptor {
            label: None,
            size: padded_bytes_per_row as u64 * size.height as u64,
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            buffer,
            src_image,
            kind,
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, RenderLabel)]
struct ImageCopyNodeLabel;

#[derive(Default)]
struct ImageCopyNode;

pub(super) struct ImageCopyPlugin;

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
            .add_systems(
                Render,
                receive_image_from_buffer.after(RenderSystems::Render),
            );
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
                (src_image.size.width as usize / block_dimensions.0 as usize) * block_size as usize,
            );
            let texture_extent = Extent3d {
                width: src_image.size.width,
                height: src_image.size.height,
                depth_or_array_layers: 1,
            };

            encoder.copy_texture_to_buffer(
                src_image.texture.as_image_copy(),
                TexelCopyBufferInfo {
                    buffer: &image_copier.buffer,
                    layout: TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_bytes_per_row as u32),
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
        let _ = render_device.poll(PollType::wait_indefinitely());
        if let Ok(Ok(())) = ready_rx.recv() {
            let packet = match image_copier.kind {
                ReadbackKind::Color => {
                    ReadbackPacket::Color(buffer_slice.get_mapped_range().to_vec())
                }
                ReadbackKind::Depth => {
                    ReadbackPacket::Depth(buffer_slice.get_mapped_range().to_vec())
                }
            };
            let _ = sender.send(packet);
        }
        image_copier.buffer.unmap();
    }
}
