//! The output stages on the GPU (`styx-softisp`'s `post`): the picture's planes, the output
//! fields of a parameter block, and the dispatches a frame runs for them.

use std::sync::Arc;

use ash::vk;

use crate::context::Inner;
use crate::error::GpuError;
use crate::kernels::{Kernels, MAX_PASSES, Pass};
use crate::layout::Layout;
use crate::memory::{Buffer, Place};
use crate::params::Block;

/// `OUT_PLANES` in `shaders/common.glsl`: the picture's planes, not an output format.
const PLANES_KIND: u32 = 4;

/// The picture's planes (`Y`, `Cb`, `Cr`, one after another): two buffers read and written in
/// turn, with their sets (set 1 to read, set 2 to write).
pub(crate) struct Planes {
    /// Owned here: the sets point at them.
    #[allow(dead_code)]
    buffers: [Buffer; 2],
    read: [vk::DescriptorSet; 2],
    write: [vk::DescriptorSet; 2],
}

impl Planes {
    /// Two buffers of `size` bytes in `place`, with their sets.
    pub fn new(ctx: &Arc<Inner>, k: &Kernels, size: u64, place: Place) -> Result<Self, GpuError> {
        let buffers = [
            Buffer::new(ctx, size, place, false)?,
            Buffer::new(ctx, size, place, false)?,
        ];
        let mut read = [vk::DescriptorSet::null(); 2];
        let mut write = [vk::DescriptorSet::null(); 2];
        for i in 0..2 {
            read[i] = k.set(1, &[buffers[i].buffer])?;
            write[i] = k.set(2, &[buffers[i].buffer])?;
        }
        Ok(Self {
            buffers,
            read,
            write,
        })
    }
}

/// The output fields of `b` for a `layout` output (the picture's, or the output stage's).
pub(crate) fn set_output(b: &mut Block, layout: &Layout) {
    b.out_kind = layout.kind.code();
    b.out_width = layout.width as u32;
    b.out_height = layout.height as u32;
    for (i, p) in layout.planes.iter().enumerate() {
        b.out_offset[i] = p.offset as u32;
        b.out_stride[i] = p.stride as u32;
    }
}

/// The output fields of `b` for the picture's planes: `Y`, `Cb`, `Cr` of `w` x `h`, one after
/// another.
pub(crate) fn set_planes(b: &mut Block, w: usize, h: usize) {
    b.out_kind = PLANES_KIND;
    b.out_width = w as u32;
    b.out_height = h as u32;
    for k in 0..3 {
        b.out_offset[k] = (k * w * h) as u32;
        b.out_stride[k] = w as u32;
    }
}

/// A frame that runs the output stages: the picture is made into planes, denoised and
/// sharpened in turn, and the output format is written from the last planes.
pub(crate) struct Stages<'a> {
    pub kernels: &'a Kernels,
    pub planes: &'a Planes,
    /// Set 0 of the picture's parameter slot and of the output's.
    pub set0: [vk::DescriptorSet; 2],
    /// The raw input's set, and the output's.
    pub input: vk::DescriptorSet,
    pub output: vk::DescriptorSet,
    /// The picture's pipeline (full or half size) and its size.
    pub picture: vk::Pipeline,
    pub size: (u32, u32),
    /// Groups of the output dispatch.
    pub out_groups: (u32, u32),
    pub denoise: bool,
    pub sharpen: bool,
}

impl Stages<'_> {
    /// The dispatches in order: each reads the planes the one before wrote.
    pub fn passes(&self) -> [Option<Pass>; MAX_PASSES] {
        let (pw, ph) = self.size;
        let (planes, k) = (self.planes, self.kernels);
        let filter = (pw.div_ceil(16), ph.div_ceil(16));
        let mut passes = [None; MAX_PASSES];
        passes[0] = Some(Pass {
            sets: [self.set0[0], self.input, planes.write[0]],
            pipeline: self.picture,
            groups: (pw.div_ceil(32), ph.div_ceil(16)),
        });
        let (mut n, mut cur) = (1, 0);
        for (on, pipeline) in [(self.denoise, k.denoise), (self.sharpen, k.sharpen)] {
            if on {
                passes[n] = Some(Pass {
                    sets: [self.set0[0], planes.read[cur], planes.write[1 - cur]],
                    pipeline,
                    groups: filter,
                });
                (n, cur) = (n + 1, 1 - cur);
            }
        }
        passes[n] = Some(Pass {
            sets: [self.set0[1], planes.read[cur], self.output],
            pipeline: k.output,
            groups: self.out_groups,
        });
        passes
    }
}
