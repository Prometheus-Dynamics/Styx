//! Generic defaults for sensors without a description: build one from what the kernel driver
//! reports, with controls going through V4L2 controls instead of registers.

use std::collections::{BTreeMap, BTreeSet};

use crate::desc::{
    Backend, Blanking, Controls, Delays, Exposure, Format, Gain, GainModel, Identity, Mode,
    PixelArray, Rect, SensorDescription, Sequences, Size,
};
use crate::error::{Issue, Issues, Result, SensorError};
use crate::mbus::MbusCode;
use crate::schedule::{Control, ControlSet};

/// Sensor-related V4L2 controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KernelControl {
    /// `V4L2_CID_EXPOSURE` (lines).
    Exposure,
    /// `V4L2_CID_ANALOGUE_GAIN` (code).
    AnalogueGain,
    /// `V4L2_CID_DIGITAL_GAIN` (code).
    DigitalGain,
    /// `V4L2_CID_VBLANK` (lines).
    Vblank,
    /// `V4L2_CID_HBLANK` (pixels).
    Hblank,
    /// `V4L2_CID_PIXEL_RATE` (pixels per second).
    PixelRate,
    /// `V4L2_CID_LINK_FREQ` (menu index).
    LinkFreq,
}

impl KernelControl {
    /// The V4L2 control id.
    pub const fn cid(self) -> u32 {
        match self {
            KernelControl::Exposure => 0x0098_0911,
            KernelControl::AnalogueGain => 0x009e_0903,
            KernelControl::DigitalGain => 0x009f_0905,
            KernelControl::Vblank => 0x009e_0901,
            KernelControl::Hblank => 0x009e_0902,
            KernelControl::PixelRate => 0x009f_0902,
            KernelControl::LinkFreq => 0x009f_0901,
        }
    }

    /// The control for a V4L2 control id.
    pub fn from_cid(cid: u32) -> Option<Self> {
        use KernelControl::*;
        [
            Exposure,
            AnalogueGain,
            DigitalGain,
            Vblank,
            Hblank,
            PixelRate,
            LinkFreq,
        ]
        .into_iter()
        .find(|c| c.cid() == cid)
    }
}

/// A control's range as the kernel reports it (`VIDIOC_QUERY_EXT_CTRL`) and its current value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlRange {
    /// Minimum.
    pub min: i64,
    /// Maximum.
    pub max: i64,
    /// Step.
    pub step: u64,
    /// Default.
    pub default: i64,
    /// Current value, if read.
    pub value: Option<i64>,
}

/// One media bus code and its frame sizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubdevFormat {
    /// The code.
    pub code: MbusCode,
    /// Discrete frame sizes.
    pub sizes: Vec<Size>,
}

/// What a sensor subdevice reports, as plain data (filled in by `styx-kernel`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SubdevReport {
    /// Entity name, e.g. `ov9782 10-0060`.
    pub name: String,
    /// Codes and sizes.
    pub formats: Vec<SubdevFormat>,
    /// `V4L2_SEL_TGT_NATIVE_SIZE`.
    pub native_size: Option<Size>,
    /// `V4L2_SEL_TGT_CROP_BOUNDS`.
    pub crop_bounds: Option<Rect>,
    /// The format size the control ranges below were read at (default: the first size).
    pub current_size: Option<Size>,
    /// Sensor controls.
    pub controls: BTreeMap<KernelControl, ControlRange>,
    /// Link frequency menu (Hz).
    pub link_frequencies: Vec<i64>,
    /// Analogue gain code for 1× (default: the minimum code). Kernel drivers do not report the
    /// gain model; a linear model with this unity code is assumed.
    pub analogue_gain_unity: Option<i64>,
}

fn issue(path: &str, message: &str) -> SensorError {
    SensorError::Invalid {
        source_name: "subdev report".into(),
        issues: Issues(vec![Issue {
            path: path.into(),
            message: message.into(),
        }]),
    }
}

fn to_u32(v: i64) -> u32 {
    v.clamp(0, i64::from(u32::MAX)) as u32
}

fn blanking(r: Option<&ControlRange>) -> Blanking {
    match r {
        Some(r) => Blanking {
            min: to_u32(r.min),
            max: to_u32(r.max),
            default: to_u32(r.value.unwrap_or(r.default)),
        },
        None => Blanking {
            min: 0,
            max: 0,
            default: 0,
        },
    }
}

fn linear_gain(r: &ControlRange, unity: i64) -> Gain {
    Gain {
        register: None,
        min_code: to_u32(r.min),
        max_code: to_u32(r.max),
        default_code: to_u32(r.default),
        model: GainModel::Linear {
            step: 1.0 / unity.max(1) as f64,
            offset: 0,
        },
    }
}

