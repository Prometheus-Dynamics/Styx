//! Control verification on the native path: register read-back, test patterns, the control
//! delays (raw register writes at a known frame start), group hold, exposure and gain sweeps,
//! and the black level. Raw writes go straight to the bus, around the control scheduler; each
//! experiment restores the registers it changed.

use std::collections::BTreeMap;
use std::time::Duration;

use styx_sensor::{Control, ControlSet, RegisterBus, Step};

use crate::analysis::{FrameLevels, LevelStats, describe_frame, fit_line};
use crate::experiments::collect;
use crate::frames::FrameSample;
use crate::rig::Rig;
use crate::{Result, ResultExt, log};

/// Exposure register (lines << 4, 20 bits over three bytes).
const EXPOSURE: u16 = 0x3500;
/// Analogue gain, real-gain format with 4 fraction bits (0x3503 bit 3).
const GAIN: u16 = 0x3509;
/// Frame length (VTS).
const VTS: u16 = 0x380e;

/// One raw register write: address, value, bytes.
pub type RawWrite = (u16, u32, u8);

/// Parses `addr=value[:bytes]` lists (`0x5e00=0x80,0x3500=0x4000:3`).
pub fn parse_writes(s: &str) -> std::result::Result<Vec<RawWrite>, String> {
    let num = |v: &str| -> std::result::Result<u32, String> {
        let v = v.trim();
        let r = match v.strip_prefix("0x") {
            Some(h) => u32::from_str_radix(h, 16),
            None => v.parse(),
        };
        r.map_err(|_| format!("'{v}' is not a number"))
    };
    s.split(',')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let (a, rest) = p
                .split_once('=')
                .ok_or(format!("'{p}' is not addr=value"))?;
            let (v, b) = rest.split_once(':').unwrap_or((rest, "1"));
            let (a, v, b) = (num(a)?, num(v)?, num(b)?);
            if a > 0xffff || !(1..=4).contains(&b) || (b < 4 && v >> (8 * b) != 0) {
                return Err(format!("'{p}': bad address, value or width"));
            }
            Ok((a as u16, v, b as u8))
        })
        .collect()
}

/// Writes raw registers.
pub fn write(rig: &Rig, writes: &[RawWrite]) -> Result<()> {
    let mut d = rig.driver();
    for &(a, v, b) in writes {
        d.bus_mut().write(a, b, v).ctx(&format!("write {a:#06x}"))?;
    }
    Ok(())
}

fn read(rig: &Rig, address: u16, bytes: u8) -> Result<u32> {
    rig.driver()
        .bus_mut()
        .read(address, bytes)
        .ctx(&format!("read {address:#06x}"))
}

/// Reads back every register the description writes (init, format, mode) and reports those
/// that differ, then the control and diagnostic registers.
pub fn regdump(rig: &Rig, mode: &str, format: &str) -> Result<()> {
    let desc = rig.driver().description().clone();
    let m = desc.mode(mode).ctx("mode")?;
    let f = desc.format_for(m, format).ctx("format")?;
    let mut want: BTreeMap<u16, (u32, u8)> = BTreeMap::new();
    for steps in [
        &desc.sequences.power_up,
        &desc.sequences.init,
        &f.registers,
        &m.registers,
    ] {
        for w in steps.iter().filter_map(Step::as_write) {
            want.insert(w.address, (w.value, w.bytes));
        }
    }
    let flips: Vec<u16> = [desc.controls.hflip, desc.controls.vflip]
        .iter()
        .flatten()
        .map(|f| f.address)
        .collect();
    let mut differ = 0;
    for (&a, &(v, b)) in &want {
        let got = read(rig, a, b)?;
        if got != v {
            differ += 1;
            let why = if flips.contains(&a) {
                " (flip bits)"
            } else {
                ""
            };
            log!("regdump: {a:#06x} wrote {v:#04x} reads {got:#04x}{why}");
        }
    }
    log!(
        "regdump: {} registers written by the description, {differ} read back different",
        want.len()
    );
    for (a, b, what) in [
        (0x0100, 1, "mode select"),
        (EXPOSURE, 3, "exposure (lines << 4)"),
        (0x3503, 1, "AEC manual / gain format"),
        (0x3508, 1, "gain[12:8]"),
        (GAIN, 1, "gain[7:0]"),
        (0x350a, 2, "digital gain"),
        (0x380c, 2, "HTS"),
        (VTS, 2, "VTS"),
        (0x3820, 1, "format 1"),
        (0x3821, 1, "format 2"),
        (0x4002, 2, "BLC target"),
        (0x4307, 1, "embedded line control"),
        (0x4800, 1, "MIPI ctrl 00"),
        (0x4814, 1, "MIPI data type"),
        (0x5000, 1, "ISP ctrl 00"),
        (0x5001, 1, "ISP ctrl 01"),
        (0x5e00, 1, "test pattern"),
        (0x4320, 1, "solid pattern ctrl"),
        (0x3208, 1, "group access"),
        (0x3308, 1, "0x3308 (driver's group hold)"),
    ] {
        log!("regdump: {a:#06x} = {:#x}  {what}", read(rig, a, b)?);
    }
    Ok(())
}

