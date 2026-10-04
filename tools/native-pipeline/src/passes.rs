//! `pisp --passes`: extra back end passes over every frame (regions of interest), their cost
//! and how their pixels compare with the same region of the main output.
//!
//! ```text
//!   --passes W x H @ X , Y [> OW x OH] [/ OUT] [; ...]
//!                                    regions, e.g. 256x256@512,272;64x64@0,0 (output 0, NV12);
//!                                    `>640x400/1`: scaled to 640x400 on output 1
//!   --pass-probe                     measure the regions in the main output without passes
//!   --pass-tdn read|off              temporal denoise in the passes (default read)
//!   --pass-move PX                   move every region PX pixels right each frame (wrapping):
//!                                    a tracker's regions, re-prepared every frame
//! ```

use std::time::Duration;

use styx_pipeline::device::{PispFrame, PispPipeline};
use styx_pipeline::pisp_passes::{PassSpec, PassTdn};
use styx_pisp::uapi::BeCropConfig;

/// One `--passes` region: crop, output size, output.
pub type Region = ((u16, u16, u16, u16), Option<(u16, u16)>, usize);

/// The regions of `--passes`.
pub fn parse(v: &str) -> Result<Vec<Region>, String> {
    v.split(';')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let bad = || format!("--passes: {s} is not WxH@X,Y[>WxH][/OUTPUT]");
            let n = |v: &str| v.trim().parse::<u16>().map_err(|_| bad());
            let (s, output) = match s.split_once('/') {
                Some((s, o)) => (s, usize::from(n(o)?)),
                None => (s, 0),
            };
            let (s, size) = match s.split_once('>') {
                Some((s, size)) => {
                    let (w, h) = size.split_once('x').ok_or_else(bad)?;
                    (s, Some((n(w)?, n(h)?)))
                }
                None => (s, None),
            };
            let (crop, at) = s.split_once('@').ok_or_else(bad)?;
            let (w, h) = crop.split_once('x').ok_or_else(bad)?;
            let (x, y) = at.split_once(',').ok_or_else(bad)?;
            Ok(((n(x)?, n(y)?, n(w)?, n(h)?), size, output))
        })
        .collect()
}

/// `--pass-tdn`.
pub fn parse_tdn(v: &str) -> Result<PassTdn, String> {
    match v {
        "read" => Ok(PassTdn::Read),
        "off" => Ok(PassTdn::Off),
        v => Err(format!("--pass-tdn: read or off, not {v}")),
    }
}

/// Luma rows of a region: `(bytes, stride, x, y, w, h)`.
type Luma<'a> = (&'a [u8], usize, usize, usize, usize, usize);

#[derive(Default)]
pub struct PassRun {
    regions: Vec<Region>,
    /// Measure the regions in the main output only.
    probe: bool,
    step: u16,
    frame: (u32, u32),
    /// Per frame: the passes' total time.
    totals: Vec<Duration>,
    /// Per pass job.
    jobs: Vec<Duration>,
    skipped: u64,
    /// Mean |pass - main| over compared frames, per pass, and the largest difference.
    diff: Vec<(f64, u8, u64)>,
    /// Temporal noise (mean squared difference to the previous frame): main region and pass.
    noise: Vec<(f64, f64, u64)>,
    last: Vec<Option<(Vec<u8>, Vec<u8>)>>,
}

fn spec(((x, y, w, h), size, output): Region) -> PassSpec {
    PassSpec {
        crop: BeCropConfig {
            offset_x: x,
            offset_y: y,
            width: w,
            height: h,
        },
        output,
        size,
        format: None,
    }
}

impl PassRun {
    pub fn new(regions: Vec<Region>, probe: bool, step: u16, frame: (u32, u32)) -> Self {
        let n = regions.len();
        Self {
            regions,
            probe,
            step,
            frame,
            diff: vec![(0.0, 0, 0); n],
            noise: vec![(0.0, 0.0, 0); n],
            last: vec![None; n],
            ..Default::default()
        }
    }

    pub fn active(&self) -> bool {
        !self.regions.is_empty()
    }

