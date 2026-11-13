//! [`GpuIsp`]: `styx-soft`'s API on the GPU.

use std::sync::Arc;
use std::time::Duration;

use ash::vk;
use styx_soft::{IspError, IspParams, IspStats, OutputBuffers, RawFormat, Scale, Window};

use crate::context::{DeviceSelect, GpuContext, Inner};
use crate::error::GpuError;
use crate::input::Input;
use crate::kernels::{Kernels, MAX_PASSES, Pass, Plan};
use crate::layout::{self, Layout, OutputKind};
use crate::lease::{GpuLease, LeaseHeld};
use crate::memory::{Buffer, Place};
use crate::params::{Block, Spare, Tables};
use crate::stages::{Planes, Stages, set_output, set_planes};
use crate::stats;

/// Bytes of parameter block per slot of the parameter buffer: slot 0 is the picture's (and the
/// statistics'), slot 1 the output stage's when the output stages run.
const PARAM_SLOT: usize = 512;
const _: () = assert!(std::mem::size_of::<Block>() <= PARAM_SLOT);

/// A dma-buf registered with a [`GpuIsp`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImportId(pub(crate) usize);

/// A frame processed into one of the ISP's exportable output buffers
/// ([`GpuIsp::process_to_export`]).
#[derive(Debug)]
pub struct ExportedFrame {
    /// The ring slot holding it: it is overwritten when the ring comes round again
    /// ([`GpuIsp::set_export_buffers`]).
    pub slot: usize,
    pub layout: Layout,
    pub stats: Option<IspStats>,
}

pub(crate) struct Slot {
    pub(crate) buffer: Buffer,
    pub(crate) set: vk::DescriptorSet,
}

pub(crate) struct Imported {
    pub(crate) buffer: Buffer,
    /// The set the shaders read it through (same-memory devices).
    pub(crate) set: Option<vk::DescriptorSet>,
}

/// The software ISP's pipeline on a Vulkan device: the same parameters ([`IspParams`]),
/// outputs ([`OutputBuffers`]) and statistics ([`IspStats`]) as [`styx_soft::SoftIsp`],
/// computed in its integer arithmetic bit for bit (the tone curve as a table or as quadratics,
/// `IntPolyTone`, both bit for bit with the CPU's).
///
/// Each frame is one submission (statistics histogram cleared, input copied in on devices
/// with their own memory, the picture, the statistics, results copied out) and one wait on
/// its fence; buffers, descriptor sets and the command buffer are kept from frame to frame.
pub struct GpuIsp {
    pub(crate) ctx: Arc<Inner>,
    context: GpuContext,
    pub(crate) k: Kernels,
    format: RawFormat,
    params: IspParams,
    pub(crate) tables: Tables,
    /// The product's GPU lease ([`GpuLease`]), asked before each frame's GPU work and released
    /// after it. `None` costs nothing.
    lease: Option<Arc<dyn GpuLease>>,
    statistics: bool,
    pub(crate) separate: bool,
    stride_align: usize,
    params_buf: Buffer,
    lut_buf: Buffer,
    lsc_buf: Buffer,
    stats_dev: Buffer,
    stats_host: Option<Buffer>,
    /// The tables' buffer (the sigma thresholds of the output stages).
    post_buf: Buffer,
    /// Set 0 per parameter slot (see [`PARAM_SLOT`]).
    set0: [vk::DescriptorSet; 2],
    planes: Option<Planes>,
    staging_in: Option<Slot>,
    dev_in: Option<Slot>,
    pub(crate) imports: Vec<Option<Imported>>,
    /// The set [`Input::Buffer`] frames are read through (same-memory devices).
    ext_set: Option<vk::DescriptorSet>,
    host_out: Option<Slot>,
    dev_out: Option<Slot>,
    pub(crate) ring: Vec<Slot>,
    pub(crate) ring_len: usize,
    pub(crate) ring_next: usize,
    gpu_time: Option<Duration>,
    /// Statistics handed back ([`Self::recycle_stats`]), written into next.
    spare_stats: Option<IspStats>,
    spare_tables: Spare,
}

