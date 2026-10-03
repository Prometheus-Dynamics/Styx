//! `styx-tune synth <dir>`: a synthetic calibration session (styx-tune's synthetic sensor:
//! dark frames at three gains, flats and charts at several temperatures, one chart at a known
//! lux) written as Styx raw recordings named the way `ctt` names its inputs, to try the tool:
//! `styx-tune synth /tmp/s && styx-tune calibrate /tmp/s`.

use std::path::Path;
use std::time::Duration;

use styx_algo::CameraConfig;
use styx_pipeline::SensorValues;
use styx_pipeline::rawrec::{Header, RawWriter, VERSION};
use styx_pipeline::sensor::SensorInfo;
use styx_softisp::{CfaPattern, RawFormat, RawPacking};
use styx_tune::synth::{SensorModel, Shot, Target, chart_placement};

use crate::{Args, Res};

fn write(dir: &Path, name: &str, m: &SensorModel, frames: &[styx_tune::raw::RawFrame]) -> Res<()> {
    let format = RawFormat::new(
        m.width as u32,
        m.height as u32,
        CfaPattern::Bggr,
        RawPacking::U16Le { bits: m.bits },
    );
    let header = Header {
        styx_raw_recording: VERSION,
        format,
        stride: m.width * 2,
        sensor: SensorInfo {
            width: m.width as u32,
            height: m.height as u32,
            cfa: CfaPattern::Bggr,
            bits: m.bits,
            black_level: 64.0 / 1024.0,
            camera: CameraConfig::default(),
            readout: Duration::ZERO,
        },
        notes: "styx-tune synthetic sensor".into(),
    };
    let mut w = RawWriter::create(&dir.join(name), &header)?;
    for (i, f) in frames.iter().enumerate() {
        let bytes: Vec<u8> = f.data.iter().flat_map(|v| v.to_le_bytes()).collect();
        let v = SensorValues {
            frame: i as u64,
            exposure: Duration::from_secs_f64(f.exposure_us / 1e6),
            analogue_gain: f.analogue_gain,
            digital_gain: 1.0,
            frame_duration: Duration::from_millis(33),
            verified: true,
        };
        w.write(&bytes, &v, i as u64 * 33_333_333)?;
    }
    w.finish()?;
    Ok(())
}

pub fn run(a: &Args) -> Res<()> {
    let dir = Path::new(a.positional.first().ok_or("styx-tune synth <dir>")?);
    std::fs::create_dir_all(dir)?;
    let m = SensorModel::default();
    let exposure = |ct: f64, light: f64, level: f64| {
        let peak = m.white_rgb(ct).into_iter().fold(0.0, f64::max);
        level * 1024.0 * m.electrons_per_code / (light * peak)
    };
    let n = 4;
    for (i, gain) in [1.0, 2.0, 4.0].into_iter().enumerate() {
        let s = Shot {
            ct: 5000.0,
            light: 0.0,
            exposure_us: 10_000.0,
            gain,
        };
        write(
            dir,
            &format!("dark_gain{gain}"),
            &m,
            &m.burst(&Target::Dark, &s, n, i as u64),
        )?;
    }
    for ct in [2800.0, 4000.0, 6500.0] {
        let s = Shot {
            ct,
            light: 0.2,
            exposure_us: exposure(ct, 0.2, 0.7),
            gain: 1.0,
        };
        write(
            dir,
            &format!("alsc_{ct}k"),
            &m,
            &m.burst(&Target::Flat(1.0), &s, n, ct as u64),
        )?;
    }
    let chart = Target::Chart {
        h: chart_placement(318.0, 204.0, 82.0, 4.0, 0.0002),
        background: 0.25,
    };
    for ct in [2800.0, 4000.0, 5000.0, 6500.0] {
        let s = Shot {
            ct,
            light: 0.2,
            exposure_us: exposure(ct, 0.2, 0.75),
            gain: 1.0,
        };
        let name = if ct == 5000.0 {
            format!("{ct}k_400l")
        } else {
            format!("{ct}k")
        };
        write(dir, &name, &m, &m.burst(&chart, &s, n, 100 + ct as u64))?;
    }
    println!(
        "wrote a synthetic session to {} (lux: 2000 per unit of light)",
        dir.display()
    );
    Ok(())
}
