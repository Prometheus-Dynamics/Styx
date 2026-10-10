//! Generic defaults for sensors without a description: build one from what the kernel driver
//! reports, with controls going through V4L2 controls instead of registers.

use alloc::{borrow::ToOwned, string::ToString};
use alloc::{format, string::String, vec, vec::Vec};

use alloc::collections::{BTreeMap, BTreeSet};

use crate::desc::{
    Backend, Blanking, Controls, Exposure, Field, Flip, Format, Gain, GainModel, Identity, Mode,
    PixelArray, Rect, SensorDescription, Sequences, Size, TestPattern,
};
use crate::error::{Issue, Issues, Result, SensorError};
use crate::kernel_data::KernelSensorData;
use crate::mbus::MbusCode;
use crate::schedule::{Control, ControlSet};

/// V4L2 control values for one `VIDIOC_S_EXT_CTRLS` call (at most the four scheduled
/// controls, so no allocation).
pub type KernelControls = crate::fixed::FixedVec<(KernelControl, i64), 4>;

/// Sensor-related V4L2 controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum KernelControl {
    /// `V4L2_CID_EXPOSURE` (lines).
    #[default]
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
    /// `V4L2_CID_HFLIP`.
    HFlip,
    /// `V4L2_CID_VFLIP`.
    VFlip,
    /// `V4L2_CID_TEST_PATTERN` (menu index).
    TestPattern,
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
            KernelControl::HFlip => 0x0098_0914,
            KernelControl::VFlip => 0x0098_0915,
            KernelControl::TestPattern => 0x009f_0903,
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
            HFlip,
            VFlip,
            TestPattern,
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
    /// gain model; without a data file a linear model with this unity code is assumed.
    pub analogue_gain_unity: Option<i64>,
    /// `HFLIP` / `VFLIP` change the Bayer order of the codes (`V4L2_CTRL_FLAG_MODIFY_LAYOUT`).
    /// The codes above are those of the flips' current values (in `controls`).
    pub flips_modify_layout: bool,
    /// `TEST_PATTERN` menu items: index and name.
    pub test_patterns: Vec<(u32, String)>,
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

/// A test pattern menu item's name as a description names it: lower case, words joined by
/// `_`; the first item (the driver's "Disabled") is `off`.
fn pattern_name(index: u32, first: u32, name: &str) -> String {
    if index == first {
        return "off".into();
    }
    let words: Vec<String> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    words.join("_")
}

impl SensorDescription {
    /// A description for a sensor with a kernel driver, from what its subdevice reports, with
    /// generic defaults for what it does not (see [`Self::from_subdev_with`]).
    pub fn from_subdev(report: &SubdevReport) -> Result<Self> {
        Self::from_subdev_with(report, None)
    }