impl std::fmt::Debug for GpuIsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuIsp")
            .field("device", &self.ctx.info.name)
            .field("format", &self.format)
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

/// Bytes the raw frame occupies with `stride`.
fn frame_bytes(format: &RawFormat, stride: usize) -> usize {
    stride * (format.height as usize - 1) + format.min_stride()
}

impl GpuIsp {
    /// An ISP on the default device ([`DeviceSelect::Auto`]).
    pub fn new(format: RawFormat, params: IspParams) -> Result<Self, GpuError> {
        Self::with_context(&GpuContext::open(DeviceSelect::Auto)?, format, params)
    }

    /// An ISP on `context`'s device.
    pub fn with_context(
        context: &GpuContext,
        format: RawFormat,
        params: IspParams,
    ) -> Result<Self, GpuError> {
        let mut spare_tables = Spare::default();
        let tables = Tables::new(&format, &params, None, &mut spare_tables)?;
        let ctx = Arc::clone(&context.inner);
        let k = Kernels::new(&ctx)?;
        let separate = ctx.info.separate_memory;
        let params_buf = Buffer::new(&ctx, 4096, Place::Upload, false)?;
        let lut_buf = Buffer::new(&ctx, 4 * 4096, Place::Upload, false)?;
        let lsc_buf = Buffer::new(&ctx, 4, Place::Upload, false)?;
        let stats_place = if separate {
            Place::Device
        } else {
            Place::Readback
        };
        let stats_dev = Buffer::new(&ctx, 4, stats_place, false)?;
        let post_buf = Buffer::new(&ctx, 4 * 256, Place::Upload, false)?;
        let mut isp = Self {
            context: context.clone(),
            ctx,
            k,
            format,
            params: params.clone(),
            tables,
            lease: None,
            statistics: true,
            separate,
            stride_align: 1,
            params_buf,
            lut_buf,
            lsc_buf,
            stats_dev,
            stats_host: None,
            post_buf,
            set0: [vk::DescriptorSet::null(); 2],
            planes: None,
            staging_in: None,
            dev_in: None,
            imports: Vec::new(),
            ext_set: None,
            host_out: None,
            dev_out: None,
            ring: Vec::new(),
            ring_len: 4,
            ring_next: 0,
            gpu_time: None,
            spare_stats: None,
            spare_tables,
        };
        isp.set0 = isp.make_set0()?;
        isp.upload_tables()?;
        Ok(isp)
    }

    /// Set 0 for each parameter slot: the parameters from the slot's offset, the tables.
    fn make_set0(&self) -> Result<[vk::DescriptorSet; 2], GpuError> {
        let mut sets = [vk::DescriptorSet::null(); 2];
        for (slot, set) in sets.iter_mut().enumerate() {
            let b = self.set0_bindings(slot);
            *set = self.k.set_at(0, b.len(), |i| b[i])?;
        }
        Ok(sets)
    }

    /// The bindings of set 0 for parameter slot `slot`.
    fn set0_bindings(&self, slot: usize) -> [(vk::Buffer, u64); 5] {
        [
            (self.params_buf.buffer, (slot * PARAM_SLOT) as u64),
            (self.lut_buf.buffer, 0),
            (self.lsc_buf.buffer, 0),
            (self.stats_dev.buffer, 0),
            (self.post_buf.buffer, 0),
        ]
    }

    /// The device in use.
    pub fn context(&self) -> &GpuContext {
        &self.context
    }

    /// Replace the parameters.
    pub fn set_params(&mut self, mut params: IspParams) -> Result<(), GpuError> {
        self.swap_params(&mut params)
    }

    /// [`Self::set_params`], handing the replaced parameters back in `params` (no allocation
    /// when the caller reuses them, as [`styx_soft::SoftIsp::swap_params`]): tables of
    /// parameters gone by are rewritten in place.
    pub fn swap_params(&mut self, params: &mut IspParams) -> Result<(), GpuError> {
        let tables = Tables::new(
            &self.format,
            params,
            Some((&self.tables, &self.params)),
            &mut self.spare_tables,
        )?;
        self.spare_tables
            .keep(std::mem::replace(&mut self.tables, tables));
        std::mem::swap(&mut self.params, params);
        self.upload_tables()
    }

