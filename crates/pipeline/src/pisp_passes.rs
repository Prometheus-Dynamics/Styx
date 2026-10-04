//! Extra back end passes over a frame's raw input: the back end works memory to memory, so the
//! raw frame a job read can go through it again with another geometry (a region of interest
//! cropped at full resolution, scaled, in another format) while the front end still holds it.
//!
//! A pass's config is the frame's main config with the pass's geometry: its tiles, crop,
//! scalers and output format are prepared once (again when the pass moves or the main config
//! is re-prepared), and every frame copies the main config's blocks (black level, gains,
//! colour, tone, denoise, lens shading) over, so the pass sees the frame exactly as the main
//! pass did. Only the pass's own output is enabled: the tiles cover only the input its region
//! needs. The 3A statistics come from the front end and are not touched.
//!
//! Temporal denoise keeps a running average in two buffers the main pass reads and writes.
//! A pass must not write it (that would fold the frame in twice): it either reads the average
//! the main pass just wrote, without writing ([`PassTdn::Read`]: the region is denoised as the
//! main output is), or does without ([`PassTdn::Off`]).

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;

use styx_pisp::be::BackEnd;
use styx_pisp::uapi::{
    BE_MIN_TILE_HEIGHT, BE_MIN_TILE_WIDTH, BeConfig, BeCropConfig, BeTilesConfig,
    ImageFormatConfig, bayer_enable, image_format, rgb_enable,
};

use crate::error::{PipelineError, Result};
use crate::pisp_be::BeConfigBuilder;

/// One extra pass: a region of the input on one output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassSpec {
    /// The region of the input: even offsets and sizes, at least 16x16, inside the input.
    pub crop: BeCropConfig,
    /// The output (0 or 1) that writes it, into that output's buffers (which must hold it:
    /// its stride and height within the buffer's). Only output 1 has a downscaler (beyond 2x
    /// down output 0's resampler alone gets softer).
    pub output: usize,
    /// The output size: `None` for the region at full resolution.
    pub size: Option<(u16, u16)>,
    /// The output's PiSP image format (`image_format`/`formats`): `None` for the output's own.
    pub format: Option<u32>,
}

/// What extra passes do with temporal denoise (see the [module documentation](self)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PassTdn {
    /// Read the average the frame's main pass wrote, write nothing: the region is denoised
    /// as the main output is.
    #[default]
    Read,
    /// No temporal denoise in extra passes.
    Off,
}

#[derive(Debug)]
struct Pass {
    spec: PassSpec,
    cfg: Box<BeTilesConfig>,
    /// The main builder's [`BeConfigBuilder::generation`] the tiles were prepared at.
    generation: u64,
}

/// The extra passes of a stream of frames, by slot. See the [module documentation](self).
#[derive(Debug, Default)]
pub struct PassConfigs {
    passes: Vec<Option<Pass>>,
    tdn: PassTdn,
    /// The output buffers' formats (strides and heights a pass must fit in).
    buffers: [Option<ImageFormatConfig>; 2],
    /// Full prepares of passes (the pass moved, or the main config was re-prepared).
    prepares: u64,
}

impl PassConfigs {
    /// No passes yet, writing into output buffers of these formats.
    pub fn new(buffers: [Option<ImageFormatConfig>; 2], tdn: PassTdn) -> Self {
        Self {
            buffers,
            tdn,
            ..Default::default()
        }
    }

    /// What passes do with temporal denoise.
    pub fn set_tdn(&mut self, tdn: PassTdn) {
        self.tdn = tdn;
    }

    /// What passes do with temporal denoise.
    pub fn tdn(&self) -> PassTdn {
        self.tdn
    }

    /// Slots (set or not).
    pub fn len(&self) -> usize {
        self.passes.len()
    }

    /// No slot is set.
    pub fn is_empty(&self) -> bool {
        self.passes.iter().all(Option::is_none)
    }

    /// Slot `k`'s pass, if set.
    pub fn spec(&self, k: usize) -> Option<PassSpec> {
        self.passes.get(k)?.as_ref().map(|p| p.spec)
    }

    /// Prepare every pass again before its next frame (the main builder was replaced).
    pub fn invalidate(&mut self) {
        for p in self.passes.iter_mut().flatten() {
            p.generation = u64::MAX;
        }
    }

    /// How many times a pass's tiles were prepared.
    pub fn prepares(&self) -> u64 {
        self.prepares
    }

    /// Sets slot `k` to `spec` (`None`: no pass), prepared against `main`'s current config.
    /// Fails, changing nothing, when the back end cannot make it.
    pub fn set(&mut self, k: usize, spec: Option<PassSpec>, main: &BeConfigBuilder) -> Result<()> {
        let Some(spec) = spec else {
            if let Some(slot) = self.passes.get_mut(k) {
                *slot = None;
            }
            return Ok(());
        };
        if self.passes.get(k).and_then(Option::as_ref).map(|p| p.spec) == Some(spec) {
            return Ok(());
        }
        let cfg = self.prepare(&spec, main)?;
        if self.passes.len() <= k {
            self.passes.resize_with(k + 1, || None);
        }
        self.passes[k] = Some(Pass {
            spec,
            cfg,
            generation: main.generation(),
        });
        Ok(())
    }

