//! Stills: a raw frame held out of the stream with what processed it ([`HeldRaw`]),
//! reprocessed at full quality by the software ISP ([`soft_still`]; the PiSP's is
//! `device::StillBackEnd`), and written as a DNG ([`dng_metadata`], with the colour
//! calibration of [`dng_calibrations`] from the tuning).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;
#[cfg(not(feature = "std"))]
use styx_core::math::Float as _;

use styx_algo::{Params, Tuning};
use styx_dng::color::{self, Matrix3};
use styx_dng::{ColorCalibration, DngMetadata, Illuminant, Preview, RawImage, SampleLayout};
use styx_softisp::{
    Arithmetic, CfaPattern, Demosaic, OutputBuffers, RawFormat, RawPacking, Scale, SoftIsp,
    YuvMatrix,
};

use crate::controller::SensorValues;
use crate::error::{PipelineError, Result};
use crate::isp::IspSettings;

/// A raw frame kept for a still, with what produced and processed it.
#[derive(Clone, Debug)]
pub struct HeldRaw {
    /// Frame sequence.
    pub sequence: u64,
    /// Capture timestamp (`CLOCK_MONOTONIC`).
    pub timestamp: Duration,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Bytes per row in `data`.
    pub stride: usize,
    /// How `data` stores samples (the PiSP front end's output: 16-bit, the sensor's value
    /// shifted to the top; the receiver's: the sensor's packing).
    pub packing: RawPacking,
    /// Colour filter.
    pub cfa: CfaPattern,
    /// The sensor's bits per sample.
    pub bits: u8,
    /// The rows.
    pub data: Vec<u8>,
    /// What produced the frame.
    pub sensor: SensorValues,
    /// The ISP settings the stream processed it with.
    pub isp: IspSettings,
    /// The algorithms' state when it was processed (colour temperature, lux, gains, black
    /// levels, the lens shading table without deflicker's bands).
    pub params: Box<Params>,
}

fn dng_cfa(c: CfaPattern) -> styx_dng::CfaPattern {
    match c {
        CfaPattern::Rggb => styx_dng::CfaPattern::Rggb,
        CfaPattern::Bggr => styx_dng::CfaPattern::Bggr,
        CfaPattern::Grbg => styx_dng::CfaPattern::Grbg,
        CfaPattern::Gbrg => styx_dng::CfaPattern::Gbrg,
    }
}

impl HeldRaw {
    /// The raw format for the software ISP.
    pub fn format(&self) -> RawFormat {
        RawFormat::new(self.width, self.height, self.cfa, self.packing)
    }

    /// The samples at the sensor's bit depth (16-bit front end output shifted back down).
    pub fn raw_image(&self) -> Result<RawImage> {
        let packing = match self.packing {
            RawPacking::U8 => styx_dng::Packing::U8,
            RawPacking::U16Le { bits } => styx_dng::Packing::U16Le { bits },
            RawPacking::Csi2Raw10 => styx_dng::Packing::Csi2Raw10,
            RawPacking::Csi2Raw12 => styx_dng::Packing::Csi2Raw12,
        };
        let stored = self.packing.bit_depth();
        let bits = self.bits.clamp(1, stored);
        let mut samples =
            styx_dng::unpack(&self.data, self.width, self.height, self.stride, packing)
                .map_err(dng_error)?;
        if stored > bits {
            let shift = stored - bits;
            for s in &mut samples {
                *s >>= shift;
            }
        }
        RawImage::new(
            self.width,
            self.height,
            SampleLayout::Cfa(dng_cfa(self.cfa)),
            bits,
            samples,
        )
        .map_err(dng_error)
    }
}

fn dng_error(e: styx_dng::DngError) -> PipelineError {
    PipelineError::Config(format!("dng: {e}"))
}

/// What a still is processed into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StillPixels {
    /// 8-bit R, G, B.
    Rgb24,
    /// NV12 (full-range BT.601, as the PiSP's).
    Nv12,
}

impl StillPixels {
    /// Bytes of a `width` x `height` image, tightly packed.
    pub fn bytes(self, width: u32, height: u32) -> usize {
        let px = width as usize * height as usize;
        match self {
            Self::Rgb24 => px * 3,
            Self::Nv12 => px * 3 / 2,
        }
    }
}

/// Processes a held raw frame at full size with the software ISP at its best: the
/// Malvar-He-Cutler demosaic and `settings` (e.g. the frame's own, see [`HeldRaw::isp`]).
/// Returns the image tightly packed.
pub fn soft_still(
    raw: &HeldRaw,
    settings: &IspSettings,
    pixels: StillPixels,
    threads: usize,
) -> Result<Vec<u8>> {
    soft_still_with(raw, settings, pixels, threads, Arithmetic::Auto)
}

