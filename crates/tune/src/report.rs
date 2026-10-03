//! A readable account of a calibration.

use std::fmt::Write;

use crate::calib::Calibration;
use crate::colour::MACBETH_NAMES;

fn codes(v: f64, bits: u32) -> f64 {
    v * f64::from(1u32 << bits)
}

/// The report as text.
pub fn text(c: &Calibration) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "styx-tune calibration report\n");
    if let Some(b) = &c.black {
        let _ = writeln!(o, "Black level (10-bit codes R / Gr / Gb / B):");
        for g in &b.by_gain {
            let l = g.levels.map(|v| codes(v, 10));
            let _ = writeln!(
                o,
                "  gain {:>6.3}: {:7.2} {:7.2} {:7.2} {:7.2}  ({} frames, {}{})",
                g.gain,
                l[0],
                l[1],
                l[2],
                l[3],
                g.frames,
                if g.extrapolated {
                    "extrapolated to zero exposure"
                } else {
                    "covered"
                },
                if g.read_noise > 0.0 {
                    format!(", read noise {:.2} codes", codes(g.read_noise, 10))
                } else {
                    String::new()
                }
            );
        }
        if b.examined > 0 {
            let _ = writeln!(
                o,
                "  hot pixels: {} of {} samples examined{}",
                b.hot_pixels.len(),
                b.examined,
                if b.hot_pixels.is_empty() {
                    String::new()
                } else {
                    format!(" (first: {:?})", &b.hot_pixels[..b.hot_pixels.len().min(8)])
                }
            );
        }
    }
    if let Some(a) = &c.alsc {
        let lum_max = a.luminance.iter().copied().fold(0.0, f64::max);
        let _ = writeln!(
            o,
            "\nLens shading ({}x{} cells): luminance gain up to {:.3} in the corners; sigma Cr {:.5}, Cb {:.5}",
            a.grid.0, a.grid.1, lum_max, a.sigma_cr, a.sigma_cb
        );
        for t in &a.tables {
            let mx = |v: &[f64]| v.iter().copied().fold(0.0, f64::max);
            let _ = writeln!(
                o,
                "  {:>6.0} K ({} flat): Cr up to {:.3}, Cb up to {:.3}",
                t.ct,
                t.shots,
                mx(&t.cr),
                mx(&t.cb)
            );
        }
    }
    for ch in &c.charts {
        let _ = writeln!(
            o,
            "\nChart in {}: {} ({} patches usable); corners {:?}",
            ch.shot,
            if ch.manual {
                "corners given".to_string()
            } else {
                format!("found automatically ({} patches seen)", ch.found)
            },
            ch.usable,
            ch.corners.map(|p| [p[0].round(), p[1].round()])
        );
    }
    if let Some(a) = &c.awb {
        let _ = writeln!(o, "\nAWB curve (ct, R/G, B/G):");
        for p in &a.curve {
            let _ = writeln!(o, "  {:>6.0} K  {:.4}  {:.4}", p[0], p[1], p[2]);
        }
        let _ = writeln!(
            o,
            "  transverse +{:.5} / -{:.5}",
            a.transverse_pos, a.transverse_neg
        );
        if !a.dropped.is_empty() {
            let _ = writeln!(o, "  dropped (out of order): {:?}", a.dropped);
        }
    }
    for f in &c.ccms {
        let m = f.ccm;
        let _ = writeln!(
            o,
            "\nColour matrix at {:.0} K ({} patches): mean ΔE76 {:.2} (max {:.2}), mean ΔE2000 {:.2}; without a matrix {:.2}",
            f.ct, f.used, f.mean_de, f.max_de, f.mean_de2000, f.identity_de
        );
        for r in 0..3 {
            let _ = writeln!(
                o,
                "  [{:8.4} {:8.4} {:8.4}]",
                m[r * 3],
                m[r * 3 + 1],
                m[r * 3 + 2]
            );
        }
        let worst = (0..24)
            .max_by(|&a, &b| f.patch_de[a].total_cmp(&f.patch_de[b]))
            .unwrap_or(0);
        let _ = writeln!(
            o,
            "  worst patch: {} (ΔE76 {:.2})",
            MACBETH_NAMES[worst], f.patch_de[worst]
        );
    }
    if let Some((n, source)) = &c.noise {
        let _ = writeln!(
            o,
            "\nNoise (16-bit scale, gain 1, from {source}): constant {:.2}, slope {:.3}; {} of {} samples, RMS error {:.1}%",
            n.constant,
            n.slope,
            n.used,
            n.offered,
            n.rms_error * 100.0
        );
    }
    if let Some(l) = &c.lux {
        let _ = writeln!(
            o,
            "\nLux reference: {:.0} lux at {:.0} us x {:.2}, mean Y {:.4} ({:.0} on the 16-bit scale)",
            l.reference_lux,
            l.reference_exposure_us,
            l.reference_gain,
            l.reference_y,
            l.reference_y * 65536.0
        );
    }
    if let Some(g) = &c.geq {
        let _ = writeln!(
            o,
            "\nGreen equalisation: offset {:.0}, slope {:.5}",
            g.offset, g.slope
        );
    }
    if !c.notes.is_empty() {
        let _ = writeln!(o, "\nNotes:");
        for n in &c.notes {
            let _ = writeln!(o, "  - {n}");
        }
    }
    o
}
