//! `BackEnd::prepare`: output sizes, smart resize, block finalisation, tiling and the
//! hardware tile descriptors.
//!
//! Ported from libpisp `src/libpisp/backend/backend_prepare.cpp` (BSD-2-Clause, Copyright
//! (C) 2021 - 2023, Raspberry Pi Ltd).

use std::fmt;

use super::tiling::{self, Interval, Interval2, Length2, TilingConfig};
use super::{BackEnd, defaults};
use crate::format::{addr_offset, compute_stride_align};
use crate::uapi::image_format as f;
use crate::uapi::*;

const SCALE_PRECISION: u32 = 12;
const UNITY_SCALE: u32 = 1 << SCALE_PRECISION;
const UNITY_PHASE: u32 = 1 << 12;
const RESAMPLE_PRECISION: u32 = 10;
const NUM_PHASES: u32 = 16;
const NUM_TAPS: usize = 6;

/// Why a configuration cannot be prepared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareError(pub String);

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "back end config: {}", self.0)
    }
}

impl std::error::Error for PrepareError {}

impl From<tiling::TilingError> for PrepareError {
    fn from(e: tiling::TilingError) -> Self {
        Self(e.to_string())
    }
}

type Result<T> = std::result::Result<T, PrepareError>;

fn fail<T>(msg: impl Into<String>) -> Result<T> {
    Err(PrepareError(msg.into()))
}

fn pixel_alignment(format: u32, byte_alignment: u32) -> u32 {
    let mut a = byte_alignment;
    if format & f::BPS_MASK == f::BPS_16 {
        a = byte_alignment / 2;
    } else if format & f::BPS_MASK == f::BPS_10 {
        a = byte_alignment * 3 / 4;
    } else if format & f::BPP_32 != 0 {
        a = byte_alignment / 4;
    }
    let planarity = format & f::PLANARITY_MASK;
    let sampling = format & f::SAMPLING_MASK;
    if planarity == f::PLANARITY_PLANAR && sampling != f::SAMPLING_444 {
        a *= 2;
    } else if planarity == f::PLANARITY_INTERLEAVED
        && (sampling == f::SAMPLING_422 || sampling == f::SAMPLING_420)
    {
        a /= 2;
    }
    a
}

fn lcm(a: u32, b: u32) -> u32 {
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    a / x * b
}

impl BackEnd {
    fn output_size(&self, i: usize) -> (u16, u16) {
        let c = &self.cfg;
        if self.smart_resize[i].0 != 0 && self.smart_resize[i].1 != 0 {
            self.smart_resize[i]
        } else if c.global.rgb_enables & rgb_enable::resample(i) != 0 {
            let e = &self.resample_extra[i];
            (e.scaled_width, e.scaled_height)
        } else if c.global.rgb_enables & rgb_enable::downscale(i) != 0 {
            let e = &self.downscale_extra[i];
            (e.scaled_width, e.scaled_height)
        } else if self.crop[i].width != 0 {
            (self.crop[i].width, self.crop[i].height)
        } else {
            (c.input_format.width, c.input_format.height)
        }
    }

    fn crop_size(&self, i: usize) -> (u16, u16) {
        if self.crop[i].width != 0 {
            (self.crop[i].width, self.crop[i].height)
        } else {
            (self.cfg.input_format.width, self.cfg.input_format.height)
        }
    }

