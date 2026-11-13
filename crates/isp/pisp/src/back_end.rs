//! The back end configuration for a stream of frames: [`IspSettings`] mapped into the Lemnos
//! library's [`BackEnd`] and prepared into the tiles config its jobs carry.
//!
//! The geometry (input, the two outputs, the temporal denoise buffers) is fixed when the builder
//! is made. Each frame's settings are applied to a copy of that template, which is cheap (the
//! template holds no heap), and prepared into the builder's own config buffer, so a frame after
//! the first allocates nothing here.

use styx_aaa::{Pwl, SharpenParams};
use styx_format::{ColorRange, ColorSpace, MediaFormat};
use styx_frame::Layouts;
use styx_settings::IspSettings;

use crate::PispError;
use crate::format::{format_or_refuse, is_yuv};
use styx_lemnos::pisp::be::defaults::{self, Encoding, ResampleFilter};
use styx_lemnos::pisp::be::lsc::{pack_lut, resample_table};
use styx_lemnos::pisp::be::{BackEnd, BeCacExtra, SplitJobs, split_tiles};
use styx_lemnos::pisp::format::compute_stride_align;
use styx_lemnos::pisp::uapi::{
    BayerOrder, BeCdnConfig, BeCropConfig, BeDpcConfig, BeGeqConfig, BeLscExtra,
    BeOutputFormatConfig, BeResampleExtra, BeSdnConfig, BeSharpenConfig, BeTdnConfig,
    BeTilesConfig, Branch, ImageFormatConfig, rgb_enable,
};

/// One back-end output: its picture (the size, the FourCc and the colour space and range) and the
/// stride it is laid out with, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output {
    pub media: MediaFormat,
    /// Bytes each row is aligned to (the stride is at least the packed row).
    pub stride_align: u32,
}

impl Output {
    /// The library's image configuration for this output, with its strides.
    pub fn image(&self) -> Result<ImageFormatConfig, PispError> {
        let r = self.media.resolution;
        let width = u16::try_from(r.width.get())
            .map_err(|_| PispError::Config(format!("output width {} is too large", r.width)))?;
        let height = u16::try_from(r.height.get())
            .map_err(|_| PispError::Config(format!("output height {} is too large", r.height)))?;
        let mut image = ImageFormatConfig {
            width,
            height,
            format: format_or_refuse(self.media.code)?,
            stride: 0,
            stride2: 0,
        };
        compute_stride_align(&mut image, self.stride_align);
        Ok(image)
    }

    /// The colour encoding of this output's YCbCr conversion: `None` for an output with no
    /// chroma (RGB and grey). A YUV output takes the encoding its colour space and range name;
    /// a combination the library has no conversion for is refused, never guessed.
    pub fn encoding(&self) -> Result<Option<Encoding>, PispError> {
        if !is_yuv(self.media.code) {
            return Ok(None);
        }
        let encoding = match (self.media.color, self.media.range) {
            (ColorSpace::Bt601, ColorRange::Full) => Encoding::Jpeg,
            (ColorSpace::Bt601, ColorRange::Limited) => Encoding::Smpte170m,
            (ColorSpace::Bt709, ColorRange::Limited) => Encoding::Rec709,
            (ColorSpace::Bt709, ColorRange::Full) => Encoding::Rec709Full,
            (ColorSpace::Bt2020, ColorRange::Limited) => Encoding::Bt2020,
            (ColorSpace::Bt2020, ColorRange::Full) => Encoding::Bt2020Full,
            _ => {
                return Err(PispError::Unsupported(
                    "YUV output colour space and range the library has no conversion for",
                ));
            }
        };
        Ok(Some(encoding))
    }
}

/// The frame geometry the back end is set up for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// The raw input the front end gives (16-bit Bayer, the stride the front end writes).
    pub input: ImageFormatConfig,
    pub bayer: BayerOrder,
    /// Output 0 (full size) and output 1 (the 2x2 box at half size, or any size the resampler
    /// makes).
    pub outputs: [Output; 2],
    /// The temporal denoise buffers' format (the input's), when the buffers exist.
    pub tdn: Option<ImageFormatConfig>,
    /// The input row a frame is split at into two back-end jobs (see [`BackEndBuilder::split`]),
    /// `None` for one job per frame. The config is then prepared in horizontal stripes so the
    /// split can be made.
    pub slice_at: Option<u16>,
}

