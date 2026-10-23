//! Pyramid levels by the rounded 2x2 box filter, on the GPU.

use std::sync::mpsc;

use crate::{Gpu, GpuError};

const SHADER: &str = include_str!("../shaders/box_half.wgsl");
/// The shader's workgroup size (words, rows).
const GROUP: (u32, u32) = (16, 8);

/// An 8-bit luma image: `height` rows of `width` pixels, `stride` bytes apart.
#[derive(Clone, Copy, Debug)]
pub struct Luma<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

/// A pyramid level as the GPU made it: `height` rows of `width` pixels, `stride` bytes apart
/// (a multiple of 4; the bytes past `width` are not part of the image).
#[derive(Clone, Copy, Debug)]
pub struct Level<'a> {
    /// 1 for half size, 2 for a quarter, ...
    pub level: u8,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: &'a [u8],
}

/// One level's buffers: its size, row stride in words, pixels, parameters' bind group, and
/// where it lands in the readback buffer.
struct LevelBuffers {
    width: u32,
    height: u32,
    stride_words: u32,
    bind: wgpu::BindGroup,
    pixels: wgpu::Buffer,
    offset: u64,
}

/// The buffers for one input size, stride and level count, made once and reused.
struct Plan {
    key: (u32, u32, usize, u8),
    /// The input's row stride in words as uploaded (the frame's own when it is a multiple of
    /// 4 bytes, else the width rounded up to words).
    src_stride_words: u32,
    src: wgpu::Buffer,
    levels: Vec<LevelBuffers>,
    readback: wgpu::Buffer,
}

/// Pyramid levels of 8-bit luma images: each level the level above halved by
/// `(a + b + c + d + 2) >> 2`, sizes rounded down, all levels in one submission and read back
/// together. Exactly styx-core's `box_downscale_luma`, level by level.
pub struct BoxPyramid {
    gpu: Gpu,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    plan: Option<Plan>,
    /// The input repacked to whole words when its stride is not.
    packed: Vec<u8>,
}

fn storage(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

impl BoxPyramid {
    pub fn new(gpu: &Gpu) -> Self {
        let device = gpu.device();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("styx box_half"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("styx box_half"),
            entries: &[
                storage(0, true),
                storage(1, false),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("styx box_half"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("styx box_half"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        Self {
            gpu: gpu.clone(),
            pipeline,
            layout,
            plan: None,
            packed: Vec::new(),
        }
    }

    /// Levels 1 to `levels` of `src`, handed to `out` finest first while the readback is
    /// mapped (copy what is kept).
    pub fn run(
        &mut self,
        src: Luma<'_>,
        levels: u8,
        mut out: impl FnMut(Level<'_>),
    ) -> Result<(), GpuError> {
        let (w, h, stride) = (src.width, src.height, src.stride);
        if levels == 0 {
            return Ok(());
        }
        if levels > 16 || (w >> levels) == 0 || (h >> levels) == 0 {
            return Err(GpuError::BadImage(format!("{w}x{h} has no level {levels}")));
        }
        let rows = h as usize;
        if stride < w as usize || src.data.len() < stride * (rows - 1) + w as usize {
            return Err(GpuError::BadImage(format!(
                "{w}x{h}, stride {stride}: {} bytes",
                src.data.len()
            )));
        }
        let key = (w, h, stride, levels);
        if self.plan.as_ref().is_none_or(|p| p.key != key) {
            self.plan = Some(self.plan_for(key));
        }
        let Some(plan) = self.plan.as_ref() else {
            return Err(GpuError::Device("no plan".into()));
        };
        let queue = self.gpu.queue();
        // The input as whole words per row: in place when its stride allows (and the last row
        // is whole), else repacked.
        let whole = stride * rows;
        if stride % 4 == 0 && src.data.len() >= whole {
            queue.write_buffer(&plan.src, 0, &src.data[..whole]);
        } else {
            let row = plan.src_stride_words as usize * 4;
            self.packed.resize(row * rows, 0);
            for (y, dst) in self.packed.chunks_exact_mut(row).enumerate() {
                let line = &src.data[y * stride..y * stride + w as usize];
                dst[..line.len()].copy_from_slice(line);
            }
            queue.write_buffer(&plan.src, 0, &self.packed);
        }
        let device = self.gpu.device();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("styx pyramid"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("styx pyramid"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            for l in &plan.levels {
                pass.set_bind_group(0, &l.bind, &[]);
                pass.dispatch_workgroups(
                    l.stride_words.div_ceil(GROUP.0),
                    l.height.div_ceil(GROUP.1),
                    1,
                );
            }
        }
        for l in &plan.levels {
            let len = u64::from(l.stride_words) * 4 * u64::from(l.height);
            encoder.copy_buffer_to_buffer(&l.pixels, 0, &plan.readback, l.offset, len);
        }
        queue.submit([encoder.finish()]);
        let (tx, rx) = mpsc::channel();
        plan.readback.map_async(wgpu::MapMode::Read, .., move |r| {
            let _ = tx.send(r);
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| GpuError::Device(e.to_string()))?;
        rx.recv()
            .map_err(|e| GpuError::Device(e.to_string()))?
            .map_err(|e| GpuError::Device(e.to_string()))?;
        {
            let view = plan
                .readback
                .get_mapped_range(..)
                .map_err(|e| GpuError::Device(e.to_string()))?;
            for (i, l) in plan.levels.iter().enumerate() {
                let stride = l.stride_words as usize * 4;
                let start = l.offset as usize;
                out(Level {
                    level: i as u8 + 1,
                    width: l.width,
                    height: l.height,
                    stride,
                    data: &view[start..start + stride * l.height as usize],
                });
            }
        }
        plan.readback.unmap();
        Ok(())
    }

    fn plan_for(&self, key: (u32, u32, usize, u8)) -> Plan {
        let (w, h, stride, levels) = key;
        let device = self.gpu.device();
        let src_stride_words = if stride % 4 == 0 {
            (stride / 4) as u32
        } else {
            w.div_ceil(4)
        };
        let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(4),
                usage,
                mapped_at_creation: false,
            })
        };
        let src = buffer(
            "styx pyramid source",
            u64::from(src_stride_words) * 4 * u64::from(h),
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        let mut out = Vec::new();
        let (mut in_w, mut in_h, mut in_stride) = (w, h, src_stride_words);
        let mut offset = 0u64;
        for _ in 0..levels {
            let (width, height) = (in_w / 2, in_h / 2);
            let stride_words = width.div_ceil(4);
            let size = u64::from(stride_words) * 4 * u64::from(height);
            let pixels = buffer(
                "styx pyramid level",
                size,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            );
            let params = buffer(
                "styx pyramid params",
                16,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            );
            let words = [in_stride, stride_words, height, 0];
            let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
            self.gpu.queue().write_buffer(&params, 0, &bytes);
            let input = out.last().map_or(&src, |l: &LevelBuffers| &l.pixels);
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("styx pyramid level"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: input.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: pixels.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: params.as_entire_binding(),
                    },
                ],
            });
            out.push(LevelBuffers {
                width,
                height,
                stride_words,
                bind,
                pixels,
                offset,
            });
            offset += size;
            (in_w, in_h, in_stride) = (width, height, stride_words);
        }
        let readback = buffer(
            "styx pyramid readback",
            offset,
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        Plan {
            key,
            src_stride_words,
            src,
            levels: out,
            readback,
        }
    }
}