    fn update_smart_resize(&mut self) {
        for i in 0..BE_NUM_OUTPUTS {
            let (out_w, out_h) = self.smart_resize[i];
            if out_w == 0 || out_h == 0 {
                continue;
            }
            let (in_w, in_h) = self.crop_size(i);
            let (mut rs_in_w, mut rs_in_h) = (in_w, in_h);
            // Only output 1 has a downscaler on BCM2712.
            if i == 1
                && (u32::from(out_w) * 2 < u32::from(in_w)
                    || u32::from(out_h) * 2 < u32::from(in_h))
            {
                let pick = |o: u16, n: u16| {
                    if u32::from(o) * 2 < u32::from(n) {
                        (o * 2).clamp(n.div_ceil(8), n / 2)
                    } else {
                        n
                    }
                };
                let (dw, dh) = (pick(out_w, in_w), pick(out_h, in_h));
                self.downscale_extra[i] = BeDownscaleExtra {
                    scaled_width: dw,
                    scaled_height: dh,
                };
                self.cfg.global.rgb_enables |= rgb_enable::downscale(i);
                (rs_in_w, rs_in_h) = (dw, dh);
            } else {
                self.cfg.global.rgb_enables &= !rgb_enable::downscale(i);
            }
            if rs_in_w == out_w && rs_in_h == out_h {
                self.cfg.global.rgb_enables &= !rgb_enable::resample(i);
                continue;
            }
            let sx = f64::from(rs_in_w - 1) / f64::from(out_w - 1);
            let sy = f64::from(rs_in_h - 1) / f64::from(out_h - 1);
            let mut coef = [0i16; BE_RESAMPLE_FILTER_SIZE];
            if sx > 2.1 && sx < sy * 1.1 && sy < sx * 1.1 {
                // Use the polyphase filter as a trapezoidal downscaler.
                let sx = sx.min((NUM_TAPS - 1) as f64);
                for p in 0..NUM_PHASES as usize {
                    let base = (1 << RESAMPLE_PRECISION)
                        - ((p as i32) << RESAMPLE_PRECISION) / NUM_PHASES as i32;
                    coef[p * NUM_TAPS] = (f64::from(base) / sx) as i16;
                    let mut scale = sx - (1.0 - p as f64 / f64::from(NUM_PHASES));
                    let mut t = 1;
                    while (t as f64) < 1.0 + sx.ceil() && t < NUM_TAPS {
                        let s = scale.min(1.0);
                        coef[p * NUM_TAPS + t] =
                            (s * f64::from(1 << RESAMPLE_PRECISION) / sx) as i16;
                        scale -= s;
                        t += 1;
                    }
                }
            } else {
                coef = defaults::resample_filter_for(sx);
            }
            self.cfg.resample[i].coef = coef;
            self.resample_extra[i].scaled_width = out_w;
            self.resample_extra[i].scaled_height = out_h;
            self.cfg.global.rgb_enables |= rgb_enable::resample(i);
        }
    }