/// Collects `n` frames with their data and returns their levels.
fn grab(rig: &mut Rig, n: usize) -> Result<Vec<(u32, FrameLevels)>> {
    let timeout = Duration::from_secs(3);
    let mut out = Vec::new();
    while out.len() < n {
        let f = rig.next_frame(timeout, true)?.ok_or("no frame")?;
        let data = f.data.ok_or("no data")?;
        out.push((
            f.sample.sequence,
            FrameLevels::unpack(&rig.layout, &data).ctx("unpack")?,
        ));
    }
    Ok(out)
}

/// Active-area statistics of the next `n` frames, averaged: the rows below the first
/// `skip_rows` (the band the frame starts with, see `describe_frame`).
fn level(rig: &mut Rig, n: usize, skip_rows: usize) -> Result<LevelStats> {
    let frames = grab(rig, n)?;
    let all: Vec<u16> = frames
        .iter()
        .flat_map(|(_, f)| f.pixels[skip_rows * f.width..].iter().copied())
        .collect();
    Ok(LevelStats::of(&all))
}

/// Describes the next frame (row bands, statistics, phases).
pub fn describe_next(rig: &mut Rig, label: &str) -> Result<()> {
    let (seq, f) = grab(rig, 1)?.remove(0);
    describe_frame(&format!("{label} (frame {seq})"), &f);
    Ok(())
}

/// Test patterns: each named pattern of the description, then a solid pattern of known values
/// (0x4320 bit 1, P1..P4 at 0x4322..0x4329), which checks the data path bit for bit.
pub fn test_patterns(rig: &mut Rig) -> Result<()> {
    let names: Vec<String> = rig
        .driver()
        .description()
        .controls
        .test_pattern
        .as_ref()
        .map(|t| t.patterns.keys().cloned().collect())
        .unwrap_or_default();
    for name in names.iter().filter(|n| *n != "off") {
        rig.driver().set_test_pattern(name).ctx("test pattern")?;
        collect(rig, 3)?;
        describe_next(rig, &format!("pattern {name}"))?;
    }
    rig.driver()
        .set_test_pattern("off")
        .ctx("test pattern off")?;
    let solid = [0x3ffu32, 0x155, 0x2aa, 0x040];
    let ctrl = read(rig, 0x4320, 1)?;
    let mut writes: Vec<RawWrite> = Vec::new();
    for (i, v) in solid.iter().enumerate() {
        let base = 0x4322 + 2 * i as u16;
        writes.push((base, v >> 8, 1));
        writes.push((base + 1, v & 0xff, 1));
    }
    writes.push((0x4320, ctrl | 0x02, 1));
    write(rig, &writes)?;
    collect(rig, 3)?;
    log!("solid pattern: P1..P4 = {solid:#05x?} (register order P1 P2 P4 P3 at 0x4322..)");
    describe_next(rig, "solid pattern")?;
    write(rig, &[(0x4320, ctrl, 1)])?;
    collect(rig, 3)?;
    Ok(())
}

/// Writes `writes` right after a frame start and returns that frame (the one the writes happen
/// in) and the frames up to `after` frames later.
fn step(rig: &mut Rig, writes: &[RawWrite], after: u32) -> Result<(u32, Vec<FrameSample>)> {
    collect(rig, 1)?;
    let during = rig.current_frame().ok_or("no frame start seen")?;
    write(rig, writes)?;
    let mut frames = Vec::new();
    loop {
        let f = collect(rig, 1)?.remove(0);
        let done = f.sequence >= during + after;
        frames.push(f);
        if done {
            return Ok((during, frames));
        }
    }
}