    /// Sets the passes for frame `i` (moved by `--pass-move` every frame).
    pub fn before_frame(&mut self, p: &mut PispPipeline, i: u64) -> Result<(), String> {
        if self.probe {
            return Ok(());
        }
        for (k, r) in self.regions.iter().enumerate() {
            let mut r = *r;
            if self.step > 0 {
                let span = (self.frame.0 as u16).saturating_sub(r.0.2) & !1;
                let moved = (u64::from(r.0.0) + i * u64::from(self.step)) % u64::from(span.max(2));
                r.0.0 = (moved as u16) & !1;
            }
            p.set_pass(k, Some(spec(r))).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Times the frame's passes and, with `compare`, compares each with the main output's
    /// region (NV12 luma at `main`), then gives the pass buffers back.
    pub fn after_frame(
        &mut self,
        p: &mut PispPipeline,
        f: &PispFrame,
        main: Option<(usize, usize)>,
    ) -> Result<(), String> {
        self.totals.push(f.times.passes);
        let Some((stride, _)) = main else {
            for pass in &f.passes {
                match pass {
                    Some(pass) => self.jobs.push(pass.elapsed),
                    None => self.skipped += u64::from(!self.probe),
                }
            }
            p.release_passes(&f.passes);
            return Ok(());
        };
        p.sync_output(0, &f.job, true).map_err(|e| e.to_string())?;
        for (k, r) in self.regions.clone().into_iter().enumerate() {
            let pass = f.passes.get(k).copied().flatten();
            let ((x, y, w, h), size, output) = match pass {
                Some(pass) => {
                    let c = pass.spec.crop;
                    self.jobs.push(pass.elapsed);
                    (
                        (c.offset_x, c.offset_y, c.width, c.height),
                        pass.spec.size,
                        pass.output,
                    )
                }
                None if self.probe => r,
                None => {
                    self.skipped += 1;
                    continue;
                }
            };
            // Only full-resolution NV12 regions compare with the main output.
            if size.is_some() || output != 0 {
                continue;
            }
            let (x, y, w, h) = (
                usize::from(x),
                usize::from(y),
                usize::from(w),
                usize::from(h),
            );
            let Some(m) = p.output(0, &f.job) else {
                continue;
            };
            let main_region: Luma<'_> = (m, stride, x, y, w, h);
            match pass {
                Some(pass) => {
                    p.sync_pass(&pass, true).map_err(|e| e.to_string())?;
                    if let Some(o) = p.pass_output(&pass) {
                        self.compare(k, main_region, Some((o, stride, 0, 0, w, h)));
                    }
                    p.sync_pass(&pass, false).map_err(|e| e.to_string())?;
                }
                None => self.compare(k, main_region, None),
            }
        }
        p.sync_output(0, &f.job, false).map_err(|e| e.to_string())?;
        p.release_passes(&f.passes);
        Ok(())
    }

    fn compare(&mut self, k: usize, main: Luma<'_>, pass: Option<Luma<'_>>) {
        let rows = |(b, stride, x, y, w, h): Luma<'_>| -> Vec<u8> {
            (0..h)
                .flat_map(|r| b[(y + r) * stride + x..][..w].iter().copied())
                .collect()
        };
        let a = rows(main);
        let b = pass.map(rows).unwrap_or_default();
        let n = a.len().max(1) as f64;
        if !b.is_empty() {
            let sum: u64 = a
                .iter()
                .zip(&b)
                .map(|(&p, &q)| u64::from(p.abs_diff(q)))
                .sum();
            let max = a
                .iter()
                .zip(&b)
                .map(|(&p, &q)| p.abs_diff(q))
                .max()
                .unwrap_or(0);
            let d = &mut self.diff[k];
            d.0 += sum as f64 / n;
            d.1 = d.1.max(max);
            d.2 += 1;
        }
        if let Some((pa, pb)) = &self.last[k]
            && pa.len() == a.len()
        {
            let msd = |x: &[u8], y: &[u8]| {
                x.iter()
                    .zip(y)
                    .map(|(&p, &q)| f64::from(i16::from(p) - i16::from(q)).powi(2))
                    .sum::<f64>()
                    / n
            };
            let t = &mut self.noise[k];
            t.0 += msd(&a, pa);
            if b.len() == pb.len() && !b.is_empty() {
                t.1 += msd(&b, pb);
            }
            t.2 += 1;
        }
        // Temporal noise only means something while the region stays put.
        self.last[k] = (self.step == 0).then_some((a, b));
    }

    pub fn report(&self, p: &PispPipeline) -> Vec<String> {
        if !self.active() {
            return Vec::new();
        }
        let q = |v: &[Duration], at: f64| {
            let mut v = v.to_vec();
            v.sort();
            v.get(((v.len().max(1) - 1) as f64 * at).round() as usize)
                .copied()
                .unwrap_or_default()
                .as_secs_f64()
                * 1e3
        };
        let mut out = vec![format!(
            "passes{}: {} regions {:?}, moved {} px/frame; per frame median {:.3} ms, p95 {:.3} ms; \
                 per job median {:.3} ms, p95 {:.3} ms; {} skipped (no buffer); {} prepares",
            if self.probe { " (probe: none run)" } else { "" },
            self.regions.len(),
            self.regions,
            self.step,
            q(&self.totals, 0.5),
            q(&self.totals, 0.95),
            q(&self.jobs, 0.5),
            q(&self.jobs, 0.95),
            self.skipped,
            p.pass_prepares()
        )];
        for (k, (d, n)) in self.diff.iter().zip(&self.noise).enumerate() {
            let noise = if n.2 > 0 {
                format!(
                    "temporal noise (MSD to the previous frame) main {:.2}, pass {:.2}",
                    n.0 / n.2 as f64,
                    n.1 / n.2 as f64
                )
            } else {
                String::new()
            };
            let diff = if d.2 > 0 {
                format!(
                    "luma vs the main output's region: mean |diff| {:.3}, max {} over {} frames; ",
                    d.0 / d.2 as f64,
                    d.1,
                    d.2
                )
            } else {
                String::new()
            };
            out.push(format!("region {k}: {diff}{noise}"));
        }
        out
    }
}