    fn finalise(&mut self) -> Result<()> {
        let g = self.cfg.global;
        let bayer_in = g.bayer_enables & bayer_enable::INPUT != 0;
        let rgb_in = g.rgb_enables & rgb_enable::INPUT != 0;
        if bayer_in == rgb_in {
            return fail("exactly one of the Bayer and RGB inputs must be enabled");
        }
        if !bayer_in && g.bayer_enables != 0 {
            return fail("Bayer input disabled but Bayer blocks enabled");
        }
        let input = self.cfg.input_format;
        if u32::from(input.width) < BE_MIN_TILE_WIDTH
            || u32::from(input.height) < BE_MIN_TILE_HEIGHT
        {
            return fail("input image too small");
        }
        if bayer_in {
            if input.width & 1 != 0 || input.height & 1 != 0 {
                return fail("Bayer image dimensions must be even");
            }
            if input.stride & 15 != 0 {
                return fail("input stride must be 16-byte aligned");
            }
            let compressed = f::is_compressed(input.format);
            let decompress = g.bayer_enables & bayer_enable::DECOMPRESS != 0;
            if compressed != decompress {
                return fail("input compression and DECOMPRESS disagree");
            }
            if decompress && input.format & f::BPS_MASK != f::BPS_8 {
                return fail("compressed input must be 8 bit");
            }
        } else if (input.stride & 15 != 0) || (input.stride2 & 15 != 0) {
            return fail("input strides must be 16-byte aligned");
        }
        for (bit, name) in [
            (bayer_enable::TDN, "TDN"),
            (bayer_enable::STITCH, "stitch"),
            (bayer_enable::CAC, "CAC"),
        ] {
            if g.bayer_enables & bit != 0 {
                return fail(format!("{name} is not supported by this builder yet"));
            }
        }
        if g.bayer_enables & bayer_enable::LSC != 0 {
            self.finalise_lsc()?;
        }
        let mut any_output = false;
        for i in 0..BE_NUM_OUTPUTS {
            if g.rgb_enables & rgb_enable::output(i) == 0 {
                continue;
            }
            any_output = true;
            let (w, h) = self.crop_size(i);
            let (mut w, mut h) = (u32::from(w), u32::from(h));
            if g.rgb_enables & rgb_enable::downscale(i) != 0 {
                if i != 1 {
                    return fail("only output 1 has a downscaler");
                }
                let e = self.downscale_extra[i];
                let (sw, sh) = (u32::from(e.scaled_width), u32::from(e.scaled_height));
                if sw == 0 || sh == 0 {
                    return fail("downscaler output size is zero");
                }
                let fh = (w << SCALE_PRECISION) / sw;
                let fv = (h << SCALE_PRECISION) / sh;
                let ok =
                    |s: u32| s == UNITY_SCALE || (2 * UNITY_SCALE..=8 * UNITY_SCALE).contains(&s);
                if !ok(fh) || !ok(fv) {
                    return fail("downscale factors must be 1x or 2x..8x");
                }
                self.cfg.downscale[i] = BeDownscaleConfig {
                    scale_factor_h: fh as u16,
                    scale_factor_v: fv as u16,
                    scale_recip_h: ((sw << SCALE_PRECISION) / w) as u16,
                    scale_recip_v: ((sh << SCALE_PRECISION) / h) as u16,
                };
                (w, h) = (sw, sh);
            }
            if g.rgb_enables & rgb_enable::resample(i) != 0 {
                let e = self.resample_extra[i];
                let (sw, sh) = (u32::from(e.scaled_width), u32::from(e.scaled_height));
                if sw < 2 || sh < 2 {
                    return fail("resampler output size too small");
                }
                let fh = ((w - 1) << SCALE_PRECISION) / (sw - 1);
                let fv = ((h - 1) << SCALE_PRECISION) / (sh - 1);
                let ok = |s: u32| (UNITY_SCALE / 16..16 * UNITY_SCALE).contains(&s);
                if !ok(fh) || !ok(fv) {
                    return fail("resample factors must be under 16x either way");
                }
                self.cfg.resample[i].scale_factor_h = fh as u16;
                self.cfg.resample[i].scale_factor_v = fv as u16;
            }
            let o = &mut self.cfg.output_format[i];
            if o.hi == 0 {
                o.hi = 65535;
            }
            if o.hi2 == 0 {
                o.hi2 = 65535;
            }
            let img = o.image;
            if u32::from(img.width) < BE_MIN_TILE_WIDTH
                || u32::from(img.height) < BE_MIN_TILE_HEIGHT
            {
                return fail(format!("output {i} too small"));
            }
            let s = img.format & f::SAMPLING_MASK;
            if s == f::SAMPLING_420 && img.height & 1 != 0 {
                return fail("4:2:0 output height must be even");
            }
            if (s == f::SAMPLING_420 || s == f::SAMPLING_422)
                && img.format & f::PLANARITY_MASK != f::PLANARITY_INTERLEAVED
                && img.width & 1 != 0
            {
                return fail("4:2:0/4:2:2 output width must be even");
            }
            if img.stride & 15 != 0 || img.stride2 & 15 != 0 {
                return fail("output strides must be 16-byte aligned");
            }
        }
        if !any_output {
            return fail("no output enabled");
        }
        Ok(())
    }

