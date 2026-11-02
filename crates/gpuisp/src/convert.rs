//! Pixel-format conversions on the GPU: the scalar kernels of `styx_core::simd`, bit for bit.
//!
//! [`GpuConvert`] runs `shaders/convert.comp`: one invocation per output pixel (per chroma
//! sample for NV12's chroma). A call copies its source planes into a host-visible buffer,
//! records one dispatch, submits it, waits, and copies the output rows back at the caller's
//! stride, so bytes between rows are left as they were. The pipeline, descriptor set,
//! command buffer and fence are made once; the buffers grow only for a larger image.

use std::sync::Arc;

use ash::vk;
use styx_core::simd::{ColorLayout, Orientation, YuvMatrix};

use crate::context::{GpuContext, Inner};
use crate::error::{GpuError, VkContext};
use crate::kernels::spirv;
use crate::memory::{Buffer, Place};

/// SPIR-V of `shaders/convert.comp` (`shaders/build.sh` makes it).
const CONVERT: &[u8] = include_bytes!("../shaders/convert.spv");

/// Push-constant words of `convert.comp`'s `Args`, in its order.
pub(crate) const ARG_WORDS: usize = 18;

/// The op codes of `convert.comp` (`OP_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
enum Op {
    SwapRb24 = 0,
    SwapRb32,
    X32Rgb24,
    Luma,
    Rgb24Rgba,
    Gray8Rgb24,
    Gray16Rgb24,
    Rgb48Rgb24,
    YuyvLuma,
    YuyvChroma,
    Transform,
    Nv12Rgb,
    YuyvRgb,
    I420Rgb,
}

/// `convert.comp`'s flags: red and blue swapped (bit 0; for TRANSFORM, mirror x), mirror y
/// (bit 1), transpose (bit 2).
const SWAP: u32 = 1;
const FLIP_Y: u32 = 2;
const TRANSPOSE: u32 = 4;

/// A host image: rows `stride` bytes apart, each row's pixels packed.
#[derive(Clone, Copy, Debug)]
pub struct Image<'a> {
    pub data: &'a [u8],
    pub stride: usize,
}

/// A writable host image (see [`Image`]).
#[derive(Debug)]
pub struct ImageMut<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
}

/// One source plane: `rows` rows of `row` bytes, `stride` apart, in `data`.
struct Plane<'a> {
    data: &'a [u8],
    stride: usize,
    row: usize,
    rows: usize,
}