/// Stripes are cut this many rows above the split line, so the first stripe's input ends by it.
const STRIPE_MARGIN: u16 = 32;

/// Temporal denoise restarts its average above this exposure ratio between frames (the
/// ratio register holds up to 4).
const TDN_MAX_RATIO: f64 = 4.0;

/// See the [module documentation](self).
#[derive(Debug)]
pub struct BackEndBuilder {
    template: BackEnd,
    config: Box<BeTilesConfig>,
    /// The two outputs' images as the configuration has them (sizes, FourCc format, strides).
    images: [ImageFormatConfig; 2],
    /// The geometry's outputs, which the settings' `outputs` must agree with.
    outputs: [MediaFormat; 2],
    tdn_on: bool,
    /// Exposure of the last frame that read temporal denoise (`None`: the next starts over).
    tdn_last: Option<f64>,
}

impl BackEndBuilder {
    /// The builder for `geometry`. Fails if the library refuses the configuration, or an output
    /// is a picture the library cannot make or a YUV colour it cannot convert (before any frame
    /// is sent).
    pub fn new(geometry: &Geometry) -> Result<Self, PispError> {
        let [o0, o1] = geometry.outputs;
        let images = [o0.image()?, o1.image()?];
        let encodings = [o0.encoding()?, o1.encoding()?];
        let input = geometry.input;
        let mut be = BackEnd::simple_bayer(
            input,
            geometry.bayer,
            0,
            (1.0, 1.0, 1.0),
            None,
            images[0].format,
            encodings[0].map(Encoding::forward),
        );
        for (i, image) in images.iter().enumerate() {
            let branch = branch(i);
            be.set_output_format(
                branch,
                BeOutputFormatConfig {
                    image: *image,
                    ..Default::default()
                },
            );
            if (image.width, image.height) != (input.width, input.height) {
                be.set_smart_resize(branch, image.width, image.height);
            }
            let rgb = be.config().global.rgb_enables | rgb_enable::output(i);
            be.config_mut().global.rgb_enables = rgb;
            be.set_crop(branch, BeCropConfig::default());
        }
        if let Some(e) = encodings[1] {
            be.set_csc_encoding(Branch::One, e);
        }
        set_blocks_with_defaults(&mut be, geometry.bayer)?;
        if (
            u32::from(images[1].width) * 2,
            u32::from(images[1].height) * 2,
        ) == (u32::from(input.width), u32::from(input.height))
        {
            set_box_half(&mut be);
        }
        if let Some(f) = geometry.tdn {
            be.set_tdn_format(f);
        }
        if let Some(line) = geometry.slice_at {
            if line == 0 || u32::from(line) >= u32::from(input.height) {
                return Err(PispError::Config(format!(
                    "slice point row {line} is outside the {} rows of the frame",
                    input.height
                )));
            }
            be.set_max_stripe_height(line.saturating_sub(STRIPE_MARGIN));
        }
        let template = be;
        let mut config = Box::<BeTilesConfig>::default();
        template
            .clone()
            .prepare_into(&mut config)
            .map_err(prepare_error)?;
        Ok(Self {
            template,
            config,
            images,
            outputs: [o0.media, o1.media],
            tdn_on: geometry.tdn.is_some(),
            tdn_last: None,
        })
    }

    /// The configuration of output `i` as the back end writes it (size, format, strides).
    pub fn image(&self, i: usize) -> ImageFormatConfig {
        self.images[i]
    }

    /// Output `i`'s planes in its buffer, as the back end lays them out: plane 0 at offset 0,
    /// each later plane after the planes before it, with the chroma strides and line counts.
    pub fn layouts(&self, i: usize) -> Result<Layouts, PispError> {
        image_layouts(&self.images[i])
    }

    /// The config as of the last [`Self::update`] (the template's before the first).
    pub fn config(&self) -> &BeTilesConfig {
        &self.config
    }