    /// A description for a sensor with a kernel driver, from what its subdevice reports and
    /// what `data` adds.
    ///
    /// Requires `PIXEL_RATE`, `EXPOSURE` and `ANALOGUE_GAIN`. Blanking ranges are those
    /// reported at `current_size` and are used for every size, except a mode the data file
    /// gives its own blanking ([`KernelSensorData::modes`]): the kernel adjusts the ranges per
    /// mode, and they are only read at one size here. Codes and the colour filter are given
    /// with the flips off (the report's are turned back when the flips change the layout).
    /// Without data, gain is assumed linear with `analogue_gain_unity` as 1× and delays are
    /// libcamera's defaults for unknown sensors; register fields are absent (the controls are
    /// the driver's V4L2 controls; flips and the test pattern carry placeholder registers).
    pub fn from_subdev_with(
        report: &SubdevReport,
        data: Option<&KernelSensorData>,
    ) -> Result<Self> {
        let ctl = |c| report.controls.get(&c);
        let pixel_rate = ctl(KernelControl::PixelRate)
            .ok_or_else(|| issue("controls", "PIXEL_RATE is required"))?;
        let exposure = ctl(KernelControl::Exposure)
            .ok_or_else(|| issue("controls", "EXPOSURE is required"))?;
        let again = ctl(KernelControl::AnalogueGain)
            .ok_or_else(|| issue("controls", "ANALOGUE_GAIN is required"))?;
        // The codes with the flips off.
        let on = |c| ctl(c).is_some_and(|r| r.value.unwrap_or(r.default) != 0);
        let (hf, vf) = if report.flips_modify_layout {
            (on(KernelControl::HFlip), on(KernelControl::VFlip))
        } else {
            (false, false)
        };
        let unflipped: Vec<SubdevFormat> = report
            .formats
            .iter()
            .map(|f| SubdevFormat {
                code: f.code.flipped(hf, vf),
                sizes: f.sizes.clone(),
            })
            .collect();
        let first = unflipped
            .first()
            .ok_or_else(|| issue("formats", "no media bus codes"))?;
        let color_filter = first
            .code
            .color_filter()
            .ok_or_else(|| issue("formats", &format!("unknown media bus code {}", first.code)))?;

        let largest = unflipped
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
        for f in &unflipped {
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
                    embedded_data: true,
                },
            );
        }

        let sizes: BTreeSet<(u32, u32)> = unflipped
            .iter()
            .flat_map(|f| &f.sizes)
            .map(|s| (s.width, s.height))
            .collect();
        let mut modes = Vec::new();
        for (w, h) in sizes.into_iter().rev() {
            let supported: Vec<String> = unflipped
                .iter()
                .filter(|f| f.sizes.contains(&Size::new(w, h)))
                .map(|f| names[&f.code].clone())
                .collect();
            // The reported ranges are those of the mode the sensor is set to; a mode the data
            // file gives its own blanking takes that.
            let name = format!("{w}x{h}");
            let (mode_hblank, mode_vblank) =
                match data.and_then(|d| d.modes.iter().find(|m| m.mode == name)) {
                    Some(m) => (m.hblank, m.vblank),
                    None => (hblank, vblank),
                };
            modes.push(Mode {
                name,
                size: Size::new(w, h),
                crop: active,
                formats: (supported.len() != formats.len()).then_some(supported),
                binning: [1, 1],
                skipping: [1, 1],
                hblank: mode_hblank,
                vblank: mode_vblank,
                pixel_rate: None,
                registers: Vec::new(),
            });
        }

        let unity = report.analogue_gain_unity.unwrap_or(again.min.max(1));
        let with_model = |r: &ControlRange, unity: i64, model: Option<&GainModel>| {
            let mut g = linear_gain(r, unity);
            if let Some(m) = model {
                g.model = m.clone();
            }
            g
        };
        let flip = |c| {
            ctl(c).map(|_| Flip {
                address: 0,
                mask: 1,
                default: false,
                changes_bayer_order: report.flips_modify_layout,
            })
        };
        let test_pattern = ctl(KernelControl::TestPattern)
            .filter(|_| !report.test_patterns.is_empty())
            .map(|r| {
                let first = report.test_patterns[0].0.max(to_u32(r.min));
                TestPattern {
                    register: Field::whole(0, 4),
                    patterns: report
                        .test_patterns
                        .iter()
                        .map(|(i, n)| (pattern_name(*i, first, n), *i))
                        .collect(),
                }
            })
            .filter(|t| t.patterns.contains_key("off"));
        let name = data.map_or_else(
            || {
                report
                    .name
                    .split_whitespace()
                    .next()
                    .unwrap_or("unknown")
                    .to_owned()
            },
            |d| d.name.clone(),
        );
        let margin = data
            .and_then(|d| d.exposure_margin)
            .unwrap_or(to_u32(frame_length - exposure_max));
        let desc = SensorDescription {
            sensor: Identity {
                name,
                vendor: data.and_then(|d| d.vendor.clone()),
                backend: Backend::Kernel,
                i2c_address: None,
                address_bits: 16,
                burst_writes: false,
                chip_id: None,
                clocks: BTreeMap::new(),
                tuning: data.and_then(|d| d.tuning.clone()),
            },
            pixel_array: PixelArray {
                size,
                active,
                color_filter,
                black_level: data.and_then(|d| d.black_level),
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
                    margin,
                    step: to_u32(i64::try_from(exposure.step).unwrap_or(1).max(1)),
                    default: to_u32(exposure.default.max(exposure.min).max(1)),
                },
                analog_gain: with_model(again, unity, data.and_then(|d| d.analog_gain.as_ref())),
                digital_gain: ctl(KernelControl::DigitalGain)
                    .map(|d| with_model(d, d.default, data.and_then(|x| x.digital_gain.as_ref()))),
                delays: data.and_then(|d| d.delays).unwrap_or_default(),
                group_hold: None,
                frame_length_extra_lines: data.map_or(0, |d| d.frame_length_extra_lines),
                hflip: flip(KernelControl::HFlip),
                vflip: flip(KernelControl::VFlip),
                test_pattern,
            },
            embedded_data: data.and_then(|d| d.embedded_data.clone()),
            lens: data.and_then(|d| d.lens.clone()),
            bus: None,
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
) -> KernelControls {
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
            ..Default::default()
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
    fn data_files_and_flips_complete_the_description() {
        let mut r = helios_ov9782();
        // The driver's default flips (both on) turn BGGR into RGGB and change the layout.
        r.formats[0].code = MbusCode(0x300f);
        r.formats[1].code = MbusCode(0x3014);
        r.flips_modify_layout = true;
        r.controls
            .insert(KernelControl::HFlip, range(0, 1, 1, Some(1)));
        r.controls
            .insert(KernelControl::VFlip, range(0, 1, 1, Some(1)));
        r.controls
            .insert(KernelControl::TestPattern, range(0, 2, 0, Some(0)));
        r.test_patterns = vec![(0, "Disabled".into()), (2, "Color Bars".into())];
        let data = KernelSensorData::find(&KernelSensorData::builtin(), &r.name, true)
            .unwrap()
            .clone();
        let d = SensorDescription::from_subdev_with(&r, Some(&data)).unwrap();
        assert_eq!(d.pixel_array.color_filter, crate::ColorFilter::Bggr);
        assert_eq!(d.formats["raw10"].code, MbusCode(0x3007));
        assert_eq!(d.formats["raw8"].code, MbusCode(0x3001));
        assert!(d.controls.hflip.unwrap().changes_bayer_order);
        let tp = d.controls.test_pattern.as_ref().unwrap();
        assert_eq!(tp.patterns["off"], 0);
        assert_eq!(tp.patterns["color_bars"], 2);
        assert_eq!(d.controls.delays, data.delays.unwrap());
        assert_eq!(d.controls.frame_length_extra_lines, 1);
        assert_eq!(d.sensor.tuning.as_deref(), Some("ov9782.json"));
        assert_eq!(d.pixel_array.black_level.unwrap().value, 64);
        assert_eq!(d.controls.analog_gain.gain_for_code(32), 2.0);
    }

    #[test]
    fn kernel_driven_sensors_run_over_v4l2_controls() {
        use std::sync::Arc;

        use crate::{ControlRequest, MockBus, NoPins, SensorDriver};

        let mut r = helios_ov9782();
        r.controls
            .insert(KernelControl::HFlip, range(0, 1, 1, Some(1)));
        let d = Arc::new(SensorDescription::from_subdev(&r).unwrap());
        let mut drv = SensorDriver::new(d, MockBus::new(), NoPins);
        drv.power_up().unwrap();
        assert_eq!(drv.verify_chip_id().unwrap(), 0);
        drv.init().unwrap();
        drv.set_mode("1280x800", "raw10").unwrap();
        let bus = drv.bus();
        // The mode's defaults: HBLANK, then VBLANK, each alone, then exposure and gain; flips off.
        assert_eq!(bus.control_log[0], vec![(KernelControl::Hblank, 176)]);
        assert_eq!(bus.control_log[1], vec![(KernelControl::Vblank, 2850)]);
        assert_eq!(
            bus.control_log[2],
            vec![
                (KernelControl::Exposure, 642),
                (KernelControl::AnalogueGain, 16)
            ]
        );
        assert_eq!(bus.control(KernelControl::HFlip), Some(0));
        assert!(bus.writes().is_empty(), "no registers");
        // A request for frame 0 is written at the start, before the stream.
        drv.request(
            0,
            &ControlRequest {
                exposure: Some(core::time::Duration::from_millis(5)),
                gain: Some(2.0),
                frame_duration: Some(core::time::Duration::from_secs_f64(1.0 / 30.0)),
            },
        )
        .unwrap();
        drv.start_streaming().unwrap();
        let bus = drv.bus();
        let fl = (160e6_f64 / 30.0 / 1456.0).round() as i64;
        assert_eq!(bus.control(KernelControl::Vblank), Some(fl - 800));
        assert_eq!(bus.control(KernelControl::AnalogueGain), Some(32));
        let lines = (0.005_f64 * 160e6 / 1456.0).round() as i64;
        assert_eq!(bus.control(KernelControl::Exposure), Some(lines));
        let a = drv.applied(0).unwrap();
        assert!((a.analog_gain - 2.0).abs() < 1e-9 && a.frame_length as i64 == fl);
        drv.set_test_pattern("off").unwrap_err();
        drv.stop_streaming().unwrap();
        assert!(
            drv.set_description(Arc::new(SensorDescription::from_subdev(&r).unwrap()))
                .is_ok()
        );
        assert!(drv.mode().is_none());
    }

    #[test]
    fn ccs_embedded_data_reads_back_kernel_controls() {
        let mut data = KernelSensorData::builtin()
            .into_iter()
            .find(|d| d.name == "imx219")
            .unwrap();
        data.embedded_data.as_mut().unwrap().packing = crate::EmbeddedPacking::None;
        let d = SensorDescription::from_subdev_with(&helios_ov9782(), Some(&data)).unwrap();
        // 0x0157 = 0x80 (gain), 0x015a..b = 0x0400 (exposure), 0x0160..1 = 0x0d78 (VTS).
        let line = [
            0x0a, 0xaa, 0x01, 0xa5, 0x57, 0x5a, 0x80, 0x55, 0, 0x55, 0, 0x5a, 0x04, 0x5a, 0x00,
            0xa5, 0x60, 0x5a, 0x0d, 0x5a, 0x78, 0x07, 0x07,
        ];
        assert_eq!(
            d.decode_embedded(&line),
            crate::ControlSet::new()
                .with(Control::AnalogGain, 0x80)
                .with(Control::Exposure, 0x400)
                .with(Control::FrameLength, 0x0d78)
        );
    }

    #[test]
    fn missing_controls_are_reported() {
        let mut r = helios_ov9782();
        r.controls.remove(&KernelControl::PixelRate);
        let e = SensorDescription::from_subdev(&r).unwrap_err().to_string();
        assert!(e.contains("PIXEL_RATE"), "{e}");
    }

    /// The OV9782 report with the three modes' sizes (`ov9282.c`'s modes; the driver reports the
    /// blanking ranges of the mode it is set to, here 1280x800).
    fn helios_ov9782_all_modes() -> SubdevReport {
        let mut r = helios_ov9782();
        for f in &mut r.formats {
            f.sizes = vec![
                Size::new(1280, 800),
                Size::new(1280, 720),
                Size::new(640, 400),
            ];
        }
        r
    }

    fn ov9782_kernel_description() -> SensorDescription {
        let r = helios_ov9782_all_modes();
        let data = KernelSensorData::find(&KernelSensorData::builtin(), &r.name, true)
            .unwrap()
            .clone();
        SensorDescription::from_subdev_with(&r, Some(&data)).unwrap()
    }

    /// Every mode runs at the frame rate asked for, whatever mode the sensor was in before: the
    /// driver keeps the HBLANK it was last given when the format changes (its range update keeps
    /// an in-range value), so the sensor's line length is the one Styx writes, not the last one.
    #[test]
    fn requested_frame_rates_hold_at_every_mode() {
        use crate::{ControlRequest, MockBus, NoPins, SensorDriver};
        use core::time::Duration;

        let mut drv = SensorDriver::new(
            std::sync::Arc::new(ov9782_kernel_description()),
            MockBus::new(),
            NoPins,
        );
        drv.power_up().unwrap();
        drv.init().unwrap();
        // Mode changes in both directions: 640x400 leaves HBLANK at 816 for 1280x800.
        for (mode, w, h, fps) in [
            ("1280x800", 1280u32, 800u32, 30.0),
            ("1280x800", 1280, 800, 60.0),
            ("1280x800", 1280, 800, 120.0),
            ("640x400", 640, 400, 60.0),
            ("640x400", 640, 400, 120.0),
            ("640x400", 640, 400, 240.0),
            ("1280x800", 1280, 800, 30.0),
        ] {
            drv.set_mode(mode, "raw10").unwrap();
            drv.request(
                0,
                &ControlRequest {
                    frame_duration: Some(Duration::from_secs_f64(1.0 / fps)),
                    ..Default::default()
                },
            )
            .unwrap();
            drv.start_streaming().unwrap();
            let bus = drv.bus();
            let hblank = bus
                .control(KernelControl::Hblank)
                .expect("HBLANK is written with the mode") as f64;
            let vblank = bus.control(KernelControl::Vblank).unwrap() as f64;
            // One line beyond VTS, as the sensor runs it (measured; the driver's formula omits it).
            let duration = (f64::from(w) + hblank) * (f64::from(h) + vblank + 1.0) / 160e6;
            assert!(
                (duration * fps - 1.0).abs() < 1e-3,
                "{mode} at {fps} fps runs at {:.3} ms ({:.3} fps)",
                duration * 1e3,
                1.0 / duration
            );
            drv.stop_streaming().unwrap();
        }
    }

    /// The driver's EXPOSURE maximum is its frame length minus the guard band (`ov9282.c`:
    /// `vblank + height - exposure_offset`, 25 lines for the OV9782). An exposure past it on the
    /// 640x400 mode, where the frame is 457 lines, gives black frames on the device.
    #[test]
    fn exposure_stays_inside_the_frame_at_every_mode() {
        use crate::{ControlRequest, MockBus, NoPins, SensorDriver};
        use core::time::Duration;

        let d = ov9782_kernel_description();
        assert_eq!(
            d.controls.exposure.margin, 25,
            "the driver's exposure offset"
        );
        let mut drv = SensorDriver::new(std::sync::Arc::new(d), MockBus::new(), NoPins);
        drv.power_up().unwrap();
        drv.init().unwrap();
        drv.set_mode("640x400", "raw10").unwrap();
        drv.request(
            0,
            &ControlRequest {
                exposure: Some(Duration::from_millis(10)),
                frame_duration: Some(Duration::from_secs_f64(1.0 / 240.0)),
                ..Default::default()
            },
        )
        .unwrap();
        drv.start_streaming().unwrap();
        let a = drv.applied(0).unwrap();
        assert_eq!(a.frame_length, 457, "VTS at 240 fps");
        assert!(
            a.exposure_lines <= 432.0 + 1e-9,
            "exposure {} lines in a {}-line frame",
            a.exposure_lines,
            a.frame_length
        );
        let bus = drv.bus();
        let vblank = bus.control(KernelControl::Vblank).unwrap();
        let exposure = bus.control(KernelControl::Exposure).unwrap();
        assert!(
            exposure <= 400 + vblank - 25,
            "EXPOSURE {exposure} with VBLANK {vblank}"
        );
        drv.stop_streaming().unwrap();
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