/// [`soft_still`] with the software ISP's per-pixel `arithmetic` (`Int`, the reference, gives
/// the same image on every CPU and build).
pub fn soft_still_with(
    raw: &HeldRaw,
    settings: &IspSettings,
    pixels: StillPixels,
    threads: usize,
    arithmetic: Arithmetic,
) -> Result<Vec<u8>> {
    let format = raw.format();
    let base = styx_softisp::IspParams {
        arithmetic,
        demosaic: Demosaic::Mhc,
        yuv: YuvMatrix::Bt601Full,
        stats: None,
        ..Default::default()
    };
    let params = settings.softisp(raw.packing.bit_depth(), &base);
    let isp = SoftIsp::new(format, params)
        .map_err(|e| PipelineError::Config(format!("software ISP: {e}")))?;
    let mut isp = crate::engine::with_threads(isp, threads.max(1));
    let (w, h) = (raw.width as usize, raw.height as usize);
    let mut out = vec![0u8; pixels.bytes(raw.width, raw.height)];
    let buffers = match pixels {
        StillPixels::Rgb24 => OutputBuffers::Rgb24 {
            data: &mut out,
            stride: w * 3,
        },
        StillPixels::Nv12 => {
            let (y, uv) = out.split_at_mut(w * h);
            OutputBuffers::Nv12 {
                y,
                y_stride: w,
                uv,
                uv_stride: w,
            }
        }
    };
    isp.process(&raw.data, raw.stride, Scale::Full, buffers)
        .map_err(|e| PipelineError::Config(format!("software ISP: {e}")))?;
    Ok(out)
}

/// The camera's neutral (R/G, 1, B/G of a grey) at colour temperature `ct` from the AWB
/// tuning's CT curve (linear between points, clamped at the ends).
pub fn neutral_at(tuning: &Tuning, ct: f64) -> Option<[f64; 3]> {
    let mut curve = tuning.awb.as_ref()?.ct_curve.clone();
    if curve.is_empty() {
        return None;
    }
    curve.sort_by(|a, b| a[0].total_cmp(&b[0]));
    let (first, last) = (curve[0], curve[curve.len() - 1]);
    let p = if ct <= first[0] {
        first
    } else if ct >= last[0] {
        last
    } else {
        let i = curve.iter().position(|p| p[0] >= ct).unwrap_or(1).max(1);
        let (a, b) = (curve[i - 1], curve[i]);
        let t = (ct - a[0]) / (b[0] - a[0]);
        core::array::from_fn(|k| a[k] + (b[k] - a[k]) * t)
    };
    Some([p[1], 1.0, p[2]])
}

/// DNG colour calibration from a tuning: `ColorMatrix` and `ForwardMatrix` at standard
/// illuminant A (2856 K) and D65 (6504 K), each from the tuning's CCM at that temperature and
/// the camera's neutral there from the AWB curve (see `styx_dng` for the derivation). Empty
/// when the tuning has no AWB curve (grey world): then see [`shot_calibration`].
pub fn dng_calibrations(tuning: &Tuning) -> Vec<ColorCalibration> {
    let ccms = tuning.ccm.clone().unwrap_or_default();
    [Illuminant::StandardA, Illuminant::D65]
        .into_iter()
        .filter_map(|ill| {
            let neutral = neutral_at(tuning, ill.cct())?;
            let ccm = ccms.matrix_for(ill.cct());
            Some(ColorCalibration {
                illuminant: ill,
                color_matrix: color::color_matrix(&ccm, neutral, ill.xy())?,
                forward_matrix: Some(color::forward_matrix(&ccm)),
            })
        })
        .collect()
}

/// One calibration from the shot itself (for tunings without an AWB curve): the CCM it was
/// processed with and its neutral, at the D65 slot.
pub fn shot_calibration(ccm: &Matrix3, neutral: [f64; 3]) -> Vec<ColorCalibration> {
    color::color_matrix(ccm, neutral, Illuminant::D65.xy())
        .map(|cm| ColorCalibration {
            illuminant: Illuminant::D65,
            color_matrix: cm,
            forward_matrix: Some(color::forward_matrix(ccm)),
        })
        .into_iter()
        .collect()
}

/// Who and what took a still, for its DNG.
#[derive(Clone, Debug, Default)]
pub struct StillSource {
    /// Camera model (the sensor's name).
    pub model: String,
    /// Unique camera model (names the calibration: sensor and tuning).
    pub unique_camera_model: String,
    /// Calibrations ([`dng_calibrations`]); empty: one from the shot.
    pub calibrations: Vec<ColorCalibration>,
}