    pub fn params(&self) -> &IspParams {
        &self.params
    }

    pub fn format(&self) -> RawFormat {
        self.format
    }

    /// Whether frames gather the statistics their parameters ask for (default: yes).
    pub fn set_statistics(&mut self, on: bool) {
        self.statistics = on;
    }

    /// Row strides of the GPU's output images are multiples of this (default 1: tight rows).
    /// Consumers importing exported frames may need 64 or 256.
    pub fn set_stride_align(&mut self, align: usize) {
        self.stride_align = align.max(1);
    }

    /// Exportable output buffers in the ring [`Self::process_to_export`] writes (default 4).
    /// A frame's buffer is reused that many frames later.
    pub fn set_export_buffers(&mut self, n: usize) {
        self.ring_len = n.max(1);
        self.ring.truncate(self.ring_len);
        self.ring_next %= self.ring_len;
    }

    /// The output image size at `scale`.
    pub fn output_size(&self, scale: Scale) -> (u32, u32) {
        match scale {
            Scale::Full => (self.format.width, self.format.height),
            Scale::Half => (self.format.width / 2, self.format.height / 2),
        }
    }

    /// Hands a frame's statistics back: the next frame's are written into them (no
    /// allocation once warm).
    pub fn recycle_stats(&mut self, stats: IspStats) {
        self.spare_stats = Some(stats);
    }

    /// GPU time of the last frame (timestamp queries; `None` where the queue has none).
    pub fn gpu_time(&self) -> Option<Duration> {
        self.gpu_time
    }

    /// Process one frame (`stride` bytes per row) into `out`, as
    /// [`styx_soft::SoftIsp::process`]. Returns the statistics when the parameters ask for
    /// them.
    pub fn process(
        &mut self,
        input: &[u8],
        stride: usize,
        scale: Scale,
        out: OutputBuffers<'_>,
    ) -> Result<Option<IspStats>, GpuError> {
        self.process_input(Input::Bytes(input), stride, scale, out)
    }

    /// [`Self::process`] from any [`Input`].
    pub fn process_input(
        &mut self,
        input: Input<'_>,
        stride: usize,
        scale: Scale,
        mut out: OutputBuffers<'_>,
    ) -> Result<Option<IspStats>, GpuError> {
        self.check_input(input, stride)?;
        let kind = OutputKind::of(&out);
        let layout = self.layout(kind, scale)?;
        let dst = layout::destination(&mut out, &layout)?;
        self.ensure_slot(Target::Host, layout.size)?;
        let stats = self.run(input, stride, scale, &layout, Target::Host, Region::Frame)?;
        let host = self.host_out.as_ref().expect("made by ensure_slot");
        host.buffer.invalidate()?;
        layout::copy_out(host.buffer.bytes(), &layout, dst);
        Ok(stats)
    }

    /// Process only `window` of the frame, at `scale`, into `out` (sized
    /// [`Self::window_size`]), as [`styx_soft::SoftIsp::process_window`]: the same pixels
    /// as that window of [`Self::process`]'s picture, bit for bit. No statistics: those of the
    /// whole frame come from [`Self::statistics`] or [`Self::process`].
    pub fn process_window(
        &mut self,
        input: &[u8],
        stride: usize,
        window: Window,
        scale: Scale,
        out: OutputBuffers<'_>,
    ) -> Result<(), GpuError> {
        self.process_window_input(Input::Bytes(input), stride, window, scale, out)
    }

    /// [`Self::process_window`] from any [`Input`].
    pub fn process_window_input(
        &mut self,
        input: Input<'_>,
        stride: usize,
        window: Window,
        scale: Scale,
        mut out: OutputBuffers<'_>,
    ) -> Result<(), GpuError> {
        let Window {
            x,
            y,
            width,
            height,
        } = window;
        let (w, h) = (self.format.width, self.format.height);
        if width == 0
            || height == 0
            || !(x | y | width | height).is_multiple_of(2)
            || x + width > w
            || y + height > h
        {
            return Err(IspError::Unsupported(format!(
                "window {width}x{height} at ({x}, {y}): even origin and size within the \
                 {w}x{h} frame"
            ))
            .into());
        }
        self.check_input(input, stride)?;
        let (ow, oh) = self.window_size(window, scale);
        let kind = OutputKind::of(&out);
        let layout = self.layout_of(kind, ow, oh)?;
        let dst = layout::destination(&mut out, &layout)?;
        self.ensure_slot(Target::Host, layout.size)?;
        self.run(
            input,
            stride,
            scale,
            &layout,
            Target::Host,
            Region::Window { x, y },
        )?;
        let host = self.host_out.as_ref().expect("made by ensure_slot");
        host.buffer.invalidate()?;
        layout::copy_out(host.buffer.bytes(), &layout, dst);
        Ok(())
    }

