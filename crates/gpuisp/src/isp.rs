//! [`GpuIsp`]: `styx-softisp`'s API on the GPU.

use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use ash::vk;
use styx_softisp::{Arithmetic, IspError, IspParams, IspStats, OutputBuffers, RawFormat, Scale};

use crate::context::{DeviceSelect, GpuContext, Inner};
use crate::error::GpuError;
use crate::kernels::{Kernels, Plan};
use crate::layout::{self, Layout, OutputKind};
use crate::memory::{Buffer, Place};
use crate::params::Tables;
use crate::stats;

/// A raw frame for [`GpuIsp::process_input`].
#[derive(Clone, Copy, Debug)]
pub enum Input<'a> {
    /// Bytes in memory: copied into a buffer the GPU reads (one `memcpy`).
    Bytes(&'a [u8]),
    /// A dma-buf registered with [`GpuIsp::import_dmabuf`], the frame starting `offset` bytes
    /// in: read by the GPU in place (zero-copy).
    DmaBuf { id: ImportId, offset: usize },
}

/// A dma-buf registered with a [`GpuIsp`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImportId(usize);

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

struct Slot {
    buffer: Buffer,
    set: vk::DescriptorSet,
}

struct Imported {
    buffer: Buffer,
    /// The set the shaders read it through (same-memory devices).
    set: Option<vk::DescriptorSet>,
}

