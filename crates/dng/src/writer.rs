//! Writing DNG 1.4 files.

use alloc::{format, string::String, vec, vec::Vec};
use core::time::Duration;

use crate::color::{Illuminant, Matrix3};
#[cfg(not(feature = "std"))]
use crate::math::Float as _;
use crate::opcode::{self, Opcode};
use crate::raw::{RawImage, SampleLayout};
use crate::tiff::{Ifd, Value, rational, srational, tag};
use crate::{Result, invalid};

/// Colour calibration under one light.
#[derive(Clone, Debug, PartialEq)]
pub struct ColorCalibration {
    /// The light.
    pub illuminant: Illuminant,
    /// XYZ → camera (see [`crate::color::color_matrix`]).
    pub color_matrix: Matrix3,
    /// White-balanced camera → XYZ D50 (see [`crate::color::forward_matrix`]).
    pub forward_matrix: Option<Matrix3>,
}

/// An sRGB preview (8-bit RGB, row-major, no padding), e.g. a thumbnail of the ISP's output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// `width * height * 3` bytes.
    pub rgb: Vec<u8>,
}

/// What a DNG says about its raw image. Levels are in the image's sample units.
#[derive(Clone, Debug, PartialEq)]
pub struct DngMetadata {
    /// Black level per 2x2 CFA cell position, row-major (all four equal for monochrome;
    /// see [`crate::CfaPattern::colors`] for the colour at each).
    pub black_level: [f64; 4],
    /// White (saturation) level; `None`: the bit depth's maximum.
    pub white_level: Option<u32>,
    /// The camera's neutral for the light of the shot (`1 / white balance gains`, green 1).
    pub as_shot_neutral: Option<[f64; 3]>,
    /// Colour calibration at one or two lights (two in increasing colour temperature).
    pub calibrations: Vec<ColorCalibration>,
    /// Exposure (in stops) to add when rendering: e.g. the ISP's digital gain, so a raw
    /// converter's default rendering is as bright as the ISP's.
    pub baseline_exposure: Option<f64>,
    /// Exposure time.
    pub exposure_time: Option<Duration>,
    /// ISO speed (Styx: 100 times the sensor's gain).
    pub iso: Option<u32>,
    /// F-number, if the lens has a known one.
    pub f_number: Option<f64>,
    /// When the frame was captured: wall-clock time since the Unix epoch (UTC; with std,
    /// `SystemTime::now().duration_since(UNIX_EPOCH)`).
    pub capture_time: Option<Duration>,
    /// Camera maker.
    pub make: String,
    /// Camera model.
    pub model: String,
    /// Unique camera model (identifies the calibration; `make model` when empty).
    pub unique_camera_model: String,
    /// Software that wrote the file.
    pub software: String,
    /// Free text stored as the image description and EXIF user comment (e.g. JSON with the
    /// frame's sequence, monotonic timestamp, gains, colour temperature and lux).
    pub description: Option<String>,
    /// EXIF/TIFF orientation (1: as stored).
    pub orientation: u16,
    /// Opcodes applied to the raw data after black/white scaling (lens shading: `GainMap`s).
    pub opcode_list2: Vec<Opcode>,
    /// A preview stored in IFD 0 (the raw image then goes to a SubIFD).
    pub preview: Option<Preview>,
}

