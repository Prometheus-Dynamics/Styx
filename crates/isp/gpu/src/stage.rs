//! The ISP as a stage between frames: a raw [`FrameLease`] in, a caller-acquired picture
//! [`FrameLease`] out, statistics into a caller's [`styx_aaa::Statistics`].
//!
//! The raw input is read where it lies (no copy); the output is written into the lease the
//! caller acquired. Buffers are sized once per geometry; a frame after warm-up allocates nothing
//! when the caller reuses its statistics.

use std::os::fd::AsRawFd;
use std::string::String;
use std::sync::Arc;
use std::vec::Vec;

use styx_aaa::{ColourZone, Statistics, ZoneGrid};
use styx_format::FourCc;
use styx_frame::{FrameLease, MemoryKind, PlaneLayout};
use styx_settings::IspSettings;
use styx_soft::{IspError, IspStats, Options, OutputBuffers, RawFormat, Scale, params_for};

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::input::Input;
use crate::isp::{GpuIsp, ImportId};
use crate::lease::GpuLease;

/// Why a frame could not go through the stage.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StageError {
    #[error("the input lease is not readable from the CPU")]
    NotReadable,
    #[error("the output lease is not writable from the CPU (it must be the only lease)")]
    NotWritable,
    #[error("input is {found:?}, not the stage's {expected:?}")]
    InputFormat { expected: FourCc, found: FourCc },
    #[error(
        "output {0} is not a picture this ISP writes (RGB3, NV12, I420, YU12 or GREY at full or half size)"
    )]
    OutputFormat(String),
    #[error("output planes are not in ascending, non-overlapping order")]
    PlaneOrder,
    #[error(transparent)]
    Isp(#[from] IspError),
    #[error(transparent)]
    Gpu(#[from] GpuError),
    #[error("a raw frame on a dma-buf must be one plane of Bayer data, linear (got {0} planes)")]
    DmabufLayout(usize),
}

/// The software ISP for one raw geometry. Build it once; [`Self::apply`] new settings as the
/// 3A loop produces them.
#[derive(Debug)]
pub struct GpuIspStage {
    code: FourCc,
    format: RawFormat,
    options: Options,
    isp: GpuIsp,
    /// Raw dma-bufs imported so far, by (pool buffer index, descriptor): imported once each.
    imports: Vec<((usize, i32), ImportId)>,
}

impl GpuIspStage {
    /// The stage for raw frames of FourCc `code` and `width` x `height`, with `settings`
    /// applied. Errors when the FourCc is not a Bayer format this ISP reads.
    pub fn new(
        code: FourCc,
        width: u32,
        height: u32,
        settings: &IspSettings,
        options: Options,
    ) -> Result<Self, StageError> {
        Self::with_context(None, code, width, height, settings, options)
    }

    /// [`Self::new`] on `context`'s device (`None`: the default device).
    pub fn with_context(
        context: Option<&GpuContext>,
        code: FourCc,
        width: u32,
        height: u32,
        settings: &IspSettings,
        options: Options,
    ) -> Result<Self, StageError> {
        let format = RawFormat::from_fourcc(code, width, height)
            .ok_or_else(|| StageError::OutputFormat(format!("input {code:?}")))?;
        let params = params_for(&format, settings, &options);
        let isp = match context {
            Some(c) => GpuIsp::with_context(c, format, params)?,
            None => GpuIsp::new(format, params)?,
        };
        Ok(Self {
            code,
            format,
            options,
            isp,
            imports: Vec::new(),
        })
    }

    /// Sets the GPU lease the stage asks before each frame's GPU work (`None` clears it).
    pub fn set_gpu_lease(&mut self, lease: Option<Arc<dyn GpuLease>>) {
        self.isp.set_gpu_lease(lease);
    }

    /// The raw frame's source: a dma-buf imported once (zero-copy) or `None` for bytes the CPU
    /// reads. A dma-buf frame must be one linear Bayer plane.
    fn dmabuf_source<M>(
        &mut self,
        input: &FrameLease<M>,
    ) -> Result<Option<(ImportId, usize)>, StageError> {
        if input.memory_kind() != MemoryKind::Dmabuf {
            return Ok(None);
        }
        let memory = input.memory();
        let Some(fd) = memory.fd() else {
            return Ok(None);
        };
        let layouts = input.layouts();
        if layouts.len() != 1 {
            return Err(StageError::DmabufLayout(layouts.len()));
        }
        let base = memory.fd_offset();
        let key = (input.buffer_index(), fd.as_raw_fd());
        let id = match self.imports.iter().find(|(k, _)| *k == key) {
            Some((_, id)) => *id,
            None => {
                let id = self.isp.import_dmabuf(fd, base + memory.len())?;
                self.imports.push((key, id));
                id
            }
        };
        Ok(Some((id, base + layouts[0].offset)))
    }

    /// The raw format this stage processes.
    pub fn format(&self) -> RawFormat {
        self.format
    }

    /// Applies `settings` to the following frames. Tables are rebuilt only for what changed.
    pub fn apply(&mut self, settings: &IspSettings) -> Result<(), StageError> {
        let mut params = params_for(&self.format, settings, &self.options);
        self.isp.swap_params(&mut params)?;
        Ok(())
    }

    /// Processes `input` into `output` (a picture the caller acquired, sized as the stage's
    /// output for its format: full or half size, by its FourCc). With `stats`, the frame's
    /// statistics are written into it; without, none are gathered.
    pub fn process<M, N>(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<N>,
        stats: Option<&mut Statistics>,
    ) -> Result<(), StageError> {
        let found = input.format().code;
        let res = input.format().resolution;
        if found != self.code
            || res.width.get() != self.format.width
            || res.height.get() != self.format.height
        {
            return Err(StageError::InputFormat {
                expected: self.code,
                found,
            });
        }
        let dma = self.dmabuf_source(input)?;
        let read = match dma {
            Some(_) => None,
            None => Some(input.read().ok_or(StageError::NotReadable)?),
        };

        let code = output.format().code;
        let out_res = output.format().resolution;
        let scale = if out_res.width.get() == self.format.width {
            Scale::Full
        } else if out_res.width.get() == self.format.width / 2 {
            Scale::Half
        } else {
            return Err(StageError::OutputFormat(format!("{out_res:?}")));
        };
        let layouts = *output.layouts();
        let mut write = output.write().ok_or(StageError::NotWritable)?;
        let mut planes = split_planes(write.bytes(), &layouts)?;
        let buffers = match (code, planes.1) {
            (FourCc::RGB3, 1) => {
                let (data, stride) = take_plane(&mut planes.0, 0);
                OutputBuffers::Rgb24 { data, stride }
            }
            (FourCc::GREY, 1) => {
                let (data, stride) = take_plane(&mut planes.0, 0);
                OutputBuffers::Luma { data, stride }
            }
            (FourCc::NV12, 2) => {
                let (y, y_stride) = take_plane(&mut planes.0, 0);
                let (uv, uv_stride) = take_plane(&mut planes.0, 1);
                OutputBuffers::Nv12 {
                    y,
                    y_stride,
                    uv,
                    uv_stride,
                }
            }
            (FourCc::I420 | FourCc::YU12, 3) => {
                let (y, y_stride) = take_plane(&mut planes.0, 0);
                let (u, u_stride) = take_plane(&mut planes.0, 1);
                let (v, v_stride) = take_plane(&mut planes.0, 2);
                OutputBuffers::I420 {
                    y,
                    y_stride,
                    u,
                    u_stride,
                    v,
                    v_stride,
                }
            }
            _ => return Err(StageError::OutputFormat(format!("{code:?}"))),
        };

        let (source, stride) = match (&read, dma) {
            (Some(read), _) => {
                let plane = read.plane(0).ok_or(StageError::NotReadable)?;
                (Input::Bytes(plane.data), plane.stride)
            }
            (None, Some((id, offset))) => (Input::DmaBuf { id, offset }, input.layouts()[0].stride),
            (None, None) => return Err(StageError::NotReadable),
        };
        self.isp.set_statistics(stats.is_some());
        let got: Option<IspStats> = self.isp.process_input(source, stride, scale, buffers)?;
        if let (Some(stats), Some(got)) = (stats, got) {
            write_statistics(&got, stats);
            self.isp.recycle_stats(got);
        }
        Ok(())
    }
}

/// The most planes a picture has here (I420 and NV12: up to three).
const PICTURE_PLANES: usize = 3;

/// The planes of a picture, each as (data, stride), and how many of them there are.
type Planes<'a> = ([(&'a mut [u8], usize); PICTURE_PLANES], usize);