/// The DNG metadata of a held raw frame: black levels per CFA cell and the white level at the
/// sensor's bit depth, the as-shot neutral from the white balance it was processed with,
/// the calibrations, the ISP's digital gain as baseline exposure, exposure time, ISO (100 ×
/// the sensor's gain), lens shading as `GainMap`s (with `lens_shading`), a JSON description
/// of the frame (sequence, timestamp, gains, colour temperature, lux) and the preview.
#[cfg(feature = "std")]
pub fn dng_metadata(
    raw: &HeldRaw,
    source: &StillSource,
    lens_shading: bool,
    captured: std::time::SystemTime,
    preview: Option<Preview>,
) -> DngMetadata {
    let at = captured
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok();
    dng_metadata_at(raw, source, lens_shading, at, preview)
}

/// [`dng_metadata`] with the capture time as a duration since the Unix epoch (`None`: not
/// known), for builds without `std`.
pub fn dng_metadata_at(
    raw: &HeldRaw,
    source: &StillSource,
    lens_shading: bool,
    captured: Option<Duration>,
    preview: Option<Preview>,
) -> DngMetadata {
    let full = f64::from(1u32 << raw.bits);
    let bl = raw.params.black_level;
    let cfa = dng_cfa(raw.cfa);
    let black_level = cfa.colors().map(|c| {
        let v = match c {
            0 => bl.r,
            2 => bl.b,
            _ => bl.g,
        };
        // Tunings without one: the level the ISP subtracted.
        let v = if v > 0.0 { v } else { raw.isp.black_level };
        (v * full * 1000.0).round() / 1000.0
    });
    let wb = raw.isp.wb;
    let neutral = [1.0 / wb[0].max(1e-6), 1.0, 1.0 / wb[2].max(1e-6)];
    let calibrations = if source.calibrations.is_empty() {
        shot_calibration(&raw.isp.ccm, neutral)
    } else {
        source.calibrations.clone()
    };
    let opcode_list2 = match (&raw.params.lens_shading, lens_shading) {
        (Some(ls), true) => styx_dng::opcode::bayer_gain_maps(
            cfa,
            (raw.width, raw.height),
            (ls.width, ls.height),
            [&ls.r, &ls.g, &ls.b],
        ),
        _ => Vec::new(),
    };
    let s = &raw.sensor;
    let p = &raw.params;
    let description = format!(
        "{{\"sequence\":{},\"timestamp_ns\":{},\"exposure_us\":{:.1},\"analogue_gain\":{:.4},\
         \"sensor_digital_gain\":{:.4},\"isp_digital_gain\":{:.4},\"colour_gains\":[{:.4},{:.4},{:.4}],\
         \"colour_temperature\":{:.0},\"lux\":{:.1},\"verified\":{}}}",
        raw.sequence,
        raw.timestamp.as_nanos(),
        s.exposure.as_secs_f64() * 1e6,
        s.analogue_gain,
        s.digital_gain,
        raw.isp.digital_gain,
        p.colour_gains[0],
        p.colour_gains[1],
        p.colour_gains[2],
        p.colour_temperature,
        p.lux,
        s.verified,
    );
    DngMetadata {
        black_level,
        white_level: Some((1u32 << raw.bits) - 1),
        as_shot_neutral: Some(neutral),
        calibrations,
        baseline_exposure: Some(raw.isp.digital_gain.max(1e-6).log2()),
        exposure_time: Some(s.exposure),
        iso: Some((100.0 * s.analogue_gain * s.digital_gain).round() as u32),
        capture_time: captured,
        model: source.model.clone(),
        unique_camera_model: source.unique_camera_model.clone(),
        software: concat!("Styx ", env!("CARGO_PKG_VERSION")).into(),
        description: Some(description),
        opcode_list2,
        preview,
        ..DngMetadata::default()
    }
}

#[cfg(test)]
mod tests {
    use styx_algo::tuning::{AwbTuning, CcmTuning, CtCcm};
    use styx_algo::{BlackLevels, LensShading};

    use super::*;

    const CCM: Matrix3 = [
        1.80439, -0.73699, -0.06739, -0.36073, 1.83327, -0.47255, -0.08378, -0.56403, 1.64781,
    ];

    fn tuning() -> Tuning {
        Tuning {
            awb: Some(AwbTuning {
                ct_curve: vec![[2500.0, 0.95, 0.4], [6500.0, 0.5, 0.8], [8000.0, 0.45, 0.9]],
                ..AwbTuning::default()
            }),
            ccm: Some(CcmTuning {
                ccms: vec![
                    CtCcm {
                        ct: 2800.0,
                        ccm: CCM,
                    },
                    CtCcm {
                        ct: 6000.0,
                        ccm: styx_algo::IDENTITY,
                    },
                ],
                saturation: None,
            }),
            ..Tuning::default()
        }
    }