    /// The output size of [`Self::process_window`] of `window` at `scale`.
    pub fn window_size(&self, window: Window, scale: Scale) -> (u32, u32) {
        match scale {
            Scale::Full => (window.width, window.height),
            Scale::Half => (window.width / 2, window.height / 2),
        }
    }

    /// Only the frame's statistics (as [`styx_soft::SoftIsp::statistics`]): no picture is
    /// made, only the statistics pass runs.
    pub fn statistics(
        &mut self,
        input: &[u8],
        stride: usize,
    ) -> Result<Option<IspStats>, GpuError> {
        self.statistics_input(Input::Bytes(input), stride)
    }

    /// [`Self::statistics`] from any [`Input`].
    pub fn statistics_input(
        &mut self,
        input: Input<'_>,
        stride: usize,
    ) -> Result<Option<IspStats>, GpuError> {
        self.check_input(input, stride)?;
        if !self.statistics || self.tables.stats.is_none() {
            return Ok(None);
        }
        // Nothing is written to it: a slot of any size will do.
        let layout = Layout::new(OutputKind::Luma, 2, 2, self.stride_align);
        self.ensure_slot(Target::Host, layout.size)?;
        self.run(
            input,
            stride,
            Scale::Full,
            &layout,
            Target::Host,
            Region::Stats,
        )
    }

    pub(crate) fn layout(&self, kind: OutputKind, scale: Scale) -> Result<Layout, GpuError> {
        let (ow, oh) = self.output_size(scale);
        self.layout_of(kind, ow, oh)
    }

    /// The layout of a `kind` output of `ow`x`oh`.
    fn layout_of(&self, kind: OutputKind, ow: u32, oh: u32) -> Result<Layout, GpuError> {
        let (ow, oh) = (ow as usize, oh as usize);
        if matches!(kind, OutputKind::Nv12 | OutputKind::I420) && (ow % 2 != 0 || oh % 2 != 0) {
            return Err(
                IspError::Unsupported(format!("4:2:0 output of odd size {ow}x{oh}")).into(),
            );
        }
        Ok(Layout::new(kind, ow, oh, self.stride_align))
    }

    /// Write the parameter-dependent tables (tone, lens shading) and grow the statistics
    /// buffer.
    fn upload_tables(&mut self) -> Result<(), GpuError> {
        let mut rebind = false;
        if let Some(lut) = self.tables.lut.as_ref().filter(|_| self.tables.lut_fresh) {
            self.lut_buf.write_words(lut);
            self.lut_buf.flush()?;
        }
        if let Some(lsc) = self.tables.lsc.as_ref().filter(|_| self.tables.lsc_fresh) {
            let need = 4 * lsc.len() as u64;
            if self.lsc_buf.size < need {
                self.lsc_buf = Buffer::new(&self.ctx, need, Place::Upload, false)?;
                rebind = true;
            }
            self.lsc_buf.write_words(lsc);
            self.lsc_buf.flush()?;
        }
        if let Some(s) = &self.tables.stats {
            let need = stats::buffer_bytes(s) as u64;
            if self.stats_dev.size < need {
                let place = if self.separate {
                    Place::Device
                } else {
                    Place::Readback
                };
                self.stats_dev = Buffer::new(&self.ctx, need, place, false)?;
                rebind = true;
            }
            if self.separate && self.stats_host.as_ref().is_none_or(|b| b.size < need) {
                self.stats_host = Some(Buffer::new(&self.ctx, need, Place::Readback, false)?);
            }
        }
        if let Some(sigma) = self.tables.sigma.as_ref() {
            self.post_buf.write_words(sigma);
            self.post_buf.flush()?;
        }
        if rebind {
            for slot in 0..2 {
                let b = self.set0_bindings(slot);
                self.k.write_set_at(self.set0[slot], b.len(), |i| b[i]);
            }
        }
        Ok(())
    }