    fn tiling_config(&self) -> TilingConfig {
        let c = &self.cfg;
        let mut t = TilingConfig::default();
        let input_align = if c.global.rgb_enables & rgb_enable::INPUT != 0 {
            let fmt = c.input_format.format;
            let y = if fmt & f::SAMPLING_MASK == f::SAMPLING_420 {
                2
            } else {
                1
            };
            Length2::new(lcm(pixel_alignment(fmt, BE_INPUT_ALIGN), 2) as i32, y)
        } else {
            let mut a = pixel_alignment(c.input_format.format, BE_INPUT_ALIGN);
            if f::is_compressed(c.input_format.format) {
                a = lcm(a, BE_COMPRESSED_ALIGN);
            }
            Length2::new(a as i32, 2)
        };
        t.input_alignment = input_align;
        t.input_image_size = Length2::new(
            i32::from(c.input_format.width),
            i32::from(c.input_format.height),
        );
        for i in 0..BE_NUM_OUTPUTS {
            let cr = self.crop[i];
            t.crop[i] = if cr.width == 0 || cr.height == 0 {
                Interval2 {
                    x: Interval::new(0, t.input_image_size.dx),
                    y: Interval::new(0, t.input_image_size.dy),
                }
            } else {
                Interval2 {
                    x: Interval::new(i32::from(cr.offset_x), i32::from(cr.width)),
                    y: Interval::new(i32::from(cr.offset_y), i32::from(cr.height)),
                }
            };
            let o = &c.output_format[i];
            t.output_h_mirror[i] = o.transform & BE_TRANSFORM_HFLIP != 0;
            t.downscale_factor[i] = Length2::new(
                i32::from(c.downscale[i].scale_factor_h),
                i32::from(c.downscale[i].scale_factor_v),
            );
            t.resample_factor[i] = Length2::new(
                i32::from(c.resample[i].scale_factor_h),
                i32::from(c.resample[i].scale_factor_v),
            );
            let de = &self.downscale_extra[i];
            t.downscale_image_size[i] =
                Length2::new(i32::from(de.scaled_width), i32::from(de.scaled_height));
            if c.global.rgb_enables & rgb_enable::output(i) != 0 {
                t.output_image_size[i] =
                    Length2::new(i32::from(o.image.width), i32::from(o.image.height));
            }
            let y = if o.image.format & f::SAMPLING_MASK == f::SAMPLING_420 {
                2
            } else {
                1
            };
            t.output_max_alignment[i] = Length2::new(
                pixel_alignment(o.image.format, BE_OUTPUT_MAX_ALIGN) as i32,
                y,
            );
            t.output_min_alignment[i] = Length2::new(
                pixel_alignment(o.image.format, BE_OUTPUT_MIN_ALIGN) as i32,
                y,
            );
        }
        t.max_tile_size = Length2::new(
            i32::from(self.max_tile_width),
            i32::from(self.max_stripe_height),
        );
        t.min_tile_size = Length2::new(BE_MIN_TILE_WIDTH as i32, BE_MIN_TILE_HEIGHT as i32);
        t.resample_enables = c.global.rgb_enables / rgb_enable::RESAMPLE0;
        t.downscale_enables = c.global.rgb_enables / rgb_enable::DOWNSCALE0;
        t.compressed_input = false;
        t
    }

    /// libpisp `finalise_lsc`: grid steps from the input size when not given, and the grid
    /// must cover the image.
    fn finalise_lsc(&mut self) -> Result<()> {
        const P: u32 = BE_LSC_STEP_PRECISION;
        let full = (BE_LSC_GRID_SIZE as u32) << P;
        let (w, h) = (
            u32::from(self.cfg.input_format.width),
            u32::from(self.cfg.input_format.height),
        );
        let lsc = &mut self.cfg.lsc;
        if lsc.grid_step_x == 0 {
            lsc.grid_step_x = (full / w.max(1)) as u16;
        }
        if lsc.grid_step_y == 0 {
            lsc.grid_step_y = (full / h.max(1)) as u16;
        }
        let e = self.lsc_extra;
        if u32::from(lsc.grid_step_x) * (w + u32::from(e.offset_x) - 1) >= full
            || u32::from(lsc.grid_step_y) * (h + u32::from(e.offset_y) - 1) >= full
        {
            return fail("lens shading grid does not cover the image");
        }
        Ok(())
    }

    /// Finalises the configuration and computes the tiles: the buffer for the config node.
    pub fn prepare(&mut self) -> Result<Box<BeTilesConfig>> {
        for i in 0..BE_NUM_OUTPUTS {
            let enabled = self.cfg.global.rgb_enables & rgb_enable::output(i) != 0;
            let (w, h) = self.output_size(i);
            let img = &mut self.cfg.output_format[i].image;
            if enabled {
                img.width = w;
                img.height = h;
                let given = (img.stride, img.stride2);
                compute_stride_align(img, BE_OUTPUT_MIN_ALIGN);
                if given.0 != 0 {
                    if given.0 < img.stride || given.1 < img.stride2 {
                        return fail(format!("output {i} strides too small"));
                    }
                    (img.stride, img.stride2) = given;
                }
            } else {
                *img = ImageFormatConfig {
                    format: img.format,
                    ..Default::default()
                };
            }
        }
        self.update_smart_resize();
        self.finalise()?;
        let tc = self.tiling_config();
        let (regions, grid) = tiling::tile_pipeline(&tc, BE_NUM_TILES)?;
        let mut out: Box<BeTilesConfig> = bytemuck::allocation::zeroed_box();
        for (n, r) in regions.iter().enumerate() {
            out.tiles[n] = self.hw_tile(n, r, grid)?;
        }
        out.num_tiles = regions.len() as u32;
        self.check_tiles(&out.tiles[..regions.len()], &tc)?;
        out.config = self.cfg;
        Ok(out)
    }

