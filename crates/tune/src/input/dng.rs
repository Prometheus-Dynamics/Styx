//! DNG files through `styx-dng`'s reader: Styx's own stills, `picamera2`'s and cameras' DNGs
//! (uncompressed at any depth or lossless JPEG, strips or tiles, either byte order), cropped
//! to the active area, with `CFAPattern`, `BlackLevel`, `WhiteLevel`, `ExposureTime` and
//! `ISOSpeedRatings` (gain = ISO / 100, as Styx and `picamera2` write it).

use super::RawDecoder;
use crate::error::{Result, TuneError};
use crate::raw::{Cfa, RawFrame};

/// The DNG decoder (`styx_dng::read_dng`).
#[derive(Clone, Copy, Debug, Default)]
pub struct StyxDng;

impl RawDecoder for StyxDng {
    fn decode(&self, bytes: &[u8]) -> Result<Vec<RawFrame>> {
        decode(bytes).map(|f| vec![f])
    }
}

fn bad(m: impl Into<String>) -> TuneError {
    TuneError::Format(format!("DNG: {}", m.into()))
}

fn cfa(p: styx_dng::CfaPattern) -> Cfa {
    match p {
        styx_dng::CfaPattern::Rggb => Cfa::Rggb,
        styx_dng::CfaPattern::Grbg => Cfa::Grbg,
        styx_dng::CfaPattern::Gbrg => Cfa::Gbrg,
        styx_dng::CfaPattern::Bggr => Cfa::Bggr,
    }
}

/// One raw frame from a DNG file's bytes.
pub fn decode(bytes: &[u8]) -> Result<RawFrame> {
    let d = styx_dng::read_dng(bytes).map_err(|e| bad(e.to_string()))?;
    if d.samples_per_pixel != 1 {
        return Err(bad("not a CFA image (linear raw)"));
    }
    let [top, left, bottom, right] = d
        .active_area
        .unwrap_or([0, 0, d.height, d.width])
        .map(|v| v as usize);
    let (w, h) = (
        right.min(d.width as usize).saturating_sub(left),
        bottom.min(d.height as usize).saturating_sub(top),
    );
    if w == 0 || h == 0 {
        return Err(bad("empty active area"));
    }
    // The pattern from the active area's corner.
    let colours: Vec<u8> = (0..4)
        .map(|i| d.color_at((left + i % 2) as u32, (top + i / 2) as u32))
        .collect::<Option<_>>()
        .ok_or_else(|| bad("not a CFA image"))?;
    let pattern = styx_dng::CfaPattern::from_colors(&colours)
        .ok_or_else(|| bad("not a 2x2 Bayer pattern"))?;
    let full_w = d.width as usize;
    let data: Vec<u16> = (top..top + h)
        .flat_map(|y| {
            d.samples[y * full_w + left..y * full_w + left + w]
                .iter()
                .copied()
        })
        .collect();
    let white = d
        .white_level
        .first()
        .map_or(f64::from((1u32 << d.bits_per_sample) - 1), |&v| {
            f64::from(v)
        });
    // Significant bits from the white level (16-bit containers of 10-bit data).
    let bits = (white + 1.0).log2().ceil().clamp(8.0, 16.0) as u8;
    let black = (!d.black_level.is_empty()).then(|| {
        d.black_level.iter().sum::<f64>() / d.black_level.len() as f64 / f64::from(1u32 << bits)
    });
    Ok(RawFrame {
        width: w,
        height: h,
        cfa: cfa(pattern),
        bits,
        data,
        exposure_us: d.exposure_time.map_or(0.0, |s| s * 1e6),
        analogue_gain: d.iso.map_or(1.0, |iso| f64::from(iso) / 100.0),
        digital_gain: 1.0,
        black_level: black,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use styx_dng::{CfaPattern, DngMetadata, RawImage, SampleLayout, write_dng};

    use super::*;

    #[test]
    fn reads_a_styx_dng() {
        let samples: Vec<u16> = (0..8).map(|i| 64 + i * 100).collect();
        let img = RawImage::new(
            4,
            2,
            SampleLayout::Cfa(CfaPattern::Bggr),
            10,
            samples.clone(),
        )
        .unwrap();
        let meta = DngMetadata {
            black_level: [64.0; 4],
            exposure_time: Some(Duration::from_millis(10)),
            iso: Some(400),
            ..DngMetadata::default()
        };
        let f = StyxDng.decode(&write_dng(&img, &meta).unwrap()).unwrap();
        let f = &f[0];
        assert_eq!((f.width, f.height, f.cfa, f.bits), (4, 2, Cfa::Bggr, 10));
        assert_eq!(f.data, samples);
        assert!((f.exposure_us - 10_000.0).abs() < 1e-6);
        assert_eq!(f.analogue_gain, 4.0);
        assert_eq!(f.black_level, Some(64.0 / 1024.0));
        assert!(decode(b"not a dng").is_err());
    }
}
