//! A raw frame from a recording (`.mcap`, e.g. from `raw_processing record`) to a DNG that
//! raw converters open: black and white levels, the colour calibration and lens shading of a
//! tuning, and the exposure the recording kept for the frame.
//!
//! ```sh
//! cargo run -p styx-examples --features native,replay-mcap --bin mcap_to_dng -- IN.mcap OUT.dng [frame] [--tuning FILE] [--ct KELVIN]
//! ```
//!
//! `frame` is the index in the recording (default: the last). Without `--tuning` the built-in
//! tuning of the recorded sensor is used when Styx has one (OV9782), else an identity colour
//! matrix. The white balance is the tuning's AWB curve at `--ct`, or grey world from the frame.

use styx_dng::{CfaPattern, DngMetadata, Packing, RawImage, SampleLayout};
use styx_pipeline::still::{dng_calibrations, neutral_at, shot_calibration};
use styx_pipeline::styx_algo::{Tuning, algos::Alsc};

fn dng_pattern(p: styx_softisp::CfaPattern) -> CfaPattern {
    match p {
        styx_softisp::CfaPattern::Rggb => CfaPattern::Rggb,
        styx_softisp::CfaPattern::Bggr => CfaPattern::Bggr,
        styx_softisp::CfaPattern::Grbg => CfaPattern::Grbg,
        styx_softisp::CfaPattern::Gbrg => CfaPattern::Gbrg,
    }
}

fn dng_packing(p: styx_softisp::RawPacking) -> Packing {
    match p {
        styx_softisp::RawPacking::U8 => Packing::U8,
        styx_softisp::RawPacking::U16Le { bits } => Packing::U16Le { bits },
        styx_softisp::RawPacking::Csi2Raw10 => Packing::Csi2Raw10,
        styx_softisp::RawPacking::Csi2Raw12 => Packing::Csi2Raw12,
    }
}

/// Grey world: the mean of each colour over the frame, black-subtracted, as R/G, 1, B/G.
fn grey_world(img: &RawImage, pattern: CfaPattern, black: f64) -> [f64; 3] {
    let mut sum = [0f64; 3];
    let mut n = [0f64; 3];
    for (i, &v) in img.samples.iter().enumerate() {
        let (x, y) = (i % img.width as usize, i / img.width as usize);
        let c = usize::from(pattern.color_at(x, y));
        sum[c] += (f64::from(v) - black).max(0.0);
        n[c] += 1.0;
    }
    let m: Vec<f64> = (0..3).map(|c| sum[c] / n[c].max(1.0)).collect();
    [m[0] / m[1].max(1e-9), 1.0, m[2] / m[1].max(1e-9)]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let positional: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !a.starts_with("--") && (*i == 0 || !args[i - 1].starts_with("--")))
        .map(|(_, a)| a)
        .collect();
    let (Some(input), Some(output)) = (positional.first(), positional.get(1)) else {
        println!("usage: mcap_to_dng IN.mcap OUT.dng [frame] [--tuning FILE] [--ct KELVIN]");
        return Ok(());
    };
    let index: Option<usize> = positional.get(2).and_then(|s| s.parse().ok());
    let (header, frames) = styx::replay::open_recording(input.as_str())?;
    let mut chosen = None;
    for (i, f) in frames.enumerate() {
        chosen = Some(f?);
        if Some(i) == index {
            break;
        }
    }
    let frame = chosen.ok_or("the recording has no frames")?;
    let f = frame.meta().format;
    let (w, h) = (f.resolution.width.get(), f.resolution.height.get());
    let (pattern, packing) = styx_softisp::bayer_fourcc(f.code)
        .ok_or_else(|| format!("{} is not a Bayer format", f.code))?;
    let planes = frame.planes();
    let img = RawImage::from_packed(
        planes[0].data(),
        w,
        h,
        planes[0].stride(),
        dng_packing(packing),
        SampleLayout::Cfa(dng_pattern(pattern)),
    )?;
    let sensor = header
        .device
        .display
        .split_whitespace()
        .next()
        .unwrap_or("camera")
        .to_string();
    let tuning: Option<Tuning> = match opt("--tuning") {
        Some(path) => Some(Tuning::load(path)?),
        None => styx_pipeline::tuning::BUILTIN_TUNINGS
            .iter()
            .find(|(name, _)| name.trim_end_matches(".json") == sensor)
            .map(|(name, text)| styx_pipeline::tuning::parse_tuning(name, text))
            .transpose()?,
    };
    let full = f64::from(img.max_value() + 1);
    let bl = tuning.as_ref().and_then(|t| t.black_level).map_or(
        // 64 at 10 bits (the OV9782's), scaled.
        [64.0 / 1024.0; 3],
        |b| [b.r, b.g, b.b],
    );
    let p = dng_pattern(pattern);
    let black_level = p.colors().map(|c| bl[usize::from(c)] * full);
    let ct: Option<f64> = opt("--ct").and_then(|s| s.parse().ok());
    let neutral = match (ct, &tuning) {
        (Some(ct), Some(t)) => neutral_at(t, ct),
        _ => None,
    }
    .unwrap_or_else(|| grey_world(&img, p, black_level[0]));
    let calibrations = match tuning.as_ref().map(dng_calibrations) {
        Some(c) if !c.is_empty() => c,
        _ => shot_calibration(&styx_pipeline::styx_algo::IDENTITY, neutral),
    };
    let opcode_list2 = tuning
        .as_ref()
        .and_then(|t| t.alsc.clone())
        .and_then(|a| Alsc::new(a).ok())
        .map(|a| {
            let ls = a.tables(ct.unwrap_or(5000.0));
            styx_dng::opcode::bayer_gain_maps(
                p,
                (w, h),
                (ls.width, ls.height),
                [&ls.r, &ls.g, &ls.b],
            )
        })
        .unwrap_or_default();
    let native = frame.meta().native().copied();
    let meta = DngMetadata {
        black_level,
        as_shot_neutral: Some(neutral),
        calibrations,
        exposure_time: native.map(|n| std::time::Duration::from_nanos(n.exposure_ns)),
        iso: native.map(|n| (100.0 * n.gain()).round() as u32),
        model: sensor.clone(),
        unique_camera_model: format!("Styx {sensor}"),
        description: Some(format!(
            "{{\"recording\":\"{input}\",\"sequence\":{},\"timestamp_ns\":{}}}",
            frame.meta().sequence().unwrap_or(0),
            frame.meta().timestamp
        )),
        opcode_list2,
        ..DngMetadata::default()
    };
    std::fs::write(output.as_str(), styx_dng::write_dng(&img, &meta)?)?;
    println!(
        "{} {}x{} {} frame {} -> {output}: black {:?}, white {}, neutral [{:.3}, 1, {:.3}], {} calibration(s), {} gain maps",
        header.device.display,
        w,
        h,
        f.code,
        frame.meta().sequence().unwrap_or(0),
        meta.black_level,
        img.max_value(),
        neutral[0],
        neutral[2],
        meta.calibrations.len(),
        meta.opcode_list2.len()
    );
    Ok(())
}
