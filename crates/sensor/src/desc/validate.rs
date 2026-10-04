//! Consistency checks beyond what the types enforce.

use alloc::collections::BTreeSet;
use alloc::{format, string::String, vec::Vec};

use super::{Backend, Field, Gain, GainModel, Rect, SensorDescription, Step};
use crate::error::{Issue, Issues};

struct Checker {
    issues: Vec<Issue>,
    address_max: u32,
}

impl Checker {
    fn err(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.issues.push(Issue {
            path: path.into(),
            message: message.into(),
        });
    }

    fn check(&mut self, cond: bool, path: impl Into<String>, message: impl Into<String>) {
        if !cond {
            self.err(path, message);
        }
    }

    fn register(&mut self, path: &str, address: u16, bytes: u8) {
        if !(1..=4).contains(&bytes) {
            self.err(path, format!("bytes must be 1 to 4, got {bytes}"));
            return;
        }
        let last = u32::from(address) + u32::from(bytes) - 1;
        if last > self.address_max {
            self.err(
                path,
                format!("register 0x{address:04x} (+{bytes} bytes) does not fit an address of 0x{:x} max", self.address_max),
            );
        }
    }

    fn field(&mut self, path: &str, f: &Field) {
        self.register(path, f.address, f.bytes);
        let width = u32::from(f.bytes) * 8;
        let bits = u32::from(f.bits());
        if bits == 0 {
            self.err(path, "the field has no bits");
        } else if u32::from(f.shift) + bits > width {
            self.err(
                path,
                format!(
                    "shift {} + bits {} exceed the {width}-bit register",
                    f.shift, bits
                ),
            );
        }
    }

    fn steps(&mut self, path: &str, steps: &[Step], desc: &SensorDescription, power: bool) {
        for (i, step) in steps.iter().enumerate() {
            let p = format!("{path}[{i}]");
            match step {
                Step::Write(w) => {
                    self.register(&p, w.address, w.bytes);
                    let max = if w.bytes >= 4 {
                        u32::MAX
                    } else {
                        (1u32 << (w.bytes * 8)) - 1
                    };
                    if w.value > max {
                        self.err(
                            &p,
                            format!("value 0x{:x} does not fit {} byte(s)", w.value, w.bytes),
                        );
                    }
                }
                Step::Delay(_) => {}
                Step::Clock { role, .. } if power => {
                    if !desc.sensor.clocks.contains_key(role) {
                        self.err(
                            &p,
                            format!("clock '{role}' is not declared in sensor.clocks"),
                        );
                    }
                }
                Step::Gpio { .. } | Step::Supply { .. } if power => {}
                _ => self.err(&p, "only register writes and delays are allowed here"),
            }
        }
    }

    fn gain(&mut self, path: &str, g: &Gain, registers: bool) {
        match &g.register {
            Some(f) => {
                self.field(&format!("{path}.register"), f);
                self.check(
                    g.max_code <= f.max_value(),
                    path,
                    format!(
                        "max_code 0x{:x} does not fit the register field",
                        g.max_code
                    ),
                );
            }
            None if registers => self.err(path, "a register-driven sensor needs `register`"),
            None => {}
        }
        self.check(
            g.min_code <= g.default_code && g.default_code <= g.max_code,
            path,
            "codes must satisfy min_code <= default_code <= max_code",
        );
        match &g.model {
            GainModel::Linear { step, offset } => {
                self.check(*step > 0.0, path, "linear step must be positive");
                self.check(
                    i64::from(g.min_code) + offset > 0,
                    path,
                    "linear gain at min_code must be positive",
                );
            }
            GainModel::Reciprocal { numerator, base } => {
                self.check(
                    *numerator > 0.0,
                    path,
                    "reciprocal numerator must be positive",
                );
                self.check(
                    *base > f64::from(g.max_code),
                    path,
                    "reciprocal base must exceed max_code",
                );
            }
            GainModel::Table(t) => {
                self.check(!t.is_empty(), path, "gain table is empty");
                for w in t.windows(2) {
                    if w[1].1.partial_cmp(&w[0].1) != Some(core::cmp::Ordering::Greater) {
                        self.err(
                            path,
                            format!(
                                "gain table must increase: code 0x{:x} ({}) then 0x{:x} ({})",
                                w[0].0, w[0].1, w[1].0, w[1].1
                            ),
                        );
                    }
                }
                for (code, gain) in t {
                    self.check(
                        *gain > 0.0,
                        path,
                        format!("gain for code 0x{code:x} must be positive"),
                    );
                    self.check(
                        (g.min_code..=g.max_code).contains(code),
                        path,
                        format!("table code 0x{code:x} is outside min_code..=max_code"),
                    );
                }
            }
        }
    }
}