    /// The output stages' planes, made on first use (a frame's size, at most).
    fn ensure_planes(&mut self) -> Result<(), GpuError> {
        if self.planes.is_none() {
            let size = 3 * self.format.width as u64 * self.format.height as u64;
            let place = if self.separate {
                Place::Device
            } else {
                Place::Readback
            };
            self.planes = Some(Planes::new(&self.ctx, &self.k, size, place)?);
        }
        Ok(())
    }

    /// Make sure `target` (and the device-side output) holds `size` bytes.
    pub(crate) fn ensure_slot(&mut self, target: Target, size: usize) -> Result<(), GpuError> {
        let size = size as u64;
        if self.separate && self.dev_out.as_ref().is_none_or(|s| s.buffer.size < size) {
            self.dev_out = Some(self.slot(size, Place::Device, false, 2, self.dev_out.as_ref())?);
        }
        match target {
            Target::Host => {
                if self.host_out.as_ref().is_none_or(|s| s.buffer.size < size) {
                    let old = self.host_out.take();
                    self.host_out =
                        Some(self.slot(size, Place::Readback, false, 2, old.as_ref())?);
                }
            }
            Target::Ring(i) => {
                if self.ring.len() <= i || self.ring[i].buffer.size < size {
                    // Room for the largest image (full-size RGB24) so slots never shrink.
                    let full = Layout::new(
                        OutputKind::Rgb24,
                        self.format.width as usize,
                        self.format.height as usize,
                        self.stride_align,
                    );
                    let size = size.max(full.size as u64);
                    let old = (i < self.ring.len()).then(|| self.ring.remove(i));
                    let s = self.slot(size, Place::Readback, true, 2, old.as_ref())?;
                    self.ring.insert(i.min(self.ring.len()), s);
                }
            }
        }
        Ok(())
    }

    /// A buffer with its descriptor set (`old`'s set is reused).
    fn slot(
        &self,
        size: u64,
        place: Place,
        export: bool,
        which: usize,
        old: Option<&Slot>,
    ) -> Result<Slot, GpuError> {
        let buffer = Buffer::new(&self.ctx, size, place, export)?;
        let set = match old {
            Some(o) => {
                self.k.write_set(o.set, &[buffer.buffer]);
                o.set
            }
            None => self.k.set(which, &[buffer.buffer])?,
        };
        Ok(Slot { buffer, set })
    }