    fn hw_tile(&self, n: usize, r: &tiling::TileRegions, grid: Length2) -> Result<Tile> {
        let c = &self.cfg;
        let (nx, ny) = (grid.dx as usize, grid.dy as usize);
        let mut t = Tile::default();
        if n < nx {
            t.edge |= TILE_TOP_EDGE;
        }
        if n >= nx * (ny - 1) {
            t.edge |= TILE_BOTTOM_EDGE;
        }
        if n.is_multiple_of(nx) {
            t.edge |= TILE_LEFT_EDGE;
        }
        if (n + 1).is_multiple_of(nx) {
            t.edge |= TILE_RIGHT_EDGE;
        }
        let inp = r.input.input;
        t.input_offset_x = inp.x.offset as u16;
        t.input_offset_y = inp.y.offset as u16;
        t.input_width = inp.x.length as u16;
        t.input_height = inp.y.length as u16;
        if r.input.output != r.input.input {
            return fail("tiling error in the Bayer pipe");
        }
        for j in 0..BE_NUM_OUTPUTS {
            if c.global.rgb_enables & rgb_enable::output(j) == 0 {
                continue;
            }
            let out = r.output[j].output;
            if out.x.length == 0 || out.y.length == 0 {
                t.crop_x_start[j] = t.input_width;
                t.crop_y_start[j] = t.input_height;
                continue;
            }
            let crop_region = &r.crop[j];
            let mut resample_size = crop_region.output;
            resample_size.x = resample_size.x.cropped(r.resample[j].crop.x);
            resample_size.y = resample_size.y.cropped(r.resample[j].crop.y);
            let dcrop = if c.global.rgb_enables & rgb_enable::downscale(j) != 0 {
                resample_size = r.downscale[j].output;
                (
                    r.downscale[j].crop.x + crop_region.crop.x,
                    r.downscale[j].crop.y + crop_region.crop.y,
                )
            } else if c.global.rgb_enables & rgb_enable::resample(j) != 0 {
                (
                    r.resample[j].crop.x + crop_region.crop.x,
                    r.resample[j].crop.y + crop_region.crop.y,
                )
            } else {
                (
                    r.output[j].crop.x + crop_region.crop.x,
                    r.output[j].crop.y + crop_region.crop.y,
                )
            };
            t.crop_x_start[j] = dcrop.0.start as u16;
            t.crop_x_end[j] = dcrop.0.end as u16;
            t.crop_y_start[j] = dcrop.1.start as u16;
            t.crop_y_end[j] = dcrop.1.end as u16;
            t.resample_in_width[j] = resample_size.x.length as u16;
            t.resample_in_height[j] = resample_size.y.length as u16;
            t.output_offset_x[j] = out.x.offset as u16;
            t.output_offset_y[j] = out.y.offset as u16;
            t.output_width[j] = out.x.length as u16;
            t.output_height[j] = out.y.length as u16;
            for p in 0..3 {
                let k = p * BE_NUM_OUTPUTS + j;
                if c.global.rgb_enables & rgb_enable::downscale(j) != 0 {
                    let mask = (1u32 << SCALE_PRECISION) - 1;
                    let fx = (resample_size.x.offset as u32
                        * u32::from(c.downscale[j].scale_factor_h))
                        & mask;
                    let fy = (resample_size.y.offset as u32
                        * u32::from(c.downscale[j].scale_factor_v))
                        & mask;
                    t.downscale_phase_x[k] = (UNITY_PHASE - fx) as u16;
                    t.downscale_phase_y[k] = (UNITY_PHASE - fy) as u16;
                }
                if c.global.rgb_enables & rgb_enable::resample(j) != 0 {
                    let rs = &c.resample[j];
                    let ix = (u32::from(t.output_offset_x[j])
                        * NUM_PHASES
                        * u32::from(rs.scale_factor_h))
                        >> SCALE_PRECISION;
                    let iy = (u32::from(t.output_offset_y[j])
                        * NUM_PHASES
                        * u32::from(rs.scale_factor_v))
                        >> SCALE_PRECISION;
                    let e = &self.resample_extra[j];
                    let px = (((ix % NUM_PHASES) << SCALE_PRECISION) / NUM_PHASES) as i32
                        + i32::from(e.initial_phase_h[p]);
                    let py = (((iy % NUM_PHASES) << SCALE_PRECISION) / NUM_PHASES) as i32
                        + i32::from(e.initial_phase_v[p]);
                    let max = 2 * UNITY_PHASE as i32 - 1;
                    if !(0..=max).contains(&px) || !(0..=max).contains(&py) {
                        return fail("resample phase out of range");
                    }
                    t.resample_phase_x[k] = px as u16;
                    t.resample_phase_y[k] = py as u16;
                }
            }
        }
        // Address offsets (libpisp `finaliseTiling`).
        let (x, y) = (u32::from(t.input_offset_x), u32::from(t.input_offset_y));
        if c.global.bayer_enables & bayer_enable::LSC != 0 {
            let e = self.lsc_extra;
            t.lsc_grid_offset_x = (x + u32::from(e.offset_x)) * u32::from(c.lsc.grid_step_x);
            t.lsc_grid_offset_y = (y + u32::from(e.offset_y)) * u32::from(c.lsc.grid_step_y);
        }
        (t.input_addr_offset, t.input_addr_offset2) = addr_offset(&c.input_format, x, y);
        t.tdn_input_addr_offset = addr_offset(&c.tdn_input_format, x, y).0;
        t.tdn_output_addr_offset = addr_offset(&c.tdn_output_format, x, y).0;
        t.stitch_input_addr_offset = addr_offset(&c.stitch_input_format, x, y).0;
        t.stitch_output_addr_offset = addr_offset(&c.stitch_output_format, x, y).0;
        for j in 0..BE_NUM_OUTPUTS {
            let o = &c.output_format[j];
            if o.transform & BE_TRANSFORM_HFLIP != 0 {
                t.output_offset_x[j] = o.image.width - t.output_offset_x[j] - t.output_width[j];
            }
            if o.transform & BE_TRANSFORM_VFLIP != 0 {
                t.output_offset_y[j] = o.image.height - t.output_offset_y[j] - 1;
            }
            (t.output_addr_offset[j], t.output_addr_offset2[j]) = addr_offset(
                &o.image,
                u32::from(t.output_offset_x[j]),
                u32::from(t.output_offset_y[j]),
            );
        }
        Ok(t)
    }