    fn held() -> HeldRaw {
        // A 16-bit front end frame of a 10-bit sensor: value 100 << 6 everywhere.
        let (w, h) = (8u32, 4u32);
        let data = (0..w * h)
            .flat_map(|_| (100u16 << 6).to_le_bytes())
            .collect();
        let mut params = Params {
            black_level: BlackLevels {
                r: 64.0 / 1024.0,
                g: 64.0 / 1024.0,
                b: 65.0 / 1024.0,
            },
            colour_temperature: 4100.0,
            ..Params::default()
        };
        params.lens_shading = Some(LensShading {
            width: 2,
            height: 2,
            r: vec![1.5; 4],
            g: vec![1.0; 4],
            b: vec![1.2; 4],
        });
        let mut isp = IspSettings::neutral(64.0 / 1024.0);
        isp.wb = [2.0, 1.0, 1.25];
        isp.digital_gain = 2.0;
        HeldRaw {
            sequence: 42,
            timestamp: Duration::from_millis(5),
            width: w,
            height: h,
            stride: w as usize * 2,
            packing: RawPacking::U16Le { bits: 16 },
            cfa: CfaPattern::Bggr,
            bits: 10,
            data,
            sensor: SensorValues {
                frame: 42,
                exposure: Duration::from_millis(10),
                analogue_gain: 2.5,
                digital_gain: 1.0,
                frame_duration: Duration::from_millis(33),
                verified: true,
            },
            isp,
            params: Box::new(params),
        }
    }

    #[test]
    fn neutrals_follow_the_awb_curve() {
        let t = tuning();
        assert_eq!(neutral_at(&t, 1000.0), Some([0.95, 1.0, 0.4]));
        let n = neutral_at(&t, 4500.0).unwrap();
        assert!((n[0] - 0.725).abs() < 1e-12 && (n[2] - 0.6).abs() < 1e-12);
        assert!(neutral_at(&Tuning::default(), 5000.0).is_none());
        let cal = dng_calibrations(&t);
        assert_eq!(cal.len(), 2);
        assert_eq!(cal[0].illuminant, Illuminant::StandardA);
        // The A matrix maps A's white to the curve's neutral at 2856 K (largest 1).
        let cam = color::mul_vec(
            &cal[0].color_matrix,
            color::xy_to_xyz(Illuminant::StandardA.xy()),
        );
        let n = neutral_at(&t, 2856.0).unwrap();
        assert!(
            cam.iter().zip(n).all(|(a, b)| (a - b).abs() < 1e-3),
            "{cam:?}"
        );
        assert!(dng_calibrations(&Tuning::default()).is_empty());
    }

    #[test]
    fn a_held_frame_becomes_a_dng() {
        let raw = held();
        let img = raw.raw_image().unwrap();
        assert_eq!(img.bits, 10);
        assert!(img.samples.iter().all(|&v| v == 100));
        let meta = dng_metadata(
            &raw,
            &StillSource {
                model: "ov9782".into(),
                ..Default::default()
            },
            true,
            std::time::SystemTime::UNIX_EPOCH,
            None,
        );
        // BGGR: blue first.
        assert_eq!(meta.black_level, [65.0, 64.0, 64.0, 64.0]);
        assert_eq!(meta.white_level, Some(1023));
        assert_eq!(meta.as_shot_neutral, Some([0.5, 1.0, 0.8]));
        assert_eq!(meta.baseline_exposure, Some(1.0));
        assert_eq!(meta.iso, Some(250));
        assert_eq!(meta.opcode_list2.len(), 4);
        assert_eq!(meta.calibrations.len(), 1);
        assert!(meta.description.unwrap().contains("\"sequence\":42"));
        let bytes = styx_dng::write_dng(
            &img,
            &dng_metadata(
                &raw,
                &StillSource::default(),
                false,
                std::time::SystemTime::UNIX_EPOCH,
                None,
            ),
        )
        .unwrap();
        let back = styx_dng::read_dng(&bytes).unwrap();
        assert_eq!(back.samples, img.samples);
        assert!(back.gain_maps().is_empty());
    }

    #[test]
    fn soft_stills_come_out_full_size() {
        let raw = held();
        for (px, len) in [
            (StillPixels::Rgb24, 8 * 4 * 3),
            (StillPixels::Nv12, 8 * 4 * 3 / 2),
        ] {
            let out = soft_still(&raw, &raw.isp, px, 1).unwrap();
            assert_eq!(out.len(), len);
            assert!(out.iter().any(|&v| v > 0));
        }
    }
}