/// The first frame from `from` whose level is past the midpoint between `before` and `after`.
fn first_moved(frames: &[FrameSample], from: u32, before: f64, after: f64) -> Option<u32> {
    let mid = (before + after) / 2.0;
    frames
        .iter()
        .filter(|f| f.sequence >= from)
        .find(|f| (f.mean - mid) * (after - before) > 0.0)
        .map(|f| f.sequence)
}

/// Frame `k`'s duration is `ts[k+1] - ts[k]` (buffer timestamps are frame starts on rp1-cfe).
fn first_long(frames: &[FrameSample], from: u32, period: f64) -> Option<u32> {
    frames
        .windows(2)
        .filter(|w| w[0].sequence >= from && w[1].sequence == w[0].sequence + 1)
        .find(|w| {
            let dt = (w[1].timestamp - w[0].timestamp).as_secs_f64();
            (dt - period).abs() / period < 0.05
        })
        .map(|w| w[0].sequence)
}

fn exposure_write(lines: u32) -> RawWrite {
    (EXPOSURE, lines << 4, 3)
}

/// Measures the delay of exposure, gain and frame length: a raw write right after the start
/// of frame N, then the first frame whose level (or duration) changed. Three times each way.
/// Needs a scene that is not black at the base exposure and gain.
pub fn delays(rig: &mut Rig, base_lines: u32, base_gain: u32) -> Result<()> {
    let t = rig.timing;
    write(rig, &[exposure_write(base_lines), (GAIN, base_gain, 1)])?;
    collect(rig, 6)?;
    let lo = level(rig, 3, 32)?.mean;
    let cases: [(&str, RawWrite, RawWrite); 2] = [
        (
            "exposure",
            exposure_write(base_lines * 2),
            exposure_write(base_lines),
        ),
        ("gain", (GAIN, base_gain * 2, 1), (GAIN, base_gain, 1)),
    ];
    for (name, up, down) in cases {
        let mut seen = Vec::new();
        for _ in 0..3 {
            for (w, from, to) in [(up, lo, 2.0 * lo), (down, 2.0 * lo, lo)] {
                let (during, frames) = step(rig, &[w], 6)?;
                let moved = first_moved(&frames, during, from, to);
                log_embedded(rig, &format!("delay {name}"), during, &frames);
                log!(
                    "delay {name}: wrote {:#x} in frame {during}; levels {:?}; first moved {moved:?}",
                    w.1,
                    frames
                        .iter()
                        .map(|f| (f.sequence, (f.mean * 10.0).round() / 10.0))
                        .collect::<Vec<_>>()
                );
                seen.push(moved.map(|m| m - during));
            }
        }
        log!("delay {name}: {seen:?} frames");
    }
    let base_fl = t.frame_length_default();
    let long = base_fl * 3 / 2;
    let mut seen = Vec::new();
    for _ in 0..3 {
        for fl in [long, base_fl] {
            let (during, frames) = step(rig, &[(VTS, fl, 2)], 6)?;
            let period = t.frame_duration(fl).as_secs_f64();
            let first = first_long(&frames, during, period);
            log_embedded(rig, "delay frame length", during, &frames);
            seen.push(first.map(|m| m - during));
        }
    }
    log!("delay frame length (duration of frame k = ts[k+1] - ts[k]): {seen:?} frames");
    Ok(())
}

/// Group hold: exposure doubled and gain halved together (the same brightness), with a pause
/// of 1.5 frames between the two writes so that without a working hold one of them lands a
/// frame before the other. Tries no hold, the kernel driver's 0x3308, and 0x3208 group 0.
pub fn group_hold(rig: &mut Rig, base_lines: u32, base_gain: u32) -> Result<()> {
    let pause = rig.timing.frame_duration(rig.timing.frame_length_default()) * 3 / 2;
    type Seq = &'static [RawWrite];
    let variants: [(&str, Seq, Seq); 3] = [
        ("none", &[], &[]),
        ("0x3308", &[(0x3308, 1, 1)], &[(0x3308, 0, 1)]),
        (
            "0x3208",
            &[(0x3208, 0x00, 1)],
            &[(0x3208, 0x10, 1), (0x3208, 0xa0, 1)],
        ),
    ];
    for (name, start, end) in variants {
        for (lines, gain) in [(base_lines * 2, base_gain / 2), (base_lines, base_gain)] {
            collect(rig, 4)?;
            let before = level(rig, 2, 32)?.mean;
            let during = rig.current_frame().ok_or("no frame start")?;
            write(rig, start)?;
            write(rig, &[exposure_write(lines)])?;
            std::thread::sleep(pause);
            write(rig, &[(GAIN, gain, 1)])?;
            write(rig, end)?;
            let frames = collect(rig, 8)?;
            log_embedded(rig, &format!("group hold {name}"), during, &frames);
            let worst = frames
                .iter()
                .map(|f| (f.mean - before) / before.max(1.0))
                .fold(0.0f64, |a, r| if r.abs() > a.abs() { r } else { a });
            log!(
                "group hold {name}: exposure {lines} + gain {gain:#x} from frame {during} (gain written 1.5 frames later); level {before:.1}; frames {:?}; largest excursion {:+.0}%",
                frames
                    .iter()
                    .map(|f| (f.sequence, (f.mean * 10.0).round() / 10.0))
                    .collect::<Vec<_>>(),
                worst * 100.0
            );
        }
    }
    Ok(())
}