fn plane(img: Image<'_>, row: usize, rows: usize) -> Plane<'_> {
    Plane {
        data: img.data,
        stride: img.stride,
        row,
        rows,
    }
}

/// Asserts that `len` bytes hold `rows` rows of `row` bytes, `stride` apart.
pub(crate) fn check_rows(what: &str, len: usize, stride: usize, row: usize, rows: usize) {
    assert!(
        stride >= row,
        "{what}: stride {stride} below a row of {row} bytes"
    );
    assert!(
        rows == 0 || len >= (rows - 1) * stride + row,
        "{what}: {len} bytes do not hold {rows} rows of {row} bytes at stride {stride}"
    );
}

/// What one dispatch computes.
#[derive(Clone, Copy)]
struct Args {
    op: Op,
    /// Output pixels (width, height) the dispatch covers.
    grid: (usize, usize),
    /// Bytes per source pixel (luma, transform) and the flags.
    bpp: usize,
    flags: u32,
    /// `YuvMatrix` as `y_offset, y_scale, r_v, g_u, g_v, b_u`.
    matrix: [i32; 6],
}

impl Args {
    fn new(op: Op, grid: (usize, usize)) -> Self {
        Self {
            op,
            grid,
            bpp: 0,
            flags: 0,
            matrix: [0; 6],
        }
    }

    fn yuv(op: Op, grid: (usize, usize), m: &YuvMatrix) -> Self {
        let mut a = Self::new(op, grid);
        a.matrix = [
            i32::from(m.y_offset),
            i32::from(m.y_scale),
            i32::from(m.r_v),
            i32::from(m.g_u),
            i32::from(m.g_v),
            i32::from(m.b_u),
        ];
        a
    }
}

/// The GPU's pixel-format conversions, one per method, each bit-exact with its `styx_core`
/// scalar kernel. Methods take `&mut self`: one conversion at a time per instance.
pub struct GpuConvert {
    ctx: Arc<Inner>,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    /// `shaders/jpeg.comp` (`src/jpeg.rs`), on the same layout.
    pub(crate) jpeg: vk::Pipeline,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// Source planes, copied in (mapped; the device reads them).
    pub(crate) input: Option<Buffer>,
    /// Output rows, read back (mapped).
    pub(crate) output: Option<Buffer>,
}

impl Drop for GpuConvert {
    fn drop(&mut self) {
        let d = &self.ctx.device;
        // SAFETY: every call waits for its submission, so nothing is in flight; null handles
        // (objects not made) are ignored.
        unsafe {
            d.destroy_fence(self.fence, None);
            d.destroy_command_pool(self.cmd_pool, None);
            d.destroy_descriptor_pool(self.pool, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline(self.jpeg, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

impl GpuConvert {
    /// Conversions on `ctx`'s device. `Unsupported` on a context that runs on Daedalus's
    /// device (it has no queue of its own for this).
    pub fn new(ctx: &GpuContext) -> Result<Self, GpuError> {
        if ctx.inner.own_queue().is_none() {
            return Err(GpuError::Unsupported(
                "conversions on Daedalus's device".into(),
            ));
        }
        let mut c = Self {
            ctx: Arc::clone(&ctx.inner),
            set_layout: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            jpeg: vk::Pipeline::null(),
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            cmd_pool: vk::CommandPool::null(),
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            input: None,
            output: None,
        };
        c.make()?;
        Ok(c)
    }

    /// Makes the objects. On error, `Drop` destroys what was made.
    fn make(&mut self) -> Result<(), GpuError> {
        let d = &self.ctx.device;
        let binding = |b: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let bindings = [binding(0), binding(1)];
        let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: valid create info; `Drop` destroys it.
        self.set_layout = unsafe { d.create_descriptor_set_layout(&info, None) }
            .ctx("vkCreateDescriptorSetLayout")?;
        let set_layouts = [self.set_layout];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .size((ARG_WORDS * 4) as u32)];
        let info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push);
        // SAFETY: as above.
        self.layout =
            unsafe { d.create_pipeline_layout(&info, None) }.ctx("vkCreatePipelineLayout")?;
        self.pipeline = self.pipeline_from(CONVERT)?;
        self.jpeg = self.pipeline_from(crate::jpeg::SPIRV)?;
        let sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::STORAGE_BUFFER,
            descriptor_count: 2,
        }];
        let info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&sizes);
        // SAFETY: as above.
        self.pool =
            unsafe { d.create_descriptor_pool(&info, None) }.ctx("vkCreateDescriptorPool")?;
        let info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.pool)
            .set_layouts(&set_layouts);
        // SAFETY: as above.
        self.set = unsafe { d.allocate_descriptor_sets(&info) }.ctx("vkAllocateDescriptorSets")?[0];
        let info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(self.ctx.queue_family);
        // SAFETY: as above.
        self.cmd_pool = unsafe { d.create_command_pool(&info, None) }.ctx("vkCreateCommandPool")?;
        let info = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: as above.
        self.cmd = unsafe { d.allocate_command_buffers(&info) }.ctx("vkAllocateCommandBuffers")?[0];
        // SAFETY: as above.
        self.fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None) }
            .ctx("vkCreateFence")?;
        Ok(())
    }

    /// A compute pipeline of `spv` on the layout.
    fn pipeline_from(&self, spv: &[u8]) -> Result<vk::Pipeline, GpuError> {
        let d = &self.ctx.device;
        let words = spirv(spv);
        let info = vk::ShaderModuleCreateInfo::default().code(&words);
        // SAFETY: SPIR-V made by glslc for Vulkan 1.2.
        let module = unsafe { d.create_shader_module(&info, None) }.ctx("vkCreateShaderModule")?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(c"main");
        let infos = [vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(self.layout)];
        // SAFETY: valid create info; the module is not needed after this.
        let made = unsafe { d.create_compute_pipelines(vk::PipelineCache::null(), &infos, None) };
        // SAFETY: as above.
        unsafe { d.destroy_shader_module(module, None) };
        Ok(made.map_err(|(_, e)| GpuError::Vulkan {
            call: "vkCreateComputePipelines",
            result: e,
        })?[0])
    }

    /// Makes the buffers hold `input` and `output` bytes, growing them (and rebinding the
    /// descriptor set) only when they do not.
    pub(crate) fn grow(&mut self, input: usize, output: usize) -> Result<(), GpuError> {
        let need_in = self
            .input
            .as_ref()
            .is_none_or(|b| (b.size as usize) < input);
        let need_out = self
            .output
            .as_ref()
            .is_none_or(|b| (b.size as usize) < output);
        if need_in {
            let size = input.next_multiple_of(4096) as u64;
            self.input = Some(Buffer::new(&self.ctx, size, Place::Upload, false)?);
        }
        if need_out {
            let size = output.next_multiple_of(4096) as u64;
            self.output = Some(Buffer::new(&self.ctx, size, Place::Readback, false)?);
        }
        if need_in || need_out {
            self.bind();
        }
        Ok(())
    }

    /// Points the descriptor set at the two buffers (the set is not in use).
    fn bind(&self) {
        let (Some(input), Some(output)) = (&self.input, &self.output) else {
            return;
        };
        let ii = [vk::DescriptorBufferInfo::default()
            .buffer(input.buffer)
            .range(vk::WHOLE_SIZE)];
        let oi = [vk::DescriptorBufferInfo::default()
            .buffer(output.buffer)
            .range(vk::WHOLE_SIZE)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(self.set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&ii),
            vk::WriteDescriptorSet::default()
                .dst_set(self.set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&oi),
        ];
        // SAFETY: valid writes to a set not in use.
        unsafe { self.ctx.device.update_descriptor_sets(&writes, &[]) };
    }

    /// Stages `planes` (one after another in the input buffer), dispatches `a` over its grid
    /// and copies the `rows` output rows of `row` bytes into `dst`.
    fn run(
        &mut self,
        a: Args,
        planes: &[Plane<'_>],
        dst: ImageMut<'_>,
        (row, rows): (usize, usize),
    ) -> Result<(), GpuError> {
        assert!(!planes.is_empty() && planes.len() <= 3);
        for p in planes {
            check_rows("source", p.data.len(), p.stride, p.row, p.rows);
        }
        check_rows("destination", dst.data.len(), dst.stride, row, rows);
        let (w, h) = a.grid;
        if w == 0 || h == 0 {
            return Ok(());
        }
        let mut stride = [0usize; 3];
        let mut off = [0usize; 3];
        let mut total = 0;
        for (i, p) in planes.iter().enumerate() {
            stride[i] = p.stride;
            off[i] = total;
            total += p.data.len();
        }
        let in_height = planes[0].rows;
        self.grow(total, dst.stride * rows)?;
        let input = self.input.as_mut().expect("grown");
        for (p, &o) in planes.iter().zip(&off) {
            input.write(o, p.data);
        }
        input.flush()?;
        let words = [
            a.op as u32,
            w as u32,
            h as u32,
            stride[0] as u32,
            off[1] as u32,
            stride[1] as u32,
            off[2] as u32,
            stride[2] as u32,
            dst.stride as u32,
            a.bpp as u32,
            a.flags,
            in_height as u32,
            a.matrix[0] as u32,
            a.matrix[1] as u32,
            a.matrix[2] as u32,
            a.matrix[3] as u32,
            a.matrix[4] as u32,
            a.matrix[5] as u32,
        ];
        let mut bytes = [0u8; ARG_WORDS * 4];
        for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        let groups = (w.div_ceil(8) as u32, h.div_ceil(8) as u32);
        self.dispatch(self.pipeline, &bytes, groups)?;
        let output = self.output.as_ref().expect("grown");
        output.invalidate()?;
        let out = output.bytes();
        for y in 0..rows {
            let at = y * dst.stride;
            dst.data[at..][..row].copy_from_slice(&out[at..][..row]);
        }
        Ok(())
    }

    /// Records a dispatch of `pipeline` with `args` as push constants, submits it and waits.
    pub(crate) fn dispatch(
        &self,
        pipeline: vk::Pipeline,
        args: &[u8],
        groups: (u32, u32),
    ) -> Result<(), GpuError> {
        let d = &self.ctx.device;
        let Some(queue) = self.ctx.own_queue() else {
            return Err(GpuError::Unsupported(
                "conversions on Daedalus's device".into(),
            ));
        };
        let cmd = self.cmd;
        // SAFETY: the command buffer is idle (the last call waited on its fence); every
        // object recorded is alive until the wait below returns.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .ctx("vkResetCommandBuffer")?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            d.begin_command_buffer(cmd, &begin)
                .ctx("vkBeginCommandBuffer")?;
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline);
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.layout,
                0,
                &[self.set],
                &[],
            );
            d.cmd_push_constants(cmd, self.layout, vk::ShaderStageFlags::COMPUTE, 0, args);
            d.cmd_dispatch(cmd, groups.0, groups.1, 1);
            let m = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)];
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &m,
                &[],
                &[],
            );
            d.end_command_buffer(cmd).ctx("vkEndCommandBuffer")?;
            let cmds = [cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            {
                let queue = queue.lock().unwrap_or_else(|e| e.into_inner());
                d.queue_submit(*queue, &submit, self.fence)
                    .ctx("vkQueueSubmit")?;
            }
            let waited = d.wait_for_fences(&[self.fence], true, u64::MAX);
            d.reset_fences(&[self.fence]).ctx("vkResetFences")?;
            waited.ctx("vkWaitForFences")?;
        }
        Ok(())
    }

    /// BGR ↔ RGB (24-bit): bytes 0 and 2 of each pixel swapped.
    pub fn swap_rb24(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
    ) -> Result<(), GpuError> {
        let p = plane(src, 3 * w, h);
        self.run(Args::new(Op::SwapRb24, (w, h)), &[p], dst, (3 * w, h))
    }

    /// BGRA ↔ RGBA (32-bit): bytes 0 and 2 swapped, byte 3 kept.
    pub fn swap_rb32(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
    ) -> Result<(), GpuError> {
        let p = plane(src, 4 * w, h);
        self.run(Args::new(Op::SwapRb32, (w, h)), &[p], dst, (4 * w, h))
    }

    /// 4-byte pixels to RGB24, dropping byte 3; `swap` also exchanges bytes 0 and 2 (BGRX).
    pub fn x32_to_rgb24(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
        swap: bool,
    ) -> Result<(), GpuError> {
        let mut a = Args::new(Op::X32Rgb24, (w, h));
        a.flags = if swap { SWAP } else { 0 };
        let p = plane(src, 4 * w, h);
        self.run(a, &[p], dst, (3 * w, h))
    }

    /// Luma of colour pixels: `(77 R + 150 G + 29 B) >> 8`, one byte per pixel.
    pub fn rgb_to_luma(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
        layout: ColorLayout,
    ) -> Result<(), GpuError> {
        let bpp = layout.bytes_per_pixel();
        let mut a = Args::new(Op::Luma, (w, h));
        a.bpp = bpp;
        a.flags = if layout.rgb_offsets()[0] == 2 {
            SWAP
        } else {
            0
        };
        let p = plane(src, bpp * w, h);
        self.run(a, &[p], dst, (w, h))
    }

    /// RGB24 to RGBA with alpha 255.
    pub fn rgb24_to_rgba(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
    ) -> Result<(), GpuError> {
        let p = plane(src, 3 * w, h);
        self.run(Args::new(Op::Rgb24Rgba, (w, h)), &[p], dst, (4 * w, h))
    }

    /// Grey to RGB24: each byte three times.
    pub fn gray8_to_rgb24(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
    ) -> Result<(), GpuError> {
        let p = plane(src, w, h);
        self.run(Args::new(Op::Gray8Rgb24, (w, h)), &[p], dst, (3 * w, h))
    }

    /// Little-endian 16-bit grey to RGB24: the high byte three times.
    pub fn gray16le_to_rgb24(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
    ) -> Result<(), GpuError> {
        let p = plane(src, 2 * w, h);
        self.run(Args::new(Op::Gray16Rgb24, (w, h)), &[p], dst, (3 * w, h))
    }

    /// Little-endian 16-bit RGB (BGR with `swap`) to RGB24: the high byte of each channel.
    pub fn rgb48le_to_rgb24(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
        swap: bool,
    ) -> Result<(), GpuError> {
        let mut a = Args::new(Op::Rgb48Rgb24, (w, h));
        a.flags = if swap { SWAP } else { 0 };
        let p = plane(src, 6 * w, h);
        self.run(a, &[p], dst, (3 * w, h))
    }

    /// YUYV to luma: the even bytes.
    pub fn yuyv_to_luma(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
    ) -> Result<(), GpuError> {
        let p = plane(src, 2 * w, h);
        self.run(Args::new(Op::YuyvLuma, (w, h)), &[p], dst, (w, h))
    }

    /// YUYV to NV12's chroma: each output byte (`w` per row, `h.div_ceil(2)` rows) is the
    /// average of the odd bytes of luma rows `2r` and `2r + 1` (the last row pairs with
    /// itself), rounding half up.
    pub fn yuyv_to_nv12_chroma(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
    ) -> Result<(), GpuError> {
        let rows = h.div_ceil(2);
        let p = plane(src, 2 * w, h);
        self.run(Args::new(Op::YuyvChroma, (w, rows)), &[p], dst, (w, rows))
    }

    /// Pixel order reversed in each row (`dst[i] = src[width - 1 - i]`), `bpp` bytes a pixel
    /// (1 to 4).
    pub fn reverse_rows(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        size: (usize, usize),
        bpp: usize,
    ) -> Result<(), GpuError> {
        let mirror = Orientation {
            transpose: false,
            flip_x: true,
            flip_y: false,
        };
        self.transform(src, dst, size, bpp, mirror)
    }

    /// Reorients a frame of `size` = (width, height) `bpp`-byte pixels as
    /// [`styx_core::simd::transform_packed`] does: a transpose (90° rotation) when
    /// `orientation.transpose`, then the mirrors. The output is (height, width) when
    /// transposed.
    pub fn transform(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
        bpp: usize,
        orientation: Orientation,
    ) -> Result<(), GpuError> {
        assert!((1..=4).contains(&bpp), "1 to 4 bytes per pixel");
        let (wo, ho) = if orientation.transpose {
            (h, w)
        } else {
            (w, h)
        };
        let mut flags = 0;
        if orientation.flip_x {
            flags |= SWAP;
        }
        if orientation.flip_y {
            flags |= FLIP_Y;
        }
        if orientation.transpose {
            flags |= TRANSPOSE;
        }
        let mut a = Args::new(Op::Transform, (wo, ho));
        a.bpp = bpp;
        a.flags = flags;
        let p = plane(src, w * bpp, h);
        self.run(a, &[p], dst, (wo * bpp, ho))
    }

    /// NV12 to RGB24: `y` (`w` bytes a row) and `uv` (interleaved, `2 * w.div_ceil(2)` bytes
    /// a row, `h.div_ceil(2)` rows).
    pub fn nv12_to_rgb(
        &mut self,
        y: Image<'_>,
        uv: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
        m: &YuvMatrix,
    ) -> Result<(), GpuError> {
        let planes = [plane(y, w, h), plane(uv, 2 * w.div_ceil(2), h.div_ceil(2))];
        self.run(Args::yuv(Op::Nv12Rgb, (w, h), m), &planes, dst, (3 * w, h))
    }

    /// YUYV to RGB24: groups of Y0 U Y1 V, `w.div_ceil(2)` of them a row.
    pub fn yuyv_to_rgb(
        &mut self,
        src: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
        m: &YuvMatrix,
    ) -> Result<(), GpuError> {
        let p = plane(src, 4 * w.div_ceil(2), h);
        self.run(Args::yuv(Op::YuyvRgb, (w, h), m), &[p], dst, (3 * w, h))
    }

    /// I420 to RGB24 (YV12 with `u` and `v` swapped): `y` as NV12's, `u` and `v` each
    /// `w.div_ceil(2)` bytes a row, `h.div_ceil(2)` rows.
    pub fn i420_to_rgb(
        &mut self,
        y: Image<'_>,
        u: Image<'_>,
        v: Image<'_>,
        dst: ImageMut<'_>,
        (w, h): (usize, usize),
        m: &YuvMatrix,
    ) -> Result<(), GpuError> {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let planes = [plane(y, w, h), plane(u, cw, ch), plane(v, cw, ch)];
        self.run(Args::yuv(Op::I420Rgb, (w, h), m), &planes, dst, (3 * w, h))
    }
}