/// The output planes of `bytes`, each as (data, stride), in the order of `layouts`, and how many
/// there are. Planes must be ascending and non-overlapping, so the buffer splits without
/// aliasing. No allocation.
fn split_planes<'a>(
    bytes: &'a mut [u8],
    layouts: &[PlaneLayout],
) -> Result<Planes<'a>, StageError> {
    if layouts.len() > PICTURE_PLANES {
        return Err(StageError::PlaneOrder);
    }
    let mut rest = bytes;
    let mut consumed = 0usize;
    let mut out: [(&'a mut [u8], usize); PICTURE_PLANES] = Default::default();
    for (slot, layout) in out.iter_mut().zip(layouts) {
        if layout.offset < consumed {
            return Err(StageError::PlaneOrder);
        }
        let (_, tail) = std::mem::take(&mut rest).split_at_mut(layout.offset - consumed);
        let (plane, after) = tail.split_at_mut(layout.len);
        *slot = (plane, layout.stride);
        rest = after;
        consumed = layout.offset + layout.len;
    }
    Ok((out, layouts.len()))
}

/// Plane `index` of a picture's planes, taken out (leaving an empty slice behind).
fn take_plane<'a>(planes: &mut [(&'a mut [u8], usize)], index: usize) -> (&'a mut [u8], usize) {
    let (data, stride) = &mut planes[index];
    (std::mem::take(data), *stride)
}

/// Writes `got` into `out`, reusing its buffers. Colour sums are taken back to before the
/// white balance gains (the statistics' sums include them), so `before_wb` holds; lens shading
/// is applied before the statistics, so `before_lsc` does not.
fn write_statistics(got: &IspStats, out: &mut Statistics) {
    let gains = got.gains.map(|g| if g > 0.0 { g as f64 } else { 1.0 });
    let colour = &mut out.colour;
    colour.width = got.zones_x;
    colour.height = got.zones_y;
    colour.zones.clear();
    colour.zones.extend(got.zones.iter().map(|z| ColourZone {
        r: z.r_sum as f64 / gains[0],
        g: z.g_sum as f64 / gains[1],
        b: z.b_sum as f64 / gains[2],
        counted: z.count,
    }));
    out.luma = None;
    out.histogram
        .set_bins(got.histogram.iter().map(|&b| b as u64));
    if got.focus.is_empty() {
        out.focus = None;
    } else {
        let grid = out.focus.get_or_insert_with(|| ZoneGrid {
            width: 0,
            height: 0,
            zones: Vec::new(),
        });
        grid.width = got.zones_x;
        grid.height = got.zones_y;
        grid.zones.clear();
        grid.zones.extend_from_slice(&got.focus);
    }
    out.before_wb = true;
    out.before_lsc = false;
}