/// Steps through `values` of one control (raw writes), waits `settle` frames at each and
/// averages the active area of the next frames; fits a line to level against value.
pub fn sweep(rig: &mut Rig, what: &str, values: &[u32], other: u32, settle: usize) -> Result<()> {
    let mut points = Vec::new();
    for &v in values {
        let (exp, gain) = if what == "exposure" {
            (v, other)
        } else {
            (other, v)
        };
        write(rig, &[exposure_write(exp), (GAIN, gain, 1)])?;
        collect(rig, settle)?;
        let s = level(rig, 3, 32)?;
        log!("sweep {what}: exposure {exp} lines, gain {gain:#x}: {s}");
        if s.saturated < 0.01 && s.p01 > 0 {
            points.push((f64::from(v), s.mean));
        }
    }
    match fit_line(&points) {
        Some(f) => log!(
            "sweep {what}: {} unsaturated points: level = {:.4} x {what} + {:.2}; r2 {:.5}, largest residual {:.1}% of range",
            points.len(),
            f.slope,
            f.intercept,
            f.r2,
            f.max_rel_residual * 100.0
        ),
        None => log!("sweep {what}: too few unsaturated points to fit"),
    }
    Ok(())
}

/// The black level: active-area and top-band statistics at the minimum exposure and 1x gain
/// (the scene barely contributes), then back to the scheduler's values.
pub fn black_level(rig: &mut Rig) -> Result<()> {
    write(rig, &[exposure_write(1), (GAIN, 0x10, 1)])?;
    collect(rig, 5)?;
    let frames = grab(rig, 4)?;
    for (seq, f) in &frames[..1] {
        describe_frame(&format!("black (1 line, 1x) frame {seq}"), f);
    }
    let all: Vec<u16> = frames
        .iter()
        .flat_map(|(_, f)| f.pixels[32 * f.width..].iter().copied())
        .collect();
    log!("black level, rows 32.., 4 frames: {}", LevelStats::of(&all));
    Ok(())
}

/// Embedded data: dumps a few buffers, searches them for the current exposure, gain and VTS
/// bytes, decodes them with the description's layout (if any), then changes exposure, gain and
/// frame length with raw writes after a frame start and prints per frame what the embedded
/// data reports, which gives the frame each register value is applied to.
pub fn embedded_report(rig: &mut Rig) -> Result<()> {
    collect(rig, 4)?;
    let seq = rig.current_frame().ok_or("no frame")?.saturating_sub(1);
    let exp = read(rig, EXPOSURE, 3)?;
    let gain = read(rig, GAIN, 1)?;
    let vts = read(rig, VTS, 2)?;
    let Some(emb) = &rig.embedded else {
        return Err("no embedded node".into());
    };
    let Some(f) = emb.frame(seq).or(emb.frames.back()) else {
        log!(
            "embedded: no buffers received (the sensor sends no embedded data with these settings)"
        );
        return Ok(());
    };
    log!(
        "embedded: frame {} {} bytes; exposure {exp:#08x} gain {gain:#04x} vts {vts:#06x}\n    {}",
        f.sequence,
        f.used,
        crate::embedded::hex(&f.head)
    );
    for (name, bytes) in [
        ("exposure", exp.to_be_bytes()[1..].to_vec()),
        ("vts", vts.to_be_bytes()[2..].to_vec()),
    ] {
        let at: Vec<usize> = f
            .head
            .windows(bytes.len())
            .enumerate()
            .filter(|(_, w)| *w == bytes.as_slice())
            .map(|(i, _)| i)
            .collect();
        log!("embedded: {name} bytes {bytes:02x?} found at offsets {at:?}");
    }
    let changes: [(&str, RawWrite, RawWrite); 3] = [
        ("exposure", exposure_write(1000), (EXPOSURE, exp, 3)),
        ("gain", (GAIN, 0x40, 1), (GAIN, gain, 1)),
        ("vts", (VTS, vts + 100, 2), (VTS, vts, 2)),
    ];
    let desc = rig.driver().description().clone();
    for (name, w, undo) in changes {
        let (during, frames) = step(rig, &[w], 5)?;
        for fr in &frames {
            let e = rig.embedded.as_ref().and_then(|e| e.frame(fr.sequence));
            let decoded = e.map(|e| desc.decode_embedded(&e.head));
            log!(
                "embedded {name}: wrote {:#x} in frame {during}; frame {} {}{}",
                w.1,
                fr.sequence,
                e.map_or("no buffer".into(), |e| crate::embedded::hex(
                    &e.head[..e.head.len().min(64)]
                )),
                decoded.map_or(String::new(), |d| format!("  decoded {d:?}"))
            );
        }
        write(rig, &[undo])?;
        collect(rig, 4)?;
    }
    Ok(())
}

