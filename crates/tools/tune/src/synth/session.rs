//! The standard calibration session of a synthetic sensor: dark frames at three gains, flat
//! fields at three temperatures, ColorChecker shots at four (the 5000 K one with a lux
//! reference), and the writer that puts such a session on disk as raw files and a session file.

use std::path::{Path, PathBuf};

use crate::error::{Result, TuneError};
use crate::input::encode_raw;
use crate::linalg::Mat3;
use crate::session::{Kind, Shot};
use crate::synth::{SensorModel, Shot as Light, Target, chart_placement};

pub const FRAMES: usize = 4;
/// Lux per unit of `Light::light` (e-/µs of a perfect white on axis).
pub const LUX_PER_LIGHT: f64 = 2000.0;

pub fn shot(
    name: &str,
    kind: Kind,
    ct: Option<f64>,
    lux: Option<f64>,
    frames: Vec<crate::raw::RawFrame>,
) -> Shot {
    Shot {
        name: name.into(),
        kind,
        ct,
        lux,
        corners: None,
        frames,
    }
}

/// The exposure that puts a perfect white's brightest channel on axis at `level` of full
/// scale (the usual exposure rule: a bright shot at 70-80% of full scale).
/// The exposure that puts a perfect white's brightest channel on axis at `level` of full
/// scale (the usual exposure rule: a bright shot at 70-80% of full scale).
pub fn exposure_for(m: &SensorModel, ct: f64, light: f64, gain: f64, level: f64) -> f64 {
    let peak = m.white_rgb(ct).into_iter().fold(0.0, f64::max);
    level * f64::from(1u32 << m.bits) * m.electrons_per_code / (light * gain * peak)
}

pub fn placement() -> Mat3 {
    chart_placement(318.0, 204.0, 82.0, 4.0, 0.0002)
}

/// The usual session: dark at gains 1, 2, 4; flats at three temperatures; charts at four,
/// the 5000 K one at a known lux.
pub fn standard_session(m: &SensorModel) -> (Vec<Shot>, f64) {
    let mut shots = Vec::new();
    for (i, gain) in [1.0, 2.0, 4.0].into_iter().enumerate() {
        let l = Light {
            ct: 5000.0,
            light: 0.0,
            exposure_us: 10_000.0,
            gain,
        };
        shots.push(shot(
            &format!("dark x{gain}"),
            Kind::Dark,
            None,
            None,
            m.burst(&Target::Dark, &l, FRAMES, 10 + i as u64),
        ));
    }
    for (i, ct) in [2800.0, 4000.0, 6500.0].into_iter().enumerate() {
        let light = 0.2;
        let l = Light {
            ct,
            light,
            exposure_us: exposure_for(m, ct, light, 1.0, 0.7),
            gain: 1.0,
        };
        shots.push(shot(
            &format!("flat {ct}"),
            Kind::Flat,
            Some(ct),
            None,
            m.burst(&Target::Flat(1.0), &l, FRAMES, 20 + i as u64),
        ));
    }
    let chart = Target::Chart {
        h: placement(),
        background: 0.25,
    };
    let lux_light = 0.15;
    for (i, ct) in [2800.0, 4000.0, 5000.0, 6500.0].into_iter().enumerate() {
        let light = if ct == 5000.0 { lux_light } else { 0.2 };
        let l = Light {
            ct,
            light,
            exposure_us: exposure_for(m, ct, light, 1.0, 0.75),
            gain: 1.0,
        };
        let lux = (ct == 5000.0).then_some(lux_light * LUX_PER_LIGHT);
        shots.push(shot(
            &format!("chart {ct}"),
            Kind::Macbeth,
            Some(ct),
            lux,
            m.burst(&chart, &l, FRAMES, 30 + i as u64),
        ));
    }
    (shots, lux_light)
}

/// Write `shots` into `dir` as raw files (`<n>-<kind>.raw`) and a `session.toml` naming them,
/// for sensor `sensor`. Every shot is written with `skip = 0`, so reading the session back
/// gives the same frames. Returns the session file's path.
pub fn write_session(dir: &Path, sensor: &str, shots: &[Shot]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(|e| TuneError::io(dir, e))?;
    let mut text = format!("sensor = {sensor:?}\n");
    for (i, shot) in shots.iter().enumerate() {
        let first = shot
            .frames
            .first()
            .ok_or_else(|| TuneError::Session(format!("{}: no frames", shot.name)))?;
        if shot.frames.iter().any(|f| {
            f.width != first.width
                || f.height != first.height
                || f.cfa != first.cfa
                || f.bits != first.bits
                || f.exposure_us != first.exposure_us
                || f.gain() != first.gain()
        }) {
            return Err(TuneError::Session(format!(
                "{}: frames differ in size or settings",
                shot.name
            )));
        }
        let kind = format!("{:?}", shot.kind).to_ascii_lowercase();
        let file = format!("{i:02}-{kind}.raw");
        let path = dir.join(&file);
        std::fs::write(&path, encode_raw(&shot.frames)).map_err(|e| TuneError::io(&path, e))?;
        text.push_str("\n[[shot]]\n");
        text.push_str(&format!("kind = {kind:?}\nfile = {file:?}\n"));
        text.push_str(&format!(
            "width = {}\nheight = {}\ncfa = {:?}\nbits = {}\n",
            first.width,
            first.height,
            format!("{:?}", first.cfa).to_ascii_lowercase(),
            first.bits
        ));
        text.push_str(&format!(
            "exposure_us = {:?}\ngain = {:?}\nskip = 0\n",
            first.exposure_us, first.analogue_gain
        ));
        if let Some(ct) = shot.ct {
            text.push_str(&format!("ct = {ct:?}\n"));
        }
        if let Some(lux) = shot.lux {
            text.push_str(&format!("lux = {lux:?}\n"));
        }
        if let Some(c) = shot.corners {
            text.push_str(&format!(
                "corners = [[{:?}, {:?}], [{:?}, {:?}], [{:?}, {:?}], [{:?}, {:?}]]\n",
                c[0][0], c[0][1], c[1][0], c[1][1], c[2][0], c[2][1], c[3][0], c[3][1]
            ));
        }
    }
    let path = dir.join("session.toml");
    std::fs::write(&path, text).map_err(|e| TuneError::io(&path, e))?;
    Ok(path)
}
