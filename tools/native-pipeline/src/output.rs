//! The software ISP's output for `soft` and `replay` (`--output`): its buffers, a cheap mean
//! level per frame and the final image.

use std::path::Path;

use styx_pipeline::measure::{grey_ratios, nv12_to_rgb, write_pgm, write_ppm};
use styx_softisp::{OutputBuffers, Scale};

/// What the ISP writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Rgb,
    Nv12,
    Luma,
}

/// `--output`: `rgb`, `nv12`, `luma`, each optionally `-half`.
pub fn parse(s: &str) -> Result<(Kind, Scale), String> {
    let (kind, scale) = match s.strip_suffix("-half") {
        Some(k) => (k, Scale::Half),
        None => (s, Scale::Full),
    };
    let kind = match kind {
        "rgb" => Kind::Rgb,
        "nv12" => Kind::Nv12,
        "luma" => Kind::Luma,
        _ => return Err(format!("--output {s}: rgb, nv12 or luma, optionally -half")),
    };
    Ok((kind, scale))
}

/// The output image of one size.
pub struct Output {
    pub kind: Kind,
    pub scale: Scale,
    pub width: usize,
    pub height: usize,
    data: Vec<u8>,
    uv: Vec<u8>,
}

impl Output {
    /// Buffers for `kind` at `scale` of a `w` x `h` raw frame.
    pub fn new(kind: Kind, scale: Scale, w: usize, h: usize) -> Self {
        let (width, height) = match scale {
            Scale::Full => (w, h),
            Scale::Half => (w / 2, h / 2),
        };
        let bytes = match kind {
            Kind::Rgb => width * height * 3,
            Kind::Nv12 | Kind::Luma => width * height,
        };
        let uv = if kind == Kind::Nv12 {
            width * height / 2
        } else {
            0
        };
        Self {
            kind,
            scale,
            width,
            height,
            data: vec![0; bytes],
            uv: vec![0; uv],
        }
    }

    pub fn buffers(&mut self) -> OutputBuffers<'_> {
        let w = self.width;
        match self.kind {
            Kind::Rgb => OutputBuffers::Rgb24 {
                data: &mut self.data,
                stride: w * 3,
            },
            Kind::Nv12 => OutputBuffers::Nv12 {
                y: &mut self.data,
                y_stride: w,
                uv: &mut self.uv,
                uv_stride: w,
            },
            Kind::Luma => OutputBuffers::Luma {
                data: &mut self.data,
                stride: w,
            },
        }
    }

    /// Mean output luma (0..255) over every fourth row and column: cheap enough per frame
    /// not to weigh on the CPU figures (a full pass over an RGB24 frame costs about 1 ms on
    /// the CM5).
    pub fn level(&self) -> f64 {
        let (w, h) = (self.width, self.height);
        let mut sum = 0.0f64;
        let mut n = 0usize;
        for y in (0..h).step_by(4) {
            for x in (0..w).step_by(4) {
                sum += match self.kind {
                    Kind::Rgb => {
                        let p = &self.data[(y * w + x) * 3..][..3];
                        0.299 * f64::from(p[0]) + 0.587 * f64::from(p[1]) + 0.114 * f64::from(p[2])
                    }
                    Kind::Nv12 | Kind::Luma => f64::from(self.data[y * w + x]),
                };
                n += 1;
            }
        }
        sum / n.max(1) as f64
    }

    /// The image as RGB24, if it has colour.
    pub fn rgb(&self) -> Option<Vec<u8>> {
        match self.kind {
            Kind::Rgb => Some(self.data.clone()),
            Kind::Nv12 => Some(nv12_to_rgb(
                &self.data,
                &self.uv,
                self.width,
                self.height,
                self.width,
            )),
            Kind::Luma => None,
        }
    }

    /// Writes `<stem>.ppm` (or `.pgm` for luma) into `dir`; returns the path and, with
    /// colour, the grey-world ratios R/G and B/G.
    pub fn save(&self, dir: &Path, stem: &str) -> std::io::Result<(String, Option<(f64, f64)>)> {
        let (w, h) = (self.width, self.height);
        match self.rgb() {
            Some(rgb) => {
                let path = dir.join(format!("{stem}.ppm"));
                write_ppm(&path, &rgb, w, h, w * 3)?;
                Ok((
                    path.display().to_string(),
                    Some(grey_ratios(&rgb, w, h, w * 3, 16, 240)),
                ))
            }
            None => {
                let path = dir.join(format!("{stem}.pgm"));
                write_pgm(&path, &self.data, w, h, w)?;
                Ok((path.display().to_string(), None))
            }
        }
    }
}

/// The summary's lines about the saved image.
pub fn summary_lines(saved: &str, ratios: Option<(f64, f64)>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some((rg, bg)) = ratios {
        v.push(format!(
            "output grey-world ratios R/G {rg:.3} B/G {bg:.3} (1.000 is neutral)"
        ));
    }
    v.push(format!("saved {saved}"));
    v
}