impl SensorDescription {
    /// A description for a sensor with a kernel driver, from what its subdevice reports.
    ///
    /// Requires `PIXEL_RATE`, `EXPOSURE` and `ANALOGUE_GAIN`. Blanking ranges are those
    /// reported at `current_size` and are used for every size (the kernel adjusts them per
    /// mode; re-read them after changing the format). Gain is assumed linear with
    /// `analogue_gain_unity` as 1×; delays are libcamera's defaults for unknown sensors.
    pub fn from_subdev(report: &SubdevReport) -> Result<Self> {
        let ctl = |c| report.controls.get(&c);
        let pixel_rate = ctl(KernelControl::PixelRate)
            .ok_or_else(|| issue("controls", "PIXEL_RATE is required"))?;
        let exposure = ctl(KernelControl::Exposure)
            .ok_or_else(|| issue("controls", "EXPOSURE is required"))?;
        let again = ctl(KernelControl::AnalogueGain)
            .ok_or_else(|| issue("controls", "ANALOGUE_GAIN is required"))?;
        let first = report
            .formats
            .first()
            .ok_or_else(|| issue("formats", "no media bus codes"))?;
        let color_filter = first
            .code
            .color_filter()
            .ok_or_else(|| issue("formats", &format!("unknown media bus code {}", first.code)))?;

        let largest = report
            .formats
            .iter()
            .flat_map(|f| &f.sizes)
            .max_by_key(|s| u64::from(s.width) * u64::from(s.height));
        let largest = *largest.ok_or_else(|| issue("formats", "no frame sizes"))?;
        let active = report.crop_bounds.unwrap_or(Rect {
            left: 0,
            top: 0,
            width: largest.width,
            height: largest.height,
        });
        let size = report.native_size.unwrap_or(Size::new(
            active.left + active.width,
            active.top + active.height,
        ));

        let hblank = blanking(ctl(KernelControl::Hblank));
        let vblank = blanking(ctl(KernelControl::Vblank));
        let current = report.current_size.unwrap_or(largest);
        let frame_length = i64::from(current.height) + i64::from(vblank.default);
        let exposure_max = exposure.max.min(frame_length);
        let pixel_rate = u64::try_from(pixel_rate.value.unwrap_or(pixel_rate.default))
            .unwrap_or(0)
            .max(1);
        let link_frequency = ctl(KernelControl::LinkFreq)
            .and_then(|l| {
                report
                    .link_frequencies
                    .get(usize::try_from(l.value.unwrap_or(l.default)).ok()?)
            })
            .map(|f| f.unsigned_abs());

        let mut formats = BTreeMap::new();
        let mut names = BTreeMap::new();
        for f in &report.formats {
            let bits = f.code.bit_depth().unwrap_or(0);
            let short = format!("raw{bits}");
            let name = if formats.contains_key(&short) {
                f.code.to_string().to_lowercase()
            } else {
                short
            };
            names.insert(f.code, name.clone());
            formats.insert(
                name,
                Format {
                    code: f.code,
                    bit_depth: None,
                    pixel_rate,
                    link_frequency,
                    registers: Vec::new(),
                },
            );
        }

        let sizes: BTreeSet<(u32, u32)> = report
            .formats
            .iter()
            .flat_map(|f| &f.sizes)
            .map(|s| (s.width, s.height))
            .collect();
        let mut modes = Vec::new();
        for (w, h) in sizes.into_iter().rev() {
            let supported: Vec<String> = report
                .formats
                .iter()
                .filter(|f| f.sizes.contains(&Size::new(w, h)))
                .map(|f| names[&f.code].clone())
                .collect();
            modes.push(Mode {
                name: format!("{w}x{h}"),
                size: Size::new(w, h),
                crop: active,
                formats: (supported.len() != formats.len()).then_some(supported),
                binning: [1, 1],
                skipping: [1, 1],
                hblank,
                vblank,
                pixel_rate: None,
                registers: Vec::new(),
            });
        }

        let unity = report.analogue_gain_unity.unwrap_or(again.min.max(1));
        let desc = SensorDescription {
            sensor: Identity {
                name: report
                    .name
                    .split_whitespace()
                    .next()
                    .unwrap_or("unknown")
                    .to_owned(),
                vendor: None,
                backend: Backend::Kernel,
                i2c_address: None,
                address_bits: 16,
                chip_id: None,
                clocks: BTreeMap::new(),
                tuning: None,
            },
            pixel_array: PixelArray {
                size,
                active,
                color_filter,
                black_level: None,
            },
            sequences: Sequences::default(),
            formats,
            modes,
            controls: Controls {
                frame_length: None,
                line_length: None,
                exposure: Exposure {
                    register: None,
                    fraction_bits: 0,
                    min: to_u32(exposure.min.max(1)),
                    margin: to_u32(frame_length - exposure_max),
                    step: to_u32(i64::try_from(exposure.step).unwrap_or(1).max(1)),
                    default: to_u32(exposure.default.max(exposure.min).max(1)),
                },
                analog_gain: linear_gain(again, unity),
                digital_gain: ctl(KernelControl::DigitalGain).map(|d| linear_gain(d, d.default)),
                delays: Delays::default(),
                group_hold: None,
                hflip: None,
                vflip: None,
                test_pattern: None,
            },
            embedded_data: None,
        };
        desc.validate().map_err(|issues| SensorError::Invalid {
            source_name: "subdev report".into(),
            issues,
        })?;
        Ok(desc)
    }
}