    /// Lets temporal denoise start its average over with the next frame.
    pub fn reset_tdn(&mut self) {
        self.tdn_last = None;
    }

    /// Maps `settings` for a frame exposed for `exposure` (exposure time x analogue gain x
    /// flicker correction, in any consistent unit) onto the config. Temporal denoise scales its
    /// average by the exposure ratio to the previous frame that read it.
    ///
    /// `settings.outputs`, when given, must name the outputs this builder was made for (the
    /// colour space and range are part of the YCbCr conversion, so a change needs a new builder).
    pub fn update(&mut self, settings: &IspSettings, exposure: f64) -> Result<(), PispError> {
        for (i, given) in settings.outputs.iter().enumerate() {
            let Some(made) = self.outputs.get(i) else {
                return Err(PispError::Config(format!(
                    "settings name output {i}, which this builder does not make"
                )));
            };
            if given != made {
                return Err(PispError::Config(format!(
                    "settings' output {i} is {given:?}; the builder was made for {made:?}"
                )));
            }
        }
        let mut be = self.template.clone();
        apply(&mut be, settings)?;
        // Temporal denoise's state moves only for a frame that is made: a refusal below restores it.
        let before = self.tdn_last;
        let tdn = self.tdn_for(settings, exposure);
        match tdn {
            Some((ratio, read)) => {
                let t = settings.denoise.tdn.expect("temporal denoise settings");
                let cfg = BeTdnConfig {
                    ratio: field(ratio, 16, 14),
                    noise_constant: field(t.noise_constant, 16, 0),
                    noise_slope: field(t.noise_slope, 16, 8),
                    threshold: field(t.threshold, 16, 16),
                    reset: u8::from(!read),
                    ..Default::default()
                };
                be.set_tdn(Some(cfg), read);
            }
            None => be.set_tdn(None, false),
        }
        if let Err(e) = be.prepare_into(&mut self.config) {
            self.tdn_last = before;
            return Err(prepare_error(e));
        }
        Ok(())
    }

    /// The temporal denoise ratio and whether the frame reads the average, or `None` when the
    /// frame has no temporal denoise.
    fn tdn_for(&mut self, settings: &IspSettings, exposure: f64) -> Option<(f64, bool)> {
        let exposure =
            Some(exposure).filter(|e| *e > 0.0 && self.tdn_on && settings.denoise.tdn.is_some());
        let Some(e) = exposure else {
            self.tdn_last = None;
            return None;
        };
        let ratio = self.tdn_last.map(|last| e / last);
        self.tdn_last = Some(e);
        match ratio {
            Some(r) if r < TDN_MAX_RATIO => Some((r, true)),
            _ => Some((1.0, false)),
        }
    }

    /// The frame split at row `line`: the first job runs once the rows up to `line` are in, the
    /// rest at the frame's end. `None` when the library cannot split it there.
    pub fn split(&self, line: u32) -> Option<SplitJobs> {
        split_tiles(&self.config, line)
    }
}

/// The library's branch for output `i`.
fn branch(i: usize) -> Branch {
    if i == 0 { Branch::Zero } else { Branch::One }
}

fn prepare_error(e: styx_lemnos::pisp::be::PrepareError) -> PispError {
    PispError::Config(format!("{e}"))
}

