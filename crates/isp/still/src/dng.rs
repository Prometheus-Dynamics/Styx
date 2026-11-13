//! The raw frame as a DNG file, with the metadata a raw converter and a calibration tool use.

use styx_aaa::Tuning;
use styx_aaa::tuning::CcmTuning;
use styx_controls::FrameMetadata;
use styx_dng::color::{self, Matrix3};
use styx_dng::{
    CfaPattern as DngCfa, ColorCalibration, DngMetadata, Illuminant, Packing, RawImage,
    SampleLayout,
};
use styx_frame::FrameLease;
use styx_soft::{CfaPattern, RawFormat, RawPacking};

use crate::Context;
use crate::error::StillError;

/// The raw frame's samples, unpacked, with the format they came in.
fn raw_image<M>(frame: &FrameLease<M>) -> Result<(RawFormat, RawImage), StillError> {
    let input = frame.format();
    let (width, height) = (input.resolution.width.get(), input.resolution.height.get());
    let raw = RawFormat::from_fourcc(input.code, width, height)
        .ok_or_else(|| StillError::Format(format!("{:?} is not a Bayer raw format", input.code)))?;
    let read = frame.read().ok_or(StillError::NotReadable)?;
    let plane = read.plane(0).ok_or(StillError::NotReadable)?;
    let samples = styx_dng::unpack(
        plane.data,
        width,
        height,
        plane.stride,
        dng_packing(raw.packing),
    )?;
    let bits = raw.packing.bit_depth();
    let image = RawImage::new(
        width,
        height,
        SampleLayout::Cfa(dng_cfa(raw.pattern)),
        bits,
        samples,
    )?;
    Ok((raw, image))
}

fn dng_packing(packing: RawPacking) -> Packing {
    match packing {
        RawPacking::U8 => Packing::U8,
        RawPacking::U16Le { bits } => Packing::U16Le { bits },
        RawPacking::Csi2Raw10 => Packing::Csi2Raw10,
        RawPacking::Csi2Raw12 => Packing::Csi2Raw12,
    }
}

fn dng_cfa(pattern: CfaPattern) -> DngCfa {
    match pattern {
        CfaPattern::Rggb => DngCfa::Rggb,
        CfaPattern::Bggr => DngCfa::Bggr,
        CfaPattern::Grbg => DngCfa::Grbg,
        CfaPattern::Gbrg => DngCfa::Gbrg,
    }
}