/// The V4L2 control values for scheduled codes on a kernel-driven sensor:
/// frame length becomes `VBLANK = frame_length - height`, exposure is in whole lines.
pub fn kernel_controls(
    codes: &ControlSet,
    height: u32,
    exposure_fraction_bits: u8,
) -> Vec<(KernelControl, i64)> {
    codes
        .iter()
        .map(|(c, v)| match c {
            Control::FrameLength => (KernelControl::Vblank, i64::from(v) - i64::from(height)),
            Control::Exposure => (
                KernelControl::Exposure,
                i64::from(v >> exposure_fraction_bits),
            ),
            Control::AnalogGain => (KernelControl::AnalogueGain, i64::from(v)),
            Control::DigitalGain => (KernelControl::DigitalGain, i64::from(v)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(min: i64, max: i64, default: i64, value: Option<i64>) -> ControlRange {
        ControlRange {
            min,
            max,
            step: 1,
            default,
            value,
        }
    }

    /// What the device's `ov9282` kernel driver reported for the OV9782 (read with
    /// `VIDIOC_QUERY_EXT_CTRL` / `VIDIOC_SUBDEV_ENUM_*`). The current VBLANK value was not read;
    /// 2850 is implied by the exposure maximum 3638 with the driver's 12-line margin.
    pub(crate) fn helios_ov9782() -> SubdevReport {
        let sizes = vec![Size::new(1280, 800)];
        SubdevReport {
            name: "ov9782 10-0060".into(),
            formats: vec![
                SubdevFormat {
                    code: MbusCode(0x3007),
                    sizes: sizes.clone(),
                },
                SubdevFormat {
                    code: MbusCode(0x3001),
                    sizes,
                },
            ],
            native_size: Some(Size::new(1296, 816)),
            crop_bounds: Some(Rect {
                left: 8,
                top: 8,
                width: 1280,
                height: 800,
            }),
            current_size: Some(Size::new(1280, 800)),
            controls: BTreeMap::from([
                (KernelControl::Exposure, range(1, 3638, 642, None)),
                (KernelControl::AnalogueGain, range(16, 255, 16, None)),
                (KernelControl::Vblank, range(110, 51540, 1022, Some(2850))),
                (KernelControl::Hblank, range(176, 31487, 176, None)),
                (
                    KernelControl::PixelRate,
                    range(160_000_000, 160_000_000, 160_000_000, None),
                ),
                (KernelControl::LinkFreq, range(0, 0, 0, None)),
            ]),
            link_frequencies: vec![400_000_000],
            analogue_gain_unity: None,
        }
    }

    #[test]
    fn description_from_the_devices_report() {
        let d = SensorDescription::from_subdev(&helios_ov9782()).unwrap();
        assert_eq!(d.sensor.name, "ov9782");
        assert_eq!(d.sensor.backend, Backend::Kernel);
        assert_eq!(d.pixel_array.color_filter, crate::ColorFilter::Bggr);
        assert_eq!(d.formats.keys().collect::<Vec<_>>(), ["raw10", "raw8"]);
        assert_eq!(d.modes.len(), 1);
        assert_eq!(d.controls.exposure.margin, 12);
        assert_eq!(d.formats["raw10"].link_frequency, Some(400_000_000));
        let t = d.timing("1280x800", "raw10").unwrap();
        let (lo, hi) = t.fps_range();
        assert!(
            (hi - 120.76).abs() < 0.01 && (lo - 2.0996).abs() < 0.001,
            "{lo} {hi}"
        );
        let g = &d.controls.analog_gain;
        assert_eq!(g.gain_for_code(16), 1.0);
        assert_eq!(g.gain_for_code(255), 255.0 / 16.0);
    }

    #[test]
    fn missing_controls_are_reported() {
        let mut r = helios_ov9782();
        r.controls.remove(&KernelControl::PixelRate);
        let e = SensorDescription::from_subdev(&r).unwrap_err().to_string();
        assert!(e.contains("PIXEL_RATE"), "{e}");
    }

    #[test]
    fn scheduled_codes_become_kernel_controls() {
        let codes = ControlSet::new()
            .with(Control::FrameLength, 1822)
            .with(Control::Exposure, 642 << 4)
            .with(Control::AnalogGain, 32);
        let k = kernel_controls(&codes, 800, 4);
        assert_eq!(
            k,
            vec![
                (KernelControl::Vblank, 1022),
                (KernelControl::Exposure, 642),
                (KernelControl::AnalogueGain, 32)
            ]
        );
        assert_eq!(
            KernelControl::from_cid(0x009e0901),
            Some(KernelControl::Vblank)
        );
    }
}