/// The software ISP's pipeline on a Vulkan device: the same parameters ([`IspParams`]),
/// outputs ([`OutputBuffers`]) and statistics ([`IspStats`]) as [`styx_softisp::SoftIsp`],
/// computed in its integer arithmetic ([`Arithmetic::Int`]) bit for bit.
///
/// Each frame is one submission (statistics histogram cleared, input copied in on devices
/// with their own memory, the picture, the statistics, results copied out) and one wait on
/// its fence; buffers, descriptor sets and the command buffer are kept from frame to frame.
pub struct GpuIsp {
    ctx: Arc<Inner>,
    context: GpuContext,
    k: Kernels,
    format: RawFormat,
    params: IspParams,
    tables: Tables,
    statistics: bool,
    separate: bool,
    stride_align: usize,
    params_buf: Buffer,
    lut_buf: Buffer,
    lsc_buf: Buffer,
    stats_dev: Buffer,
    stats_host: Option<Buffer>,
    set0: vk::DescriptorSet,
    staging_in: Option<Slot>,
    dev_in: Option<Slot>,
    imports: Vec<Option<Imported>>,
    host_out: Option<Slot>,
    dev_out: Option<Slot>,
    ring: Vec<Slot>,
    ring_len: usize,
    ring_next: usize,
    gpu_time: Option<Duration>,
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
        let tables = Tables::new(&format, &params, None)?;
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
        let set0 = k.set(
            0,
            &[
                params_buf.buffer,
                lut_buf.buffer,
                lsc_buf.buffer,
                stats_dev.buffer,
            ],
        )?;
        let mut isp = Self {
            context: context.clone(),
            ctx,
            k,
            format,
            params: params.clone(),
            tables,
            statistics: true,
            separate,
            stride_align: 1,
            params_buf,
            lut_buf,
            lsc_buf,
            stats_dev,
            stats_host: None,
            set0,
            staging_in: None,
            dev_in: None,
            imports: Vec::new(),
            host_out: None,
            dev_out: None,
            ring: Vec::new(),
            ring_len: 4,
            ring_next: 0,
            gpu_time: None,
        };
        isp.upload_tables()?;
        Ok(isp)
    }

    /// The device in use.
    pub fn context(&self) -> &GpuContext {
        &self.context
    }

    /// Replace the parameters.
    pub fn set_params(&mut self, params: IspParams) -> Result<(), GpuError> {
        self.tables = Tables::new(&self.format, &params, Some(&self.tables))?;
        self.params = params;
        self.upload_tables()
    }

    pub fn params(&self) -> &IspParams {
        &self.params
    }

    pub fn format(&self) -> RawFormat {
        self.format
    }

    /// Always [`Arithmetic::Int`]: the GPU computes the integer arithmetic exactly, whatever
    /// [`IspParams::arithmetic`] asks.
    pub fn arithmetic(&self) -> Arithmetic {
        Arithmetic::Int
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

    /// GPU time of the last frame (timestamp queries; `None` where the queue has none).
    pub fn gpu_time(&self) -> Option<Duration> {
        self.gpu_time
    }

    /// Process one frame (`stride` bytes per row) into `out`, as
    /// [`styx_softisp::SoftIsp::process`]. Returns the statistics when the parameters ask for
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
        let stats = self.run(input, stride, scale, &layout, Target::Host)?;
        let host = self.host_out.as_ref().expect("made by ensure_slot");
        host.buffer.invalidate()?;
        layout::copy_out(host.buffer.bytes(), &layout, dst);
        Ok(stats)
    }

    /// Process one frame into the next buffer of the export ring and leave it there:
    /// [`Self::export_dmabuf`] hands it to other devices or processes, [`Self::exported`]
    /// reads it on the CPU.
    pub fn process_to_export(
        &mut self,
        input: Input<'_>,
        stride: usize,
        scale: Scale,
        kind: OutputKind,
    ) -> Result<ExportedFrame, GpuError> {
        if self.ctx.external_fd.is_none() {
            return Err(GpuError::Unsupported("dma-buf export".into()));
        }
        let layout = self.layout(kind, scale)?;
        let slot = self.ring_next;
        self.ensure_slot(Target::Ring(slot), layout.size)?;
        let stats = self.run(input, stride, scale, &layout, Target::Ring(slot))?;
        self.ring_next = (slot + 1) % self.ring_len;
        Ok(ExportedFrame {
            slot,
            layout,
            stats,
        })
    }

    /// A dma-buf of export ring slot `slot` (a new file each call).
    pub fn export_dmabuf(&self, slot: usize) -> Result<OwnedFd, GpuError> {
        let s = self
            .ring
            .get(slot)
            .ok_or_else(|| GpuError::Unsupported(format!("no export slot {slot}")))?;
        s.buffer.export_dmabuf()
    }

    /// The bytes of export ring slot `slot`.
    pub fn exported(&self, slot: usize) -> Result<&[u8], GpuError> {
        let s = self
            .ring
            .get(slot)
            .ok_or_else(|| GpuError::Unsupported(format!("no export slot {slot}")))?;
        s.buffer.invalidate()?;
        Ok(s.buffer.bytes())
    }

    /// Register a dma-buf holding raw frames (`size` bytes; for example a capture buffer)
    /// so that frames in it are read in place ([`Input::DmaBuf`]). The file is duplicated;
    /// the caller keeps its own.
    pub fn import_dmabuf(&mut self, fd: BorrowedFd<'_>, size: usize) -> Result<ImportId, GpuError> {
        let buffer = Buffer::import_dmabuf(&self.ctx, fd, size as u64)?;
        let set = if self.separate {
            None
        } else {
            Some(self.k.set(1, &[buffer.buffer])?)
        };
        let entry = Some(Imported { buffer, set });
        let id = match self.imports.iter().position(Option::is_none) {
            Some(i) => {
                self.imports[i] = entry;
                i
            }
            None => {
                self.imports.push(entry);
                self.imports.len() - 1
            }
        };
        Ok(ImportId(id))
    }

    /// Forget an imported dma-buf.
    pub fn release_import(&mut self, id: ImportId) {
        if let Some(Some(i)) = self.imports.get_mut(id.0).map(Option::take)
            && let Some(set) = i.set
        {
            self.k.free_set(set);
        }
    }

    fn layout(&self, kind: OutputKind, scale: Scale) -> Result<Layout, GpuError> {
        let (ow, oh) = self.output_size(scale);
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
        if rebind {
            self.k.write_set(
                self.set0,
                &[
                    self.params_buf.buffer,
                    self.lut_buf.buffer,
                    self.lsc_buf.buffer,
                    self.stats_dev.buffer,
                ],
            );
        }
        Ok(())
    }

    /// Make sure `target` (and the device-side output) holds `size` bytes.
    fn ensure_slot(&mut self, target: Target, size: usize) -> Result<(), GpuError> {
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

    /// The input holds a frame with `stride` (as `styx-softisp` checks it).
    fn check_input(&self, input: Input<'_>, stride: usize) -> Result<(), GpuError> {
        let row = self.format.min_stride();
        let bytes = frame_bytes(&self.format, stride);
        let len = match input {
            Input::Bytes(data) => data.len(),
            Input::DmaBuf { .. } => usize::MAX,
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

    fn run(
        &mut self,
        input: Input<'_>,
        stride: usize,
        scale: Scale,
        layout: &Layout,
        target: Target,
    ) -> Result<Option<IspStats>, GpuError> {
        self.check_input(input, stride)?;
        let bytes = frame_bytes(&self.format, stride);
        let (in_set, in_offset, copy_in) = self.bind_input(input, bytes)?;
        let stats = self.tables.stats.clone().filter(|_| self.statistics);
        let mut block = self.tables.block;
        block.in_stride = stride as u32;
        block.in_offset = in_offset as u32;
        block.out_kind = layout.kind.code();
        block.out_width = layout.width as u32;
        block.out_height = layout.height as u32;
        for (i, p) in layout.planes.iter().enumerate() {
            block.out_offset[i] = p.offset as u32;
            block.out_stride[i] = p.stride as u32;
        }
        self.params_buf.write(0, block.bytes());
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
        let plan = Plan {
            sets: [self.set0, in_set, shader_out],
            pipeline: match scale {
                Scale::Full => self.k.full,
                Scale::Half => self.k.half,
            },
            groups: match scale {
                Scale::Full => (
                    self.format.width.div_ceil(32),
                    self.format.height.div_ceil(16),
                ),
                Scale::Half => (
                    (layout.width as u32).div_ceil(32),
                    (layout.height as u32).div_ceil(16),
                ),
            },
            copy_in: copy_in.map(|(b, o)| (b, o as u64, bytes as u64)),
            copy_out: copy_out.map(|(s, d)| (s, d, layout.size as u64)),
            stats: stats.as_ref().map(|s| {
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
        Ok(match &stats {
            Some(s) => {
                let buf = self.stats_host.as_ref().unwrap_or(&self.stats_dev);
                buf.invalidate()?;
                Some(stats::read(buf.bytes(), s, self.tables.channel_gains))
            }
            None => None,
        })
    }
}

/// The input set the shaders read, the byte offset of the frame in it, and the buffer and
/// offset to copy it from into device memory (devices with their own).
type BoundInput = (vk::DescriptorSet, usize, Option<(vk::Buffer, usize)>);

/// Where a frame's output goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// The ISP's own readback buffer, copied into the caller's buffers.
    Host,
    /// Export ring slot `i`.
    Ring(usize),
}