    /// Slot `k`'s config for the frame `main`'s config is for: re-prepared when `main` was,
    /// else its blocks copied from it. `None` when the slot has no pass.
    pub fn config(&mut self, k: usize, main: &BeConfigBuilder) -> Result<Option<&BeTilesConfig>> {
        let Some(spec) = self.spec(k) else {
            return Ok(None);
        };
        let stale = self.passes[k]
            .as_ref()
            .is_some_and(|p| p.generation != main.generation());
        if stale {
            let cfg = self.prepare(&spec, main)?;
            let pass = self.passes[k].as_mut().expect("checked above");
            pass.cfg = cfg;
            pass.generation = main.generation();
        }
        let pass = self.passes[k].as_mut().expect("checked above");
        let tdn = tdn_for(self.tdn, main.config(), &pass.cfg);
        pass.cfg.config = pass_blocks(&main.config().config, &pass.cfg.config, tdn);
        Ok(Some(&*pass.cfg))
    }

    fn prepare(&mut self, spec: &PassSpec, main: &BeConfigBuilder) -> Result<Box<BeTilesConfig>> {
        let buffer = self
            .buffers
            .get(spec.output)
            .copied()
            .flatten()
            .ok_or_else(|| PipelineError::Config(format!("no output {}", spec.output)))?;
        let input = main.config().config.input_format;
        check_crop(spec.crop, (u32::from(input.width), u32::from(input.height)))?;
        // The driver places the planes by the buffer's format: a pass keeps its planarity
        // (or writes one plane of luma).
        let planarity = |f: u32| f & image_format::PLANARITY_MASK;
        if let Some(f) = spec.format
            && planarity(f) != planarity(buffer.format)
            && f & image_format::THREE_CHANNEL != 0
        {
            return Err(PipelineError::Config(format!(
                "extra pass: format {f:#x} does not fit output {}'s buffers (format {:#x})",
                spec.output, buffer.format
            )));
        }
        let mut be = pass_geometry(main.work(), spec, buffer);
        let mut cfg = be
            .prepare()
            .map_err(|e| PipelineError::Config(format!("extra pass: {}", e.0)))?;
        let out = cfg.config.output_format[spec.output].image;
        if out.stride > buffer.stride
            || out.stride2 > buffer.stride2.max(buffer.stride)
            || out.height > buffer.height
        {
            return Err(PipelineError::Config(format!(
                "extra pass: a {}x{} output (stride {}) does not fit output {}'s {}x{} buffers \
                 (stride {})",
                out.width,
                out.height,
                out.stride,
                spec.output,
                buffer.width,
                buffer.height,
                buffer.stride
            )));
        }
        let tdn = tdn_for(self.tdn, main.config(), &cfg);
        cfg.config = pass_blocks(&main.config().config, &cfg.config, tdn);
        self.prepares += 1;
        Ok(cfg)
    }
}

/// The input a config's tiles read: `(x0, y0, x1, y1)`, the bounding box of their windows.
pub fn tiles_cover(cfg: &BeTilesConfig) -> (u32, u32, u32, u32) {
    let n = (cfg.num_tiles as usize).min(cfg.tiles.len());
    cfg.tiles[..n]
        .iter()
        .fold((u32::MAX, u32::MAX, 0, 0), |(x0, y0, x1, y1), t| {
            let (x, y) = (u32::from(t.input_offset_x), u32::from(t.input_offset_y));
            (
                x0.min(x),
                y0.min(y),
                x1.max(x + u32::from(t.input_width)),
                y1.max(y + u32::from(t.input_height)),
            )
        })
}

/// [`PassTdn::Read`] only where the main pass's tiles cover all the pass reads: elsewhere the
/// average was not updated this frame (the main output is a crop elsewhere) and the pass goes
/// without.
fn tdn_for(tdn: PassTdn, main: &BeTilesConfig, pass: &BeTilesConfig) -> PassTdn {
    let (m, p) = (tiles_cover(main), tiles_cover(pass));
    let inside = p.0 >= m.0 && p.1 >= m.1 && p.2 <= m.2 && p.3 <= m.3;
    if inside { tdn } else { PassTdn::Off }
}