impl Default for DngMetadata {
    fn default() -> Self {
        Self {
            black_level: [0.0; 4],
            white_level: None,
            as_shot_neutral: None,
            calibrations: Vec::new(),
            baseline_exposure: None,
            exposure_time: None,
            iso: None,
            f_number: None,
            capture_time: None,
            make: "Styx".into(),
            model: "camera".into(),
            unique_camera_model: String::new(),
            software: concat!("styx-dng ", env!("CARGO_PKG_VERSION")).into(),
            description: None,
            orientation: 1,
            opcode_list2: Vec::new(),
            preview: None,
        }
    }
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian calendar.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// EXIF date `YYYY:MM:DD HH:MM:SS` (UTC) and the milliseconds, from the time since the Unix
/// epoch.
fn exif_time(d: Duration) -> (String, String) {
    let secs = d.as_secs() as i64;
    let (y, mo, da) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    (
        format!(
            "{y:04}:{mo:02}:{da:02} {:02}:{:02}:{:02}",
            s / 3600,
            s / 60 % 60,
            s % 60
        ),
        format!("{:03}", d.subsec_millis()),
    )
}

fn matrix(m: &Matrix3) -> Value {
    Value::SRational(m.iter().map(|&v| srational(v, 10_000)).collect())
}

/// The raw image's IFD.
fn raw_ifd(image: &RawImage, meta: &DngMetadata) -> Result<Ifd> {
    let mut ifd = Ifd::default();
    let (w, h) = (image.width, image.height);
    ifd.set(tag::NEW_SUBFILE_TYPE, Value::Long(vec![0]));
    ifd.set(tag::IMAGE_WIDTH, Value::Long(vec![w]));
    ifd.set(tag::IMAGE_LENGTH, Value::Long(vec![h]));
    let eight = image.bits <= 8;
    ifd.set(
        tag::BITS_PER_SAMPLE,
        Value::Short(vec![if eight { 8 } else { 16 }]),
    );
    ifd.set(tag::COMPRESSION, Value::Short(vec![1]));
    ifd.set(tag::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
    ifd.set(tag::ROWS_PER_STRIP, Value::Long(vec![h]));
    ifd.set(tag::PLANAR_CONFIGURATION, Value::Short(vec![1]));
    let cells = match image.layout {
        SampleLayout::Cfa(p) => {
            ifd.set(tag::PHOTOMETRIC, Value::Short(vec![32803]));
            ifd.set(tag::CFA_REPEAT_PATTERN_DIM, Value::Short(vec![2, 2]));
            ifd.set(tag::CFA_PATTERN, Value::Byte(p.colors().to_vec()));
            ifd.set(tag::CFA_PLANE_COLOR, Value::Byte(vec![0, 1, 2]));
            ifd.set(tag::CFA_LAYOUT, Value::Short(vec![1]));
            4
        }
        SampleLayout::Mono => {
            ifd.set(tag::PHOTOMETRIC, Value::Short(vec![34892]));
            1
        }
    };
    let side = if cells == 4 { 2 } else { 1 };
    ifd.set(tag::BLACK_LEVEL_REPEAT_DIM, Value::Short(vec![side, side]));
    let levels = &meta.black_level[..cells];
    if levels.iter().all(|v| v.fract() == 0.0 && *v >= 0.0) {
        ifd.set(
            tag::BLACK_LEVEL,
            Value::Long(levels.iter().map(|&v| v as u32).collect()),
        );
    } else {
        ifd.set(
            tag::BLACK_LEVEL,
            Value::Rational(levels.iter().map(|&v| rational(v, 1000)).collect()),
        );
    }
    let white = meta.white_level.unwrap_or_else(|| image.max_value());
    ifd.set(tag::WHITE_LEVEL, Value::Long(vec![white]));
    ifd.set(tag::DEFAULT_SCALE, Value::Rational(vec![(1, 1), (1, 1)]));
    ifd.set(tag::DEFAULT_CROP_ORIGIN, Value::Long(vec![0, 0]));
    ifd.set(tag::DEFAULT_CROP_SIZE, Value::Long(vec![w, h]));
    if !meta.opcode_list2.is_empty() {
        ifd.set(
            tag::OPCODE_LIST_2,
            Value::Undefined(opcode::encode_list(&meta.opcode_list2)),
        );
    }
    ifd.strip = Some(if eight {
        image.samples.iter().map(|&v| v.min(255) as u8).collect()
    } else {
        image.samples.iter().flat_map(|v| v.to_le_bytes()).collect()
    });
    Ok(ifd)
}

/// The preview's IFD (IFD 0 when there is a preview).
fn preview_ifd(p: &Preview) -> Result<Ifd> {
    if p.rgb.len() != p.width as usize * p.height as usize * 3 || p.width == 0 || p.height == 0 {
        return Err(invalid(format!(
            "preview {}x{} with {} bytes",
            p.width,
            p.height,
            p.rgb.len()
        )));
    }
    let mut ifd = Ifd::default();
    ifd.set(tag::NEW_SUBFILE_TYPE, Value::Long(vec![1]));
    ifd.set(tag::IMAGE_WIDTH, Value::Long(vec![p.width]));
    ifd.set(tag::IMAGE_LENGTH, Value::Long(vec![p.height]));
    ifd.set(tag::BITS_PER_SAMPLE, Value::Short(vec![8, 8, 8]));
    ifd.set(tag::COMPRESSION, Value::Short(vec![1]));
    ifd.set(tag::PHOTOMETRIC, Value::Short(vec![2]));
    ifd.set(tag::SAMPLES_PER_PIXEL, Value::Short(vec![3]));
    ifd.set(tag::ROWS_PER_STRIP, Value::Long(vec![p.height]));
    ifd.set(tag::PLANAR_CONFIGURATION, Value::Short(vec![1]));
    // sRGB.
    ifd.set(tag::PREVIEW_COLOR_SPACE, Value::Long(vec![2]));
    ifd.strip = Some(p.rgb.clone());
    Ok(ifd)
}

fn exif_ifd(meta: &DngMetadata) -> Ifd {
    let mut exif = Ifd::default();
    exif.set(tag::EXIF_VERSION, Value::Undefined(b"0230".to_vec()));
    if let Some(t) = meta.exposure_time {
        let s = t.as_secs_f64();
        // 1/n for short exposures (as cameras write them), else microseconds.
        let r = if s > 0.0 && s < 1.0 && (1.0 / s).fract().abs() < 1e-6 {
            (1, (1.0 / s).round() as u32)
        } else {
            rational(s, 1_000_000)
        };
        exif.set(tag::EXPOSURE_TIME, Value::Rational(vec![r]));
    }
    if let Some(f) = meta.f_number {
        exif.set(tag::F_NUMBER, Value::Rational(vec![rational(f, 100)]));
    }
    if let Some(iso) = meta.iso {
        exif.set(
            tag::ISO_SPEED_RATINGS,
            Value::Short(vec![iso.min(65535) as u16]),
        );
        // Recommended exposure index.
        exif.set(tag::SENSITIVITY_TYPE, Value::Short(vec![2]));
    }
    if let Some(t) = meta.capture_time {
        let (date, ms) = exif_time(t);
        exif.set(tag::DATE_TIME_ORIGINAL, Value::Ascii(date));
        exif.set(tag::SUB_SEC_TIME_ORIGINAL, Value::Ascii(ms));
    }
    if let Some(d) = &meta.description {
        let mut c = b"ASCII\0\0\0".to_vec();
        c.extend_from_slice(d.as_bytes());
        exif.set(tag::USER_COMMENT, Value::Undefined(c));
    }
    exif
}

/// Writes `image` with `meta` as a DNG file.
pub fn write_dng(image: &RawImage, meta: &DngMetadata) -> Result<Vec<u8>> {
    if image.samples.len() != image.width as usize * image.height as usize {
        return Err(invalid("sample count does not match the size"));
    }
    if meta.calibrations.len() > 2 {
        return Err(invalid("DNG 1.4 holds at most two calibrations"));
    }
    let raw = raw_ifd(image, meta)?;
    let mut root = match &meta.preview {
        Some(p) => {
            let mut root = preview_ifd(p)?;
            root.children.push((tag::SUB_IFDS, vec![raw]));
            root
        }
        None => raw,
    };
    root.set(tag::DNG_VERSION, Value::Byte(vec![1, 4, 0, 0]));
    let backward = if meta.opcode_list2.is_empty() { 1 } else { 3 };
    root.set(
        tag::DNG_BACKWARD_VERSION,
        Value::Byte(vec![1, backward, 0, 0]),
    );
    root.set(tag::MAKE, Value::Ascii(meta.make.clone()));
    root.set(tag::MODEL, Value::Ascii(meta.model.clone()));
    let unique = if meta.unique_camera_model.is_empty() {
        format!("{} {}", meta.make, meta.model)
    } else {
        meta.unique_camera_model.clone()
    };
    root.set(tag::UNIQUE_CAMERA_MODEL, Value::Ascii(unique));
    root.set(tag::SOFTWARE, Value::Ascii(meta.software.clone()));
    root.set(tag::ORIENTATION, Value::Short(vec![meta.orientation]));
    if let Some(d) = &meta.description {
        root.set(tag::IMAGE_DESCRIPTION, Value::Ascii(d.clone()));
    }
    if let Some(t) = meta.capture_time {
        root.set(tag::DATE_TIME, Value::Ascii(exif_time(t).0));
    }
    if let Some(n) = meta.as_shot_neutral {
        root.set(
            tag::AS_SHOT_NEUTRAL,
            Value::Rational(n.iter().map(|&v| rational(v, 1_000_000)).collect()),
        );
    }
    if let Some(b) = meta.baseline_exposure {
        root.set(
            tag::BASELINE_EXPOSURE,
            Value::SRational(vec![srational(b, 100)]),
        );
    }
    let tags = [
        (
            tag::CALIBRATION_ILLUMINANT_1,
            tag::COLOR_MATRIX_1,
            tag::FORWARD_MATRIX_1,
        ),
        (
            tag::CALIBRATION_ILLUMINANT_2,
            tag::COLOR_MATRIX_2,
            tag::FORWARD_MATRIX_2,
        ),
    ];
    if matches!(image.layout, SampleLayout::Cfa(_)) {
        for (c, (ill, cm, fm)) in meta.calibrations.iter().zip(tags) {
            root.set(ill, Value::Short(vec![c.illuminant.code()]));
            root.set(cm, matrix(&c.color_matrix));
            if let Some(f) = &c.forward_matrix {
                root.set(fm, matrix(f));
            }
        }
    }
    root.children.push((tag::EXIF_IFD, vec![exif_ifd(meta)]));
    crate::tiff::write(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        assert_eq!(civil(20_729), (2026, 10, 3));
        let t = Duration::from_millis(1_791_000_000_123);
        assert_eq!(
            exif_time(t),
            ("2026:10:03 04:00:00".to_string(), "123".to_string())
        );
    }
}