/// Applies `s` to `be`: the blocks the algorithms move, in the order that makes the black level
/// the denoise blocks read.
fn apply(be: &mut BackEnd, s: &IspSettings) -> Result<(), PispError> {
    be.set_black_levels(s.black_cfa().map(level16));
    let d = &s.denoise;
    if d.dpc != 0 {
        return Err(PispError::Unsupported(
            "defective pixel correction level (denoise.dpc)",
        ));
    }
    be.set_dpc(None::<BeDpcConfig>);
    be.set_geq(d.geq.map(|g| BeGeqConfig {
        offset: field(g.offset, 16, 0),
        slope_sharper: field(g.slope, 10, 10),
        min: 0,
        max: 0xffff,
    }));
    be.set_sdn(d.sdn.map(|s| BeSdnConfig {
        leakage: field(1.0 - s.strength, 8, 8) as u8,
        noise_constant: field(s.noise_constant, 16, 0),
        noise_slope: field(s.noise_slope, 16, 8),
        noise_constant2: field(s.noise_constant2, 16, 0),
        noise_slope2: field(s.noise_slope2, 16, 8),
        ..Default::default()
    }));
    be.set_cdn(d.cdn.map(|c| BeCdnConfig {
        thresh: field(c.threshold, 16, 0),
        iir_strength: field(c.strength, 8, 8) as u8,
        g_adjust: 0,
    }));
    match &s.sharpen {
        None => be.set_sharpen(Some(defaults::SHARPEN_DEFAULT)),
        Some(p) => be.set_sharpen(Some(sharpen_scaled(p))),
    }
    .map_err(|e| PispError::Config(format!("sharpen: {e}")))?;
    if let Some(ls) = &s.lens_shading {
        let (w, h) = (ls.width as usize, ls.height as usize);
        let lsc =
            |t: &[f64]| resample_table(t, w, h).map_err(|e| PispError::Config(format!("{e}")));
        let table = [lsc(&ls.r)?, lsc(&ls.g)?, lsc(&ls.b)?];
        be.set_lsc(pack_lut(&table), BeLscExtra::default());
    }
    let g = s.channel_gains();
    be.set_wb_gains(g[0], g[1], g[2]);
    be.set_ccm(s.ccm);
    be.set_gamma_curve(&gamma_points(s.gamma.as_ref()));
    // The tone curve is always on: the library's template leaves its block enable off.
    let rgb = be.config().global.rgb_enables | rgb_enable::GAMMA;
    be.config_mut().global.rgb_enables = rgb;
    Ok(())
}

/// The back end's blocks that have no settings of their own, each at the library's own default
/// (`be::defaults`), enabled or left off as dev's configuration has them. Sharpening is on with
/// its default unless the settings scale it (see [`apply`]).
///
/// The YCbCr conversion around sharpening and false colour is libpisp's full-range BT.601
/// (`jpeg`), as dev has it. Debinning is off, both directions included (its filter coefficients
/// are the library's, as dev writes them); demosaic and false colour run with their defaults;
/// tone mapping, saturation control, chromatic aberration and HOG are off. Output 0's resampler
/// filter is the library's Lanczos 3 coefficients with the resampler off (smart resize writes
/// its own when it turns on); output 1 is the box filter ([`set_box_half`]).
fn set_blocks_with_defaults(be: &mut BackEnd, bayer: BayerOrder) -> Result<(), PispError> {
    be.set_ycbcr_encoding(Some(Encoding::Jpeg));
    be.set_debin(None)
        .map_err(|e| PispError::Config(format!("debin: {e}")))?;
    // The block is off, and so are its directions: dev's configuration has both enables at 0.
    let debin = &mut be.config_mut().debin;
    debin.h_enable = 0;
    debin.v_enable = 0;
    let demosaic = (bayer != BayerOrder::Greyscale).then_some(defaults::DEMOSAIC_DEFAULT);
    be.set_demosaic(demosaic);
    be.set_false_colour(Some(defaults::FALSE_COLOUR_DEFAULT));
    let mut combine = be.config().sh_fc_combine;
    combine.y_factor = defaults::SHFC_Y_FACTOR_DEFAULT;
    be.set_sh_fc_combine(combine);
    be.set_sharpen(Some(defaults::SHARPEN_DEFAULT))
        .map_err(|e| PispError::Config(format!("sharpen: {e}")))?;
    be.set_resample_default(Branch::Zero, ResampleFilter::Lanczos3);
    be.set_sat_control(None);
    be.set_tonemap(None)
        .map_err(|e| PispError::Config(format!("tone map: {e}")))?;
    be.set_cac(None, BeCacExtra::default());
    be.set_hog(None);
    Ok(())
}

