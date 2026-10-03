//! Stills through the PiSP back end from memory, on a node group of their own: a raw frame
//! held out of the stream ([`crate::still::HeldRaw`], the front end's 16-bit output) goes
//! through a back end config of its own (full size, the denoise the caller asks for, no
//! temporal denoise) while the stream's back end group keeps processing the preview.

use std::time::Duration;

use styx_pisp::device::{BackEndDevice, BeOutput};
use styx_pisp::format::formats;
use styx_pisp::uapi::{BayerOrder, ImageFormatConfig};
use styx_softisp::{CfaPattern, RawPacking};

use crate::error::{PipelineError, Result};
use crate::isp::{IspSettings, be_template};
use crate::pisp_be::BeConfigBuilder;
use crate::still::{HeldRaw, StillPixels};

fn bayer(c: CfaPattern) -> BayerOrder {
    match c {
        CfaPattern::Rggb => BayerOrder::Rggb,
        CfaPattern::Bggr => BayerOrder::Bggr,
        CfaPattern::Grbg => BayerOrder::Grbg,
        CfaPattern::Gbrg => BayerOrder::Gbrg,
    }
}

/// One back end node group set up for stills of one size and output format; reopen for
/// another.
pub struct StillBackEnd {
    dev: BackEndDevice,
    group: usize,
    key: (u32, u32, usize, CfaPattern, StillPixels),
}

impl std::fmt::Debug for StillBackEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StillBackEnd")
            .field("group", &self.group)
            .finish_non_exhaustive()
    }
}

fn key(raw: &HeldRaw, pixels: StillPixels) -> (u32, u32, usize, CfaPattern, StillPixels) {
    (raw.width, raw.height, raw.stride, raw.cfa, pixels)
}

fn input(raw: &HeldRaw) -> ImageFormatConfig {
    ImageFormatConfig {
        width: raw.width as u16,
        height: raw.height as u16,
        format: formats::BAYER16,
        stride: raw.stride as i32,
        stride2: 0,
    }
}

impl StillBackEnd {
    /// Opens back end node group `group` (the stream uses group 0; group 1 is free on the
    /// Pi 5 / CM5) for stills like `raw` in `pixels`.
    pub fn open(group: usize, raw: &HeldRaw, pixels: StillPixels) -> Result<Self> {
        if raw.packing != (RawPacking::U16Le { bits: 16 }) {
            return Err(PipelineError::Config(
                "the back end takes the front end's 16-bit raw frames".into(),
            ));
        }
        let out = match pixels {
            StillPixels::Rgb24 => BeOutput::Rgb24,
            StillPixels::Nv12 => BeOutput::Nv12,
        };
        let dev = BackEndDevice::open(group, input(raw), bayer(raw.cfa), out)?;
        Ok(Self {
            dev,
            group,
            key: key(raw, pixels),
        })
    }

    /// Whether this set-up fits stills like `raw` in `pixels`.
    pub fn fits(&self, raw: &HeldRaw, pixels: StillPixels) -> bool {
        self.key == key(raw, pixels)
    }

    /// Processes `raw` with `settings` (e.g. its own, with stronger spatial denoise); returns
    /// the image tightly packed and how long the back end job took.
    pub fn process(
        &mut self,
        raw: &HeldRaw,
        settings: &IspSettings,
        timeout: Duration,
    ) -> Result<(Vec<u8>, Duration)> {
        let pixels = self.key.4;
        if !self.fits(raw, pixels) {
            return Err(PipelineError::Config(
                "still back end set up for another size".into(),
            ));
        }
        let out = self.dev.output_format();
        let template = be_template(
            input(raw),
            bayer(raw.cfa),
            settings.black_level,
            [Some(out), None],
        )?;
        let mut builder = BeConfigBuilder::new(template)?;
        builder.update(settings)?;
        let (bytes, took) = self.dev.process(&raw.data, builder.config(), timeout)?;
        let (w, h, s) = (
            usize::from(out.width),
            usize::from(out.height),
            out.stride.max(0) as usize,
        );
        let (row, rows) = match pixels {
            StillPixels::Rgb24 => (w * 3, h),
            StillPixels::Nv12 => (w, h + h / 2),
        };
        let mut packed = Vec::with_capacity(row * rows);
        for r in bytes.chunks(s.max(1)).take(rows) {
            packed.extend_from_slice(r.get(..row).unwrap_or(r));
        }
        if packed.len() != row * rows {
            return Err(PipelineError::Device(format!(
                "still back end gave {} of {} bytes",
                packed.len(),
                row * rows
            )));
        }
        Ok((packed, took))
    }

    /// Stops the node group.
    pub fn close(self) -> Result<()> {
        Ok(self.dev.stop()?)
    }
}
