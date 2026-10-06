use bevy::app::App;
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::{
    Buffer, BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Extent3d, MapMode, PollType,
    TexelCopyBufferInfo, TexelCopyBufferLayout,
};
use bevy::render::renderer::{RenderDevice, RenderGraph, RenderGraphSystems, RenderQueue};
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

pub(super) struct ImageCopyPlugin;

impl Plugin for ImageCopyPlugin {
    fn build(&self, app: &mut App) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        app.insert_resource(MainWorldReceiver(receiver));

        // The copies run in the render graph schedule once the frame's commands are submitted,
        // so they read this frame's images.
        app.sub_app_mut(RenderApp)
            .insert_resource(RenderWorldSender(sender))
            .add_systems(ExtractSchedule, image_copy_extract)
            .add_systems(RenderGraph, copy_images.in_set(RenderGraphSystems::Finish))
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

fn copy_images(
    image_copiers: Option<Res<ImageCopiers>>,
    gpu_images: Option<Res<RenderAssets<GpuImage>>>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
) {
    let (Some(image_copiers), Some(gpu_images)) = (image_copiers, gpu_images) else {
        return;
    };
    for image_copier in image_copiers.iter() {
        let Some(src_image) = gpu_images.get(&image_copier.src_image) else {
            continue;
        };

        let mut encoder =
            render_device.create_command_encoder(&CommandEncoderDescriptor::default());

        let format = src_image.texture_descriptor.format;
        let size = src_image.texture_descriptor.size;
        let block_dimensions = format.block_dimensions();
        let block_size = format.block_copy_size(None).unwrap_or(4);
        let padded_bytes_per_row = RenderDevice::align_copy_bytes_per_row(
            (size.width as usize / block_dimensions.0 as usize) * block_size as usize,
        );
        let texture_extent = Extent3d {
            width: size.width,
            height: size.height,
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

        render_queue.submit(std::iter::once(encoder.finish()));
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