    /// The input the shaders read (set, byte offset), with the copy into device memory
    /// when the device has its own (source buffer, offset, bytes).
    fn bind_input(&mut self, input: Input<'_>, bytes: usize) -> Result<BoundInput, GpuError> {
        let (src, offset) = match input {
            Input::Bytes(data) => {
                if self
                    .staging_in
                    .as_ref()
                    .is_none_or(|s| (s.buffer.size as usize) < bytes)
                {
                    let old = self.staging_in.take();
                    self.staging_in =
                        Some(self.slot(bytes as u64, Place::Upload, false, 1, old.as_ref())?);
                }
                let s = self.staging_in.as_mut().expect("made above");
                s.buffer.write(0, &data[..bytes]);
                s.buffer.flush()?;
                if !self.separate {
                    return Ok((s.set, 0, None));
                }
                (s.buffer.buffer, 0)
            }
            Input::Buffer(b) => {
                if b.device != self.ctx.device.handle() {
                    return Err(GpuError::Unsupported("a buffer of another device".into()));
                }
                if b.len < bytes {
                    return Err(IspError::InputTooShort(format!(
                        "buffer of {} bytes past the frame's start, frame of {bytes}",
                        b.len
                    ))
                    .into());
                }
                if !self.separate {
                    // One set, pointed at this frame's buffer: the last frame has completed.
                    let set = match self.ext_set {
                        Some(set) => {
                            self.k.write_set(set, &[b.buffer]);
                            set
                        }
                        None => *self.ext_set.insert(self.k.set(1, &[b.buffer])?),
                    };
                    return Ok((set, b.offset, None));
                }
                (b.buffer, b.offset)
            }
            Input::DmaBuf { id, offset } => {
                let imp = self
                    .imports
                    .get(id.0)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| GpuError::Unsupported(format!("no import {id:?}")))?;
                if (imp.buffer.size as usize) < offset + bytes {
                    return Err(IspError::InputTooShort(format!(
                        "dma-buf of {} bytes, frame of {bytes} at {offset}",
                        imp.buffer.size
                    ))
                    .into());
                }
                if let Some(set) = imp.set {
                    return Ok((set, offset, None));
                }
                (imp.buffer.buffer, offset)
            }
        };
        if self
            .dev_in
            .as_ref()
            .is_none_or(|s| (s.buffer.size as usize) < bytes)
        {
            let old = self.dev_in.take();
            self.dev_in = Some(self.slot(bytes as u64, Place::Device, false, 1, old.as_ref())?);
        }
        let d = self.dev_in.as_ref().expect("made above");
        Ok((d.set, 0, Some((src, offset))))
    }

    /// The input holds a frame with `stride` (as `styx-soft` checks it).
    fn check_input(&self, input: Input<'_>, stride: usize) -> Result<(), GpuError> {
        let row = self.format.min_stride();
        let bytes = frame_bytes(&self.format, stride);
        let len = match input {
            Input::Bytes(data) => data.len(),
            Input::DmaBuf { .. } => usize::MAX,
            Input::Buffer(b) => b.len,
        };
        if stride < row || len < bytes {
            return Err(IspError::InputTooShort(format!(
                "{len} bytes with stride {stride} for {}x{} {:?} (rows of {row} bytes)",
                self.format.width, self.format.height, self.format.packing
            ))
            .into());
        }
        Ok(())
    }

    /// Sets the GPU lease the ISP asks before each frame's GPU work (`None` to clear it).
    pub fn set_gpu_lease(&mut self, lease: Option<Arc<dyn GpuLease>>) {
        self.lease = lease;
    }

    pub(crate) fn run(
        &mut self,
        input: Input<'_>,
        stride: usize,
        scale: Scale,
        layout: &Layout,
        target: Target,
        region: Region,
    ) -> Result<Option<IspStats>, GpuError> {
        self.check_input(input, stride)?;
        // Held until this frame's work is done (`LeaseHeld` releases on drop).
        let _lease = match &self.lease {
            Some(lease) => Some(LeaseHeld::take(Arc::clone(lease))?),
            None => None,
        };
        let bytes = frame_bytes(&self.format, stride);
        // With the output stages, the picture is the whole frame's (at the scale), made into
        // planes; the output stage reads the window from them.
        let post = self.tables.post && region != Region::Stats;
        if post {
            self.ensure_planes()?;
        }
        let (in_set, in_offset, copy_in) = self.bind_input(input, bytes)?;
        let stats = match region {
            Region::Window { .. } => None,
            Region::Frame | Region::Stats => self.tables.stats.as_ref().filter(|_| self.statistics),
        };
        let mut block = self.tables.block;
        block.in_stride = stride as u32;
        block.in_offset = in_offset as u32;
        let (pw, ph) = self.output_size(scale);
        let div = match scale {
            Scale::Full => 1,
            Scale::Half => 2,
        };
        let (wx, wy) = match region {
            Region::Window { x, y } => (x, y),
            Region::Frame | Region::Stats => (0, 0),
        };
        (block.pic_w, block.pic_h) = (pw, ph);
        (block.crop_x, block.crop_y) = (wx / div, wy / div);
        let mut out = block;
        set_output(&mut out, layout);
        if post {
            (block.win_x, block.win_y) = (0, 0);
            set_planes(&mut block, pw as usize, ph as usize);
        } else {
            (block.win_x, block.win_y) = (wx, wy);
            set_output(&mut block, layout);
        }
        self.params_buf.write(0, block.bytes());
        if post {
            self.params_buf.write(PARAM_SLOT, out.bytes());
        }
        self.params_buf.flush()?;
        let (out_set, out_buf) = match target {
            Target::Host => {
                let s = self.host_out.as_ref().expect("ensured");
                (s.set, s.buffer.buffer)
            }
            Target::Ring(i) => (self.ring[i].set, self.ring[i].buffer.buffer),
        };
        let (shader_out, copy_out) = match &self.dev_out {
            Some(d) if self.separate => (d.set, Some((d.buffer.buffer, out_buf))),
            _ => (out_set, None),
        };
        let picture = self.picture_pipeline(scale);
        let out_groups = (
            (layout.width as u32).div_ceil(32),
            (layout.height as u32).div_ceil(16),
        );
        let passes = if post {
            let block = &self.tables.block;
            Stages {
                kernels: &self.k,
                planes: self.planes.as_ref().expect("made above"),
                set0: self.set0,
                input: in_set,
                output: shader_out,
                picture,
                size: (pw, ph),
                out_groups,
                denoise: block.denoise_luma != 0 || block.denoise_chroma != 0,
                sharpen: block.sharpen_on != 0,
            }
            .passes()
        } else {
            let mut passes = [None; MAX_PASSES];
            if region != Region::Stats {
                passes[0] = Some(Pass {
                    sets: [self.set0[0], in_set, shader_out],
                    pipeline: picture,
                    groups: out_groups,
                });
            }
            passes
        };
        let plan = Plan {
            passes,
            stats_sets: [self.set0[0], in_set, shader_out],
            copy_in: copy_in.map(|(b, o)| (b, o as u64, bytes as u64)),
            copy_out: copy_out.map(|(s, d)| (s, d, layout.size as u64)),
            stats: stats.map(|s| {
                let host = self.stats_host.as_ref().map(|h| h.buffer);
                (
                    (s.config.zones_x, s.config.zones_y),
                    stats::histogram_offset(s) as u64,
                    4 * s.config.histogram_bins as u64,
                    stats::buffer_bytes(s) as u64,
                    host,
                )
            }),
        };
        let dev_in = self.dev_in.as_ref().map(|d| d.buffer.buffer);
        self.gpu_time = self.k.run(&plan, self.stats_dev.buffer, dev_in)?;
        Ok(match stats {
            Some(s) => {
                let buf = self.stats_host.as_ref().unwrap_or(&self.stats_dev);
                buf.invalidate()?;
                let mut out = self.spare_stats.take().unwrap_or_default();
                stats::read_into(buf.bytes(), s, self.tables.channel_gains, &mut out);
                Some(out)
            }
            None => None,
        })
    }
}