/// Logs, per frame, the exposure, gain and frame length the embedded data reports.
fn log_embedded(rig: &Rig, label: &str, during: u32, frames: &[FrameSample]) {
    let Some(emb) = &rig.embedded else { return };
    let desc = rig.driver().description().clone();
    let per: Vec<String> = frames
        .iter()
        .map(|f| match emb.frame(f.sequence) {
            Some(e) => {
                let c = desc.decode_embedded(&e.head);
                format!(
                    "{}: exp {:?} gain {:?} vts {:?}",
                    f.sequence,
                    c.get(Control::Exposure),
                    c.get(Control::AnalogGain).map(|g| format!("{g:#x}")),
                    c.get(Control::FrameLength)
                )
            }
            None => format!("{}: -", f.sequence),
        })
        .collect();
    log!(
        "{label} (writes in frame {during}), embedded data: {}",
        per.join("; ")
    );
}

/// Writes the scheduler's current exposure and gain back (after raw experiments).
pub fn restore(rig: &mut Rig) -> Result<()> {
    let mut d = rig.driver();
    let seq = rig.current_frame().unwrap_or(0);
    let codes: ControlSet = d
        .scheduler()
        .map(|s| s.predicted(u64::from(seq)))
        .unwrap_or_default();
    let exp = codes.get(Control::Exposure).unwrap_or(642);
    let gain = codes.get(Control::AnalogGain).unwrap_or(0x10);
    let fl = codes.get(Control::FrameLength);
    d.bus_mut().write(EXPOSURE, 3, exp << 4).ctx("exposure")?;
    d.bus_mut().write(GAIN, 1, gain).ctx("gain")?;
    if let Some(fl) = fl {
        d.bus_mut().write(VTS, 2, fl).ctx("vts")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_parse() {
        assert_eq!(
            parse_writes("0x5e00=0x80, 0x3500=0x4000:3,16=3").unwrap(),
            [(0x5e00, 0x80, 1), (0x3500, 0x4000, 3), (16, 3, 1)]
        );
        for bad in ["0x5e00", "0x5e00=0x100", "0x10000=1", "1=1:5", "x=1"] {
            assert!(parse_writes(bad).is_err(), "{bad}");
        }
    }

    fn samples(means: &[f64], periods_ms: &[u64]) -> Vec<FrameSample> {
        let mut t = 0;
        means
            .iter()
            .enumerate()
            .map(|(i, &m)| {
                let ts = Duration::from_millis(t);
                t += periods_ms.get(i).copied().unwrap_or(33);
                FrameSample {
                    sequence: 10 + i as u32,
                    timestamp: ts,
                    mean: m,
                    error: false,
                }
            })
            .collect()
    }

    #[test]
    fn finds_moves_and_long_frames() {
        let f = samples(&[100.0, 100.0, 100.0, 200.0, 200.0], &[]);
        assert_eq!(first_moved(&f, 10, 100.0, 200.0), Some(13));
        assert_eq!(first_moved(&f, 10, 200.0, 100.0), Some(10));
        let f = samples(&[0.0; 5], &[33, 33, 50, 50]);
        // Frame 12 lasts from ts[12] to ts[13]: 50 ms.
        assert_eq!(first_long(&f, 10, 0.050), Some(12));
    }
}
