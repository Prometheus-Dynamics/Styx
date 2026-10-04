//! `soft --roi X,Y,W,H [--overview F] [--check-roi]`: the software ISP makes only a region of
//! each frame at full resolution (and, with `--overview`, the whole frame binned by `F`, whose
//! pass gathers the statistics; without it the statistics have a pass of their own), as the
//! native backend does for a region of interest. `--check-roi` processes every frame whole
//! again with the settings it was processed with (a second ISP fed the same settings, so its
//! lens shading tables are kept and rebuilt as the loop's are) and compares the region with
//! that frame's crop: the CPU figures of such a run include the check.

use styx_pipeline::soft::LSC_TOLERANCE;
use styx_pipeline::{IspSettings, SoftParts, SoftTarget};
use styx_softisp::{RawFormat, Scale, SoftIsp, Window};

use crate::Args;
use crate::output::{Kind, Output};

/// The region's and the overview's buffers.
pub struct Parts {
    pub window: Window,
    pub region: Output,
    pub overview: Option<(u32, Output)>,
}

impl Parts {
    /// For `--roi` / `--overview` on `w`x`h` frames; `None` without either.
    pub fn new(a: &Args, w: usize, h: usize) -> Option<Self> {
        if a.roi.is_none() && a.overview.is_none() {
            return None;
        }
        let window = a.roi.unwrap_or(Window::new(0, 0, w as u32, h as u32));
        let kind = a.output.0;
        let region = Output::new(
            kind,
            Scale::Full,
            window.width as usize,
            window.height as usize,
        );
        let overview = a.overview.map(|f| {
            let (ow, oh) = ((w / f as usize) & !1, (h / f as usize) & !1);
            (f, Output::new(kind, Scale::Full, ow, oh))
        });
        Some(Self {
            window,
            region,
            overview,
        })
    }

    pub fn target(&mut self) -> SoftTarget<'_> {
        SoftTarget::Parts(SoftParts {
            regions: vec![(self.window, self.region.buffers())],
            overview: self.overview.as_mut().map(|(f, o)| (*f, o.buffers())),
        })
    }

    /// Lines for the summary.
    pub fn summary(&self) -> String {
        let w = self.window;
        format!(
            "region {}x{} at ({}, {}){}",
            w.width,
            w.height,
            w.x,
            w.y,
            self.overview.as_ref().map_or(
                ", statistics on their own (no overview)".into(),
                |(f, o)| format!(
                    ", overview {}x{} (binned 1/{f}) with the statistics",
                    o.width, o.height
                )
            )
        )
    }
}

/// `--check-roi`: each frame whole with the same settings, its crop against the region.
pub struct Checker {
    isp: SoftIsp,
    applied: Option<IspSettings>,
    bits: u8,
    base: styx_softisp::IspParams,
    full: Output,
    pub frames: usize,
    /// Frames whose region differs from the crop, bytes that differ, the largest difference.
    pub differing_frames: usize,
    pub differing_bytes: usize,
    pub max_diff: u8,
    /// Frames whose statistics from the region's passes (the overview's, or their own)
    /// differ from the whole frame's.
    pub differing_stats: usize,
}

impl Checker {
    pub fn new(a: &Args, format: RawFormat, bits: u8) -> Result<Self, String> {
        let base = crate::soft_base(a);
        let mut isp = SoftIsp::new(format, base.clone()).map_err(|e| e.to_string())?;
        isp.set_lens_shading_tolerance(LSC_TOLERANCE);
        let (w, h) = (format.width as usize, format.height as usize);
        Ok(Self {
            isp,
            applied: None,
            bits,
            base,
            full: Output::new(a.output.0, Scale::Full, w, h),
            frames: 0,
            differing_frames: 0,
            differing_bytes: 0,
            max_diff: 0,
            differing_stats: 0,
        })
    }

    /// Processes `raw` whole with `applied` and compares `parts`' region with its crop.
    pub fn check(
        &mut self,
        raw: &[u8],
        stride: usize,
        applied: &IspSettings,
        parts: &Parts,
    ) -> Result<(), String> {
        if self.applied.as_ref() != Some(applied) {
            self.isp
                .set_params(applied.softisp(self.bits, &self.base))
                .map_err(|e| e.to_string())?;
            self.applied = Some(applied.clone());
        }
        let whole = self
            .isp
            .process(raw, stride, Scale::Full, self.full.buffers())
            .map_err(|e| e.to_string())?;
        let parts_stats = match &parts.overview {
            Some((f, o)) => {
                let mut o = Output::new(o.kind, Scale::Full, o.width, o.height);
                self.isp.process_binned(raw, stride, *f, o.buffers())
            }
            None => self.isp.statistics(raw, stride),
        }
        .map_err(|e| e.to_string())?;
        self.differing_stats += usize::from(whole != parts_stats);
        let (w, r) = (parts.window, &parts.region);
        let (fw, (x, y)) = (self.full.width, (w.x as usize, w.y as usize));
        let (rw, rh) = (w.width as usize, w.height as usize);
        let (full_y, full_uv) = self.full.planes();
        let (reg_y, reg_uv) = r.planes();
        let bpp = if r.kind == Kind::Rgb { 3 } else { 1 };
        let mut rows: Vec<(&[u8], &[u8])> = (0..rh)
            .map(|j| {
                (
                    &full_y[((y + j) * fw + x) * bpp..][..rw * bpp],
                    &reg_y[j * rw * bpp..][..rw * bpp],
                )
            })
            .collect();
        if r.kind == Kind::Nv12 {
            rows.extend((0..rh / 2).map(|j| {
                (
                    &full_uv[(y / 2 + j) * fw + x..][..rw],
                    &reg_uv[j * rw..][..rw],
                )
            }));
        }
        let mut differing = 0;
        for (a, b) in rows {
            for (&a, &b) in a.iter().zip(b) {
                if a != b {
                    differing += 1;
                    self.max_diff = self.max_diff.max(a.abs_diff(b));
                }
            }
        }
        self.frames += 1;
        self.differing_bytes += differing;
        self.differing_frames += usize::from(differing > 0);
        Ok(())
    }

    pub fn summary(&self) -> String {
        format!(
            "region against the crop of the whole frame processed again with the same settings: \
             {} frames, {} differ ({} bytes, largest difference {}); statistics of the region's \
             passes differ from the whole frame's on {}",
            self.frames,
            self.differing_frames,
            self.differing_bytes,
            self.max_diff,
            self.differing_stats
        )
    }
}