    fn check_tiles(&self, tiles: &[Tile], tc: &TilingConfig) -> Result<()> {
        let min_w = BE_MIN_TILE_WIDTH as u16;
        let min_h = BE_MIN_TILE_HEIGHT as u16;
        for (n, t) in tiles.iter().enumerate() {
            if t.input_width < min_w || t.input_height < min_h {
                return fail(format!("tile {n} too small at input"));
            }
            for i in 0..BE_NUM_OUTPUTS {
                if self.cfg.global.rgb_enables & rgb_enable::output(i) == 0 {
                    continue;
                }
                let w = t.input_width - t.crop_x_start[i] - t.crop_x_end[i];
                let h = t.input_height - t.crop_y_start[i] - t.crop_y_end[i];
                let has_out = t.output_width[i] != 0 && t.output_height[i] != 0;
                if (w != 0 && h != 0) != has_out {
                    return fail(format!("tile {n} output {i}: crop and output disagree"));
                }
                if !has_out {
                    continue;
                }
                let rh_edge = i32::from(t.output_offset_x[i]) + i32::from(t.output_width[i])
                    == tc.output_image_size[i].dx;
                if (w < min_w || t.resample_in_width[i] < min_w || t.output_width[i] < min_w)
                    && !rh_edge
                {
                    return fail(format!("tile {n} output {i} too narrow"));
                }
                if h < min_h || t.resample_in_height[i] < min_h || t.output_height[i] < min_h {
                    return fail(format!("tile {n} output {i} too short"));
                }
            }
        }
        Ok(())
    }
}