pub(super) fn validate(d: &SensorDescription) -> Result<(), Issues> {
    let mut c = Checker {
        issues: Vec::new(),
        address_max: 0xffff,
    };
    let registers = d.sensor.backend == Backend::Registers;

    // Bus wiring.
    for (path, message) in d.bus.iter().flat_map(|b| b.problems()) {
        c.err(path, message);
    }

    // Identity.
    c.check(
        !d.sensor.name.trim().is_empty(),
        "sensor.name",
        "must not be empty",
    );
    match d.sensor.address_bits {
        8 => c.address_max = 0xff,
        16 => {}
        n => c.err("sensor.address_bits", format!("must be 8 or 16, got {n}")),
    }
    match d.sensor.i2c_address {
        Some(a) if !(0x03..=0x77).contains(&a) => c.err(
            "sensor.i2c_address",
            format!("0x{a:x} is not a 7-bit I2C device address (0x03..=0x77)"),
        ),
        None if registers => c.err("sensor.i2c_address", "required for backend = \"registers\""),
        _ => {}
    }
    if let Some(id) = &d.sensor.chip_id {
        c.register("sensor.chip_id", id.address, id.bytes);
        c.check(
            !id.values.is_empty(),
            "sensor.chip_id.values",
            "must list at least one value",
        );
        let max = if id.bytes >= 4 {
            u32::MAX
        } else {
            (1u32 << (u32::from(id.bytes) * 8)) - 1
        };
        for v in &id.values {
            c.check(
                *v <= max,
                "sensor.chip_id.values",
                format!("0x{v:x} does not fit {} byte(s)", id.bytes),
            );
        }
    }
    for (role, rate) in &d.sensor.clocks {
        c.check(
            *rate > 0,
            format!("sensor.clocks.{role}"),
            "rate must be positive",
        );
    }

    // Pixel array.
    let pa = &d.pixel_array;
    let array = Rect {
        left: 0,
        top: 0,
        width: pa.size.width,
        height: pa.size.height,
    };
    c.check(
        array.contains(&pa.active),
        "pixel_array.active",
        "must lie inside pixel_array.size",
    );
    if let Some(bl) = pa.black_level {
        c.check(
            (1..=32).contains(&bl.bits),
            "pixel_array.black_level.bits",
            "must be 1 to 32",
        );
        c.check(
            bl.bits >= 32 || bl.value < (1u32 << bl.bits.min(31)),
            "pixel_array.black_level.value",
            "does not fit its bit depth",
        );
    }

    // Sequences.
    let s = &d.sequences;
    c.steps("sequences.power_up", &s.power_up, d, true);
    c.steps("sequences.power_down", &s.power_down, d, true);
    c.steps("sequences.init", &s.init, d, false);
    c.steps("sequences.stream_on", &s.stream_on, d, false);
    c.steps("sequences.stream_off", &s.stream_off, d, false);
    if registers {
        c.check(
            !s.stream_on.is_empty(),
            "sequences.stream_on",
            "a register-driven sensor needs a stream-on sequence",
        );
    }

    // Formats.
    c.check(
        !d.formats.is_empty(),
        "formats",
        "at least one format is required",
    );
    for (name, f) in &d.formats {
        let p = format!("formats.{name}");
        match (f.bit_depth, f.code.bit_depth()) {
            (None, None) => c.err(&p, format!("unknown code {}: give bit_depth", f.code)),
            (Some(a), Some(b)) if a != b => c.err(
                &p,
                format!("bit_depth {a} does not match {} ({b} bits)", f.code),
            ),
            _ => {}
        }
        if let Some(cf) = f.code.color_filter() {
            c.check(
                cf == pa.color_filter,
                &p,
                format!(
                    "code {} does not match pixel_array.color_filter {:?}",
                    f.code, pa.color_filter
                ),
            );
        }
        c.check(f.pixel_rate > 0, &p, "pixel_rate must be positive");
        c.steps(&format!("{p}.registers"), &f.registers, d, false);
    }

    // Modes.
    c.check(
        !d.modes.is_empty(),
        "modes",
        "at least one mode is required",
    );
    let mut names = BTreeSet::new();
    for (i, m) in d.modes.iter().enumerate() {
        let p = format!("modes[{i}] ({})", m.name);
        c.check(names.insert(m.name.as_str()), &p, "duplicate mode name");
        if let Some(fs) = &m.formats {
            for f in fs {
                c.check(
                    d.formats.contains_key(f),
                    format!("{p}.formats"),
                    format!("unknown format '{f}'"),
                );
            }
        }
        c.check(
            array.contains(&m.crop),
            format!("{p}.crop"),
            "must lie inside pixel_array.size",
        );
        c.check(
            m.size.width > 0 && m.size.height > 0,
            format!("{p}.size"),
            "must not be empty",
        );
        let axes = [
            ("width", m.size.width, m.crop.width),
            ("height", m.size.height, m.crop.height),
        ];
        for (axis, (name, out, crop)) in axes.into_iter().enumerate() {
            let factor = m.binning[axis].max(1) * m.skipping[axis].max(1);
            c.check(
                u64::from(out) * u64::from(factor) <= u64::from(crop),
                format!("{p}.size"),
                format!("{name} {out} x binning/skipping {factor} exceeds the crop {name} {crop}"),
            );
        }
        c.check(
            m.binning.iter().chain(&m.skipping).all(|f| *f >= 1),
            &p,
            "binning and skipping factors must be at least 1",
        );
        for (which, b) in [("hblank", &m.hblank), ("vblank", &m.vblank)] {
            c.check(
                b.min <= b.default && b.default <= b.max,
                format!("{p}.{which}"),
                format!(
                    "needs min <= default <= max, got {} / {} / {}",
                    b.min, b.default, b.max
                ),
            );
        }
        let fl_max = u64::from(m.size.height) + u64::from(m.vblank.max);
        if let Some(f) = &d.controls.frame_length {
            c.check(
                fl_max <= u64::from(f.max_value()),
                format!("{p}.vblank.max"),
                format!("frame length {fl_max} does not fit controls.frame_length"),
            );
        }
        if let Some(ll) = &d.controls.line_length {
            let max = (u64::from(m.size.width) + u64::from(m.hblank.max))
                .div_ceil(u64::from(ll.pixels_per_unit.max(1)));
            c.check(
                max <= u64::from(ll.register.max_value()),
                format!("{p}.hblank.max"),
                format!("line length {max} (register units) does not fit controls.line_length"),
            );
        }
        let e = &d.controls.exposure;
        let exp_max = (u64::from(m.size.height) + u64::from(m.vblank.min))
            .saturating_sub(u64::from(e.margin));
        c.check(
            exp_max >= u64::from(e.min),
            format!("{p}.vblank.min"),
            "leaves no room for the minimum exposure",
        );
        if let Some(r) = &m.pixel_rate {
            c.check(*r > 0, format!("{p}.pixel_rate"), "must be positive");
        }
        c.steps(&format!("{p}.registers"), &m.registers, d, false);
    }

    // Controls.
    let ctl = &d.controls;
    match &ctl.frame_length {
        Some(f) => c.field("controls.frame_length", f),
        None if registers => c.err(
            "controls.frame_length",
            "a register-driven sensor needs the frame length (VTS) register",
        ),
        None => {}
    }
    if let Some(ll) = &ctl.line_length {
        c.field("controls.line_length.register", &ll.register);
        c.check(
            ll.pixels_per_unit >= 1,
            "controls.line_length.pixels_per_unit",
            "must be at least 1",
        );
    }
    let e = &ctl.exposure;
    match &e.register {
        Some(f) => {
            c.field("controls.exposure.register", f);
            c.check(
                e.fraction_bits <= f.shift,
                "controls.exposure.fraction_bits",
                "fractional bits must fit below the integer field (fraction_bits <= shift)",
            );
        }
        None if registers => c.err(
            "controls.exposure",
            "a register-driven sensor needs `register`",
        ),
        None => {}
    }
    c.check(
        e.min >= 1 || e.fraction_bits > 0,
        "controls.exposure.min",
        "must be at least 1 line",
    );
    c.check(e.step >= 1, "controls.exposure.step", "must be at least 1");
    c.check(
        e.default >= e.min,
        "controls.exposure.default",
        "must be at least min",
    );
    c.gain("controls.analog_gain", &ctl.analog_gain, registers);
    if let Some(g) = &ctl.digital_gain {
        c.gain("controls.digital_gain", g, registers);
    }
    let dl = ctl.delays;
    for (n, v) in [
        ("exposure", dl.exposure),
        ("analog_gain", dl.analog_gain),
        ("digital_gain", dl.digital_gain),
        ("frame_length", dl.frame_length),
    ] {
        c.check(
            v <= 16,
            format!("controls.delays.{n}"),
            format!("{v} frames is implausible (max 16)"),
        );
    }
    if let Some(gh) = &ctl.group_hold {
        c.check(
            !gh.start.is_empty(),
            "controls.group_hold.start",
            "must not be empty",
        );
        c.steps("controls.group_hold.start", &gh.start, d, false);
        c.steps("controls.group_hold.end", &gh.end, d, false);
        c.steps("controls.group_hold.launch", &gh.launch, d, false);
    }
    for (n, f) in [("hflip", &ctl.hflip), ("vflip", &ctl.vflip)] {
        if let Some(f) = f {
            c.register(&format!("controls.{n}"), f.address, 1);
            c.check(f.mask != 0, format!("controls.{n}.mask"), "must not be 0");
        }
    }
    if let Some(tp) = &ctl.test_pattern {
        c.field("controls.test_pattern.register", &tp.register);
        c.check(
            tp.patterns.contains_key("off"),
            "controls.test_pattern.patterns",
            "must include `off`",
        );
        for (name, v) in &tp.patterns {
            c.check(
                *v <= tp.register.max_value(),
                format!("controls.test_pattern.patterns.{name}"),
                "does not fit the register field",
            );
        }
    }

    // Embedded data.
    if let Some(ed) = &d.embedded_data {
        c.check(ed.lines >= 1, "embedded_data.lines", "must be at least 1");
        let mut seen = BTreeSet::new();
        for (i, e) in ed.entries.iter().enumerate() {
            c.check(
                seen.insert(e.address),
                format!("embedded_data.entries[{i}]"),
                format!("register 0x{:04x} listed twice", e.address),
            );
        }
        let mut kinds = Vec::new();
        for (i, e) in ed.controls.iter().enumerate() {
            let at = format!("embedded_data.controls[{i}]");
            c.check(
                !kinds.contains(&e.control),
                at.clone(),
                "control listed twice",
            );
            kinds.push(e.control);
            c.check(
                (1..=4).contains(&e.bytes),
                at.clone(),
                "bytes must be 1..=4",
            );
            c.check(
                u32::from(e.bytes) * 8 + u32::from(e.shift) <= 32,
                at,
                "does not fit 32 bits",
            );
        }
        for (i, e) in ed.registers.iter().enumerate() {
            let at = format!("embedded_data.registers[{i}]");
            c.check(
                !kinds.contains(&e.control),
                at.clone(),
                "control listed twice",
            );
            kinds.push(e.control);
            c.check(
                (1..=4).contains(&e.bytes) && u32::from(e.bytes) * 8 + u32::from(e.shift) <= 32,
                at,
                "bytes must be 1..=4 and fit 32 bits with the shift",
            );
        }
    }

    if c.issues.is_empty() {
        Ok(())
    } else {
        Err(Issues(c.issues))
    }
}