/// `main`'s blocks with `pass`'s geometry (its output enables, colour conversion, scalers and
/// output formats; the tiles are the pass's own), temporal denoise as `tdn` says.
fn pass_blocks(main: &BeConfig, pass: &BeConfig, tdn: PassTdn) -> BeConfig {
    let mut c = *main;
    let geometry = rgb_enable::OUTPUT0
        | rgb_enable::OUTPUT1
        | rgb_enable::CSC0
        | rgb_enable::CSC1
        | rgb_enable::DOWNSCALE0
        | rgb_enable::DOWNSCALE1
        | rgb_enable::RESAMPLE0
        | rgb_enable::RESAMPLE1;
    c.global.rgb_enables =
        (main.global.rgb_enables & !geometry) | (pass.global.rgb_enables & geometry);
    c.csc = pass.csc;
    c.downscale = pass.downscale;
    c.resample = pass.resample;
    c.output_format = pass.output_format;
    let all_tdn = bayer_enable::TDN
        | bayer_enable::TDN_INPUT
        | bayer_enable::TDN_OUTPUT
        | bayer_enable::TDN_COMPRESS
        | bayer_enable::TDN_DECOMPRESS;
    let main_tdn = main.global.bayer_enables & bayer_enable::TDN != 0;
    c.global.bayer_enables &= !all_tdn;
    if main_tdn && tdn == PassTdn::Read {
        // The average the main pass just wrote is of this frame, at its exposure.
        c.global.bayer_enables |= bayer_enable::TDN | bayer_enable::TDN_INPUT;
        c.tdn.reset = 0;
        c.tdn.ratio = 1 << 14;
    }
    c
}

/// `main` with `spec`'s geometry: only its output enabled, cropped, sized and in its format.
fn pass_geometry(main: &BackEnd, spec: &PassSpec, buffer: ImageFormatConfig) -> BackEnd {
    let mut be = main.clone();
    let (o, other) = (spec.output, 1 - spec.output);
    let g = be.config().global;
    let off = rgb_enable::output(other)
        | rgb_enable::csc(other)
        | rgb_enable::downscale(other)
        | rgb_enable::resample(other);
    let mut rgb = (g.rgb_enables & !off) | rgb_enable::output(o);
    let mut out = be.config().output_format[o];
    // The buffer's stride, so rows land where the buffer's layout puts them.
    out.image = ImageFormatConfig {
        width: 0,
        height: 0,
        format: spec.format.unwrap_or(buffer.format),
        stride: buffer.stride,
        stride2: buffer.stride2,
    };
    let f = out.image.format;
    let planarity = f & image_format::PLANARITY_MASK;
    if planarity == image_format::PLANARITY_INTERLEAVED {
        out.image.stride2 = 0;
    } else if out.image.stride2 == 0 {
        out.image.stride2 = out.image.stride;
    }
    // YCbCr (and luma alone) through the output's colour space conversion, RGB without.
    let yuv = f & (image_format::SAMPLING_MASK | image_format::PLANARITY_MASK) != 0
        || f & image_format::THREE_CHANNEL == 0;
    if yuv {
        let jpeg = styx_pisp::be::defaults::encoding("jpeg").expect("jpeg encoding");
        be.set_csc(o, jpeg.ycbcr);
        rgb |= rgb_enable::csc(o);
    } else {
        rgb &= !rgb_enable::csc(o);
    }
    be.set_output_format(o, out);
    be.set_crop(o, spec.crop);
    let (w, h) = spec.size.unwrap_or((spec.crop.width, spec.crop.height));
    be.set_smart_resize(o, w, h);
    be.set_global(g.bayer_enables, rgb, bayer_order(g.bayer_order));
    be
}

fn bayer_order(v: u8) -> styx_pisp::uapi::BayerOrder {
    use styx_pisp::uapi::BayerOrder;
    match v {
        1 => BayerOrder::Gbrg,
        2 => BayerOrder::Bggr,
        3 => BayerOrder::Grbg,
        128.. => BayerOrder::Greyscale,
        _ => BayerOrder::Rggb,
    }
}

/// Whether the back end can crop a `frame`-sized input to `c`: even offsets and sizes, at
/// least 16x16, inside the frame.
pub fn check_crop(c: BeCropConfig, frame: (u32, u32)) -> Result<()> {
    let (x, y, w, h) = (
        u32::from(c.offset_x),
        u32::from(c.offset_y),
        u32::from(c.width),
        u32::from(c.height),
    );
    let fail = |why: &str| {
        Err(PipelineError::Config(format!(
            "crop {w}x{h} at ({x}, {y}) of {}x{}: {why}",
            frame.0, frame.1
        )))
    };
    if w < BE_MIN_TILE_WIDTH || h < BE_MIN_TILE_HEIGHT {
        return fail("smaller than 16x16");
    }
    if (x | y | w | h) & 1 != 0 {
        return fail("offsets and sizes must be even");
    }
    if x + w > frame.0 || y + h > frame.1 {
        return fail("outside the frame");
    }
    Ok(())
}

#[cfg(test)]
#[path = "pisp_passes_tests.rs"]
mod tests;
