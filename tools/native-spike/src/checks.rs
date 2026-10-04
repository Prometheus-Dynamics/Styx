//! Checks on the sensor description before anything touches the device.

use styx_sensor::{RegWrite, SensorDescription, Step, Timing};

fn writes(steps: &[Step]) -> impl Iterator<Item = &RegWrite> {
    steps.iter().filter_map(Step::as_write)
}

/// Problems that would let the sensor start streaming (drive its CSI-2 lanes out of LP-11)
/// before the bridge's start event: any write before `stream_on` that sets a `stream_on`
/// register to its `stream_on` value, or a software reset (`0x0103`, OmniVision and SMIA),
/// which the description notes must not appear after `MIPI_CTRL00` is set.
pub fn standby_problems(desc: &SensorDescription, mode: &str, format: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let on: Vec<&RegWrite> = writes(&desc.sequences.stream_on).collect();
    if on.is_empty() {
        problems.push("stream_on has no register writes".to_owned());
    }
    let mut check = |what: &str, steps: &[Step]| {
        for w in writes(steps) {
            if on
                .iter()
                .any(|s| s.address == w.address && s.value == w.value)
            {
                let w = styx_sensor::show_write(w);
                problems.push(format!("{what} writes {w}, the stream-on value"));
            }
            if w.address == 0x0103 && w.value & 1 != 0 {
                let w = styx_sensor::show_write(w);
                problems.push(format!("{what} writes {w} (software reset)"));
            }
        }
    };
    check("power_up", &desc.sequences.power_up);
    check("init", &desc.sequences.init);
    match desc.mode(mode) {
        Ok(m) => {
            check("mode registers", &m.registers);
            match desc.format_for(m, format) {
                Ok(f) => check("format registers", &f.registers),
                Err(e) => problems.push(e.to_string()),
            }
        }
        Err(e) => problems.push(e.to_string()),
    }
    let off: Vec<&RegWrite> = writes(&desc.sequences.stream_off).collect();
    for s in &on {
        if !off
            .iter()
            .any(|o| o.address == s.address && o.value != s.value)
        {
            let s = styx_sensor::show_write(s);
            problems.push(format!("stream_off does not undo {s}"));
        }
    }
    problems
}

/// A frame rate target and what the timing model gives for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RatePlan {
    /// Requested frames per second.
    pub requested: f64,
    /// Frame length in lines.
    pub frame_length: u32,
    /// Vertical blanking in lines.
    pub vblank: u32,
    /// The rate that frame length gives.
    pub fps: f64,
    /// Clamped to the mode's limits.
    pub clamped: bool,
}

/// Frame lengths for each target rate.
pub fn rate_plans(timing: &Timing, targets: &[f64]) -> Vec<RatePlan> {
    targets
        .iter()
        .map(|&requested| {
            let fl = timing.frame_length_for_fps(requested);
            RatePlan {
                requested,
                frame_length: fl.lines,
                vblank: fl.vblank,
                fps: fl.fps,
                clamped: fl.clamped,
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn ov9782() -> SensorDescription {
        SensorDescription::from_file(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/sensor/sensors/ov9782.toml"
        ))
        .unwrap()
    }

    #[test]
    fn ov9782_stays_in_standby_until_stream_on() {
        let d = ov9782();
        assert_eq!(
            standby_problems(&d, "1280x800", "raw10"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn early_stream_on_and_resets_are_caught() {
        let mut d = ov9782();
        d.sequences
            .init
            .push(Step::Write(RegWrite::byte(0x0100, 1)));
        d.sequences
            .power_up
            .push(Step::Write(RegWrite::byte(0x0103, 1)));
        let p = standby_problems(&d, "1280x800", "raw10");
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p[0].contains("power_up") && p[0].contains("software reset"));
        assert!(p[1].contains("init"));
        assert!(!standby_problems(&d, "nope", "raw10").is_empty());
    }

    #[test]
    fn ov9782_rates() {
        let d = ov9782();
        let t = d.timing("1280x800", "raw10").unwrap();
        let plans = rate_plans(&t, &[30.0, 60.0, 120.0, 500.0]);
        for p in &plans[..3] {
            assert!(!p.clamped, "{p:?}");
            assert!((p.fps - p.requested).abs() / p.requested < 0.002, "{p:?}");
            assert_eq!(p.frame_length, 800 + p.vblank);
        }
        assert!(plans[3].clamped);
        assert_eq!(plans[3].vblank, 110);
    }
}