/// The library's sharpening defaults scaled by the settings' three factors: the threshold factor
/// on each filter's threshold, the strength factor on both response strengths, the limit factor
/// on both limits. Styx's own mapping of the factors onto the library's fields; the factors are
/// 1 for the library's defaults, so the default block is unchanged.
pub fn sharpen_scaled(p: &SharpenParams) -> BeSharpenConfig {
    let mut s = defaults::SHARPEN_DEFAULT;
    let scale = |v: u16, k: f64| (f64::from(v) * k).round().clamp(0.0, f64::from(u16::MAX)) as u16;
    for t in &mut s.thresholds {
        t[0] = scale(t[0], p.threshold);
    }
    s.positive_strength = scale(s.positive_strength, p.strength);
    s.negative_strength = scale(s.negative_strength, p.strength);
    s.positive_limit = scale(s.positive_limit, p.limit);
    s.negative_limit = scale(s.negative_limit, p.limit);
    s
}

/// The planes of an image in its buffer, as the back end lays them out (see
/// [`BackEndBuilder::layouts`]): plane 0 at offset 0 with `stride` bytes a line, then each
/// later plane with `stride2` bytes a line and its own line count (halved for 4:2:0).
pub fn image_layouts(image: &ImageFormatConfig) -> Result<Layouts, PispError> {
    let planes = styx_lemnos::pisp::format::num_planes(image.format);
    let mut out = Vec::with_capacity(planes);
    let mut offset = 0usize;
    for plane in 0..planes {
        let stride = if plane == 0 {
            image.stride
        } else {
            image.stride2
        };
        let stride = usize::try_from(stride)
            .map_err(|_| PispError::Config("negative stride".to_string()))?;
        let len = styx_lemnos::pisp::format::plane_size(image, plane);
        out.push(styx_frame::PlaneLayout::new(offset, stride, len));
        offset += len;
    }
    Layouts::new(&out).ok_or_else(|| PispError::Config("too many planes".to_string()))
}

/// A normalised level on the 16-bit scale the PiSP works in.
pub fn level16(v: f64) -> u16 {
    (v * 65536.0).round().clamp(0.0, 65535.0) as u16
}

/// `v` as an unsigned fixed-point register field of `bits` bits with `frac` fractional bits,
/// rounded and clamped.
fn field(v: f64, bits: u32, frac: u32) -> u16 {
    let max = f64::from((1u32 << bits) - 1);
    (v * f64::from(1u32 << frac)).round().clamp(0.0, max) as u16
}

/// The tone curve as `(x, y)` points on 16-bit scales; the sRGB curve (65 points) when `None`.
pub fn gamma_points(curve: Option<&Pwl>) -> Vec<(u32, u32)> {
    let to16 = |v: f64| (v.clamp(0.0, 1.0) * 65535.0).round() as u32;
    match curve {
        Some(p) if p.points().len() >= 2 => p
            .points()
            .iter()
            .map(|&(x, y)| (to16(x), to16(y)))
            .collect(),
        _ => (0..=64)
            .map(|i| {
                let x = (f64::from(i) / 64.0).powi(2);
                let y = if x <= 0.003_130_8 {
                    12.92 * x
                } else {
                    1.055 * x.powf(1.0 / 2.4) - 0.055
                };
                (to16(x), to16(y))
            })
            .collect(),
    }
}

/// Output 1 as a 2x2 box average of output 1's input: the resampler at a step of exactly 2 with
/// two equal taps (`[0, 0, 512, 512, 0, 0]` per phase), as the library's `box2` and the CPU's
/// half-size filter.
fn set_box_half(be: &mut BackEnd) {
    let out = be.config().output_format[1].image;
    let (w, h) = (out.width, out.height);
    be.set_resample_step(Branch::One, None);
    be.set_smart_resize(Branch::One, 0, 0);
    let mut rgb =
        be.config().global.rgb_enables & !(rgb_enable::downscale(1) | rgb_enable::resample(1));
    be.set_resample(
        Branch::One,
        BeResampleExtra {
            scaled_width: w,
            scaled_height: h,
            ..Default::default()
        },
    );
    let step = 2u16 << 12;
    be.set_resample_step(Branch::One, Some((step, step)));
    be.set_resample_filter(Branch::One, &styx_lemnos::pisp::be::ops::BOX2);
    rgb |= rgb_enable::resample(1);
    be.config_mut().global.rgb_enables = rgb;
}