/// The input set the shaders read, the byte offset of the frame in it, and the buffer and
/// offset to copy it from into device memory (devices with their own).
type BoundInput = (vk::DescriptorSet, usize, Option<(vk::Buffer, usize)>);

/// What a pass makes of the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Region {
    /// The whole picture, with the statistics the parameters ask for.
    Frame,
    /// The picture of the window at frame position (`x`, `y`) (even), no statistics.
    Window { x: u32, y: u32 },
    /// The statistics alone: no picture (no picture workgroups).
    Stats,
}

/// Where a frame's output goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// The ISP's own readback buffer, copied into the caller's buffers.
    Host,
    /// Export ring slot `i`.
    Ring(usize),
}

impl GpuIsp {
    /// The pipeline of a picture at `scale` (integer arithmetic: the one `styx-soft` runs bit for
    /// bit).
    pub(crate) fn picture_pipeline(&self, scale: Scale) -> vk::Pipeline {
        match scale {
            Scale::Full => self.k.full,
            Scale::Half => self.k.half,
        }
    }

    /// The arithmetic the pictures run in: [`styx_soft::Arithmetic::IntPolyTone`] with the tone
    /// curve as quadratics, else [`styx_soft::Arithmetic::Int`].
    pub fn arithmetic(&self) -> styx_soft::Arithmetic {
        if self.tables.block.tone_lut == 2 {
            styx_soft::Arithmetic::IntPolyTone
        } else {
            styx_soft::Arithmetic::Int
        }
    }
}