/// The camera's neutral (R/G, 1, B/G of a grey) at colour temperature `ct`, from the AWB tuning's
/// curve of `[ct, R/G, B/G]` points (linear between points, clamped at the ends). `None` when the
/// tuning has no curve.
fn neutral_at(tuning: &Tuning, ct: f64) -> Option<[f64; 3]> {
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

/// The DNG colour calibration of a tuning at standard illuminant A (2856 K) and D65 (6504 K):
/// `ColorMatrix` and `ForwardMatrix` from the tuning's CCM at that temperature and the camera's
/// neutral there from the AWB curve (the derivation is in `styx_dng::color`). Empty when the
/// tuning has no AWB curve.
pub fn calibrations(tuning: &Tuning) -> Vec<ColorCalibration> {
    let default = CcmTuning::default();
    let ccms = tuning.ccm.as_ref().unwrap_or(&default);
    [Illuminant::StandardA, Illuminant::D65]
        .into_iter()
        .filter_map(|illuminant| {
            let neutral = neutral_at(tuning, illuminant.cct())?;
            let ccm = ccms.matrix_for(illuminant.cct());
            Some(ColorCalibration {
                illuminant,
                color_matrix: color::color_matrix(&ccm, neutral, illuminant.xy())?,
                forward_matrix: Some(color::forward_matrix(&ccm)),
            })
        })
        .collect()
}

/// One calibration from the shot itself, for a tuning without an AWB curve: the CCM the frame was
/// processed with and its neutral, at the D65 slot.
fn shot_calibration(ccm: &Matrix3, neutral: [f64; 3]) -> Vec<ColorCalibration> {
    color::color_matrix(ccm, neutral, Illuminant::D65.xy())
        .map(|color_matrix| ColorCalibration {
            illuminant: Illuminant::D65,
            color_matrix,
            forward_matrix: Some(color::forward_matrix(ccm)),
        })
        .into_iter()
        .collect()
}

/// The DNG of `frame`, with `meta` and the context's settings.
pub(crate) fn write<M>(
    frame: &FrameLease<M>,
    meta: &FrameMetadata,
    ctx: &Context<'_>,
) -> Result<Vec<u8>, StillError> {
    let (raw, image) = raw_image(frame)?;
    let metadata = metadata(&raw, meta, ctx, image.bits);
    Ok(styx_dng::write_dng(&image, &metadata)?)
}

/// The DNG metadata of a frame processed with `ctx.isp`.
fn metadata(raw: &RawFormat, meta: &FrameMetadata, ctx: &Context<'_>, bits: u8) -> DngMetadata {
    let isp = ctx.isp;
    let full = f64::from(1u32 << bits);
    let cells = isp.black_cfa();
    // DNG lists the 2x2 CFA cells row by row; styx's black levels are red, green on the red row,
    // green on the blue row, blue.
    let black_level = core::array::from_fn(|i| {
        let cell = raw.pattern.cell_at(i % 2, i / 2);
        (cells[cell] * full * 1000.0).round() / 1000.0
    });
    let wb = isp.wb;
    let neutral = [1.0 / wb[0].max(1e-6), 1.0, 1.0 / wb[2].max(1e-6)];
    let mut calibrations = calibrations(ctx.tuning);
    if calibrations.is_empty() {
        calibrations = shot_calibration(&isp.ccm, neutral);
    }
    let opcode_list2 = match &isp.lens_shading {
        Some(ls) => styx_dng::opcode::bayer_gain_maps(
            dng_cfa(raw.pattern),
            (raw.width, raw.height),
            (ls.width, ls.height),
            [&ls.r, &ls.g, &ls.b],
        ),
        None => Vec::new(),
    };
    let analogue = f64::from(meta.analogue_gain_milli) / 1000.0;
    let sensor_digital = f64::from(meta.digital_gain_milli) / 1000.0;
    DngMetadata {
        black_level,
        white_level: Some((1u32 << bits) - 1),
        as_shot_neutral: Some(neutral),
        calibrations,
        baseline_exposure: Some(isp.digital_gain.max(1e-6).log2()),
        exposure_time: Some(std::time::Duration::from_micros(u64::from(
            meta.exposure_us,
        ))),
        iso: Some((100.0 * analogue * sensor_digital).round() as u32),
        capture_time: None,
        model: ctx.model.into(),
        unique_camera_model: ctx.unique_camera_model.into(),
        software: concat!("styx-still ", env!("CARGO_PKG_VERSION")).into(),
        description: Some(description(meta, isp.digital_gain)),
        opcode_list2,
        preview: None,
        ..DngMetadata::default()
    }
}

/// The frame's description: its measured values and controls, as one JSON object.
fn description(meta: &FrameMetadata, isp_digital_gain: f64) -> String {
    let optional = |v: Option<String>| v.unwrap_or_else(|| "null".into());
    format!(
        "{{\"timestamp_ns\":{},\"exposure_us\":{},\"analogue_gain\":{:.4},\
         \"sensor_digital_gain\":{:.4},\"isp_digital_gain\":{:.4},\
         \"colour_temperature\":{},\"lux\":{}}}",
        meta.sensor_timestamp_ns,
        meta.exposure_us,
        f64::from(meta.analogue_gain_milli) / 1000.0,
        f64::from(meta.digital_gain_milli) / 1000.0,
        isp_digital_gain,
        optional(meta.colour_temperature_k.map(|k| k.to_string())),
        optional(meta.lux.map(|l| format!("{l:.1}"))),
    )
}
