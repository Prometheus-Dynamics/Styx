//! The ISP as a stage between frames: a raw [`FrameLease`] in, a caller-acquired picture
//! [`FrameLease`] out, statistics into a caller's [`styx_aaa::Statistics`].
//!
//! The raw input is read where it lies (no copy); the output is written into the lease the
//! caller acquired. Buffers are sized once per geometry; a frame after warm-up allocates nothing
//! when the caller reuses its statistics.

use alloc::string::String;
use alloc::vec::Vec;

use styx_aaa::{ColourZone, Statistics, ZoneGrid};
use styx_format::FourCc;
use styx_frame::{FrameLease, PlaneLayout};
use styx_settings::IspSettings;

use crate::format::RawFormat;
use crate::isp::{IspError, SoftIsp};
use crate::output::{OutputBuffers, Scale};
use crate::params::Paths;
use crate::settings::{Options, params_for};
use crate::stats::IspStats;

/// Why a frame could not go through the stage.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
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
}

/// The software ISP for one raw geometry. Build it once; [`Self::apply`] new settings as the
/// 3A loop produces them.
#[derive(Debug)]
pub struct SoftIspStage {
    code: FourCc,
    format: RawFormat,
    options: Options,
    paths: Paths,
    isp: SoftIsp,
}

impl SoftIspStage {
    /// The stage for raw frames of FourCc `code` and `width` x `height`, with `settings`
    /// applied. Errors when the FourCc is not a Bayer format this ISP reads.
    pub fn new(
        code: FourCc,
        width: u32,
        height: u32,
        settings: &IspSettings,
        options: Options,
    ) -> Result<Self, StageError> {
        let format = RawFormat::from_fourcc(code, width, height)
            .ok_or_else(|| StageError::OutputFormat(alloc::format!("input {code:?}")))?;
        let params = params_for(&format, settings, &options);
        Ok(Self {
            code,
            format,
            options,
            paths: Paths::default(),
            isp: SoftIsp::new(format, params)?,
        })
    }

    /// Takes the given row paths (see [`Paths`]) for this stage and the settings applied after.
    pub fn with_paths(mut self, paths: Paths) -> Result<Self, StageError> {
        self.paths = paths;
        let mut params = self.isp.params().clone();
        params.paths = paths;
        self.isp.set_params(params)?;
        Ok(self)
    }

    /// The raw format this stage processes.
    pub fn format(&self) -> RawFormat {
        self.format
    }

    /// Applies `settings` to the following frames. Tables are rebuilt only for what changed.
    pub fn apply(&mut self, settings: &IspSettings) -> Result<(), StageError> {
        let mut params = params_for(&self.format, settings, &self.options);
        params.paths = self.paths;
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
        let read = input.read().ok_or(StageError::NotReadable)?;
        let plane = read.plane(0).ok_or(StageError::NotReadable)?;

        let code = output.format().code;
        let out_res = output.format().resolution;
        let scale = if out_res.width.get() == self.format.width {
            Scale::Full
        } else if out_res.width.get() == self.format.width / 2 {
            Scale::Half
        } else {
            return Err(StageError::OutputFormat(alloc::format!("{out_res:?}")));
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
            _ => return Err(StageError::OutputFormat(alloc::format!("{code:?}"))),
        };

        self.isp.set_statistics(stats.is_some());
        let got: Option<IspStats> = self.isp.process(plane.data, plane.stride, scale, buffers)?;
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
        let (_, tail) = core::mem::take(&mut rest).split_at_mut(layout.offset - consumed);
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
    (core::mem::take(data), *stride)
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
