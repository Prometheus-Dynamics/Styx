//! How an MCAP recording stores 16-bit Bayer frames (`Frames::sensor_raw`: the sensor's value
//! in the top bits of each 16-bit sample): as they are, packed to 10 bits (MIPI CSI-2 RAW10,
//! no loss for sensors of up to 10 bits), or their top 8 bits. The frame is recorded in that
//! format, so replay gives frames in it.

use styx_core::prelude::*;

use super::ReplayError;

/// How a recording stores 16-bit Bayer frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RawStorage {
    /// As they are (2 bytes a sample; chunk compression takes the zero low bits well).
    #[default]
    AsIs,
    /// MIPI CSI-2 packed RAW10 (`pRAA` and its other orders): 1.25 bytes a sample, the top 10
    /// bits; no loss for sensors of up to 10 bits.
    Packed10,
    /// The top 8 bits (8-bit Bayer): 1 byte a sample, lossy.
    Top8,
}

/// The packed RAW10 and 8-bit formats of a 16-bit Bayer format's colour filter.
fn targets(code: FourCc) -> Option<(FourCc, FourCc)> {
    Some(match &code.to_u32().to_le_bytes() {
        b"BYR2" | b"BG16" => (FourCc::new(*b"pBAA"), FourCc::BGGR),
        b"GB16" => (FourCc::new(*b"pGAA"), FourCc::GBRG),
        b"GR16" => (FourCc::new(*b"pgAA"), FourCc::GRBG),
        b"RG16" => (FourCc::new(*b"pRAA"), FourCc::RGGB),
        _ => return None,
    })
}

/// `frame` in `storage`'s format; `None` when it stays as it is (not 16-bit Bayer, or
/// [`RawStorage::AsIs`], or a width packing cannot take).
pub(super) fn convert(
    frame: &FrameLease,
    storage: RawStorage,
) -> Result<Option<FrameLease>, ReplayError> {
    let format = frame.meta().format;
    let (Some((packed, eight)), false) = (targets(format.code), storage == RawStorage::AsIs) else {
        return Ok(None);
    };
    let (w, h) = (
        format.resolution.width.get() as usize,
        format.resolution.height.get() as usize,
    );
    if storage == RawStorage::Packed10 && w % 4 != 0 {
        return Ok(None);
    }
    let src = frame
        .to_visible_vec()
        .map_err(|e| ReplayError::Frame(e.to_string()))?;
    let sample = |row: &[u8], x: usize| u16::from_le_bytes([row[2 * x], row[2 * x + 1]]);
    let (code, out) = match storage {
        RawStorage::Top8 => {
            let mut out = Vec::with_capacity(w * h);
            for row in src.chunks_exact(2 * w).take(h) {
                out.extend((0..w).map(|x| (sample(row, x) >> 8) as u8));
            }
            (eight, out)
        }
        _ => {
            let mut out = Vec::with_capacity(w * h * 5 / 4);
            for row in src.chunks_exact(2 * w).take(h) {
                for x in (0..w).step_by(4) {
                    let v: [u16; 4] = core::array::from_fn(|i| sample(row, x + i) >> 6);
                    out.extend(v.map(|s| (s >> 2) as u8));
                    out.push(
                        (v[0] & 3) as u8
                            | ((v[1] & 3) as u8) << 2
                            | ((v[2] & 3) as u8) << 4
                            | ((v[3] & 3) as u8) << 6,
                    );
                }
            }
            (packed, out)
        }
    };
    let mut converted = FrameLease::from_visible_bytes(
        MediaFormat::new(code, format.resolution, format.color),
        frame.meta().timestamp,
        &out,
    )
    .map_err(|e| ReplayError::Frame(e.to_string()))?;
    let meta = frame.meta().clone();
    *converted.meta_mut() = FrameMeta {
        format: converted.meta().format,
        ..meta
    };
    Ok(Some(converted))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rg16(w: u32, h: u32, value: impl Fn(usize) -> u16) -> FrameLease {
        let bytes: Vec<u8> = (0..(w * h) as usize)
            .flat_map(|i| (value(i) << 6).to_le_bytes())
            .collect();
        let format = MediaFormat::new(
            FourCc::new(*b"RG16"),
            Resolution::new(w, h).unwrap(),
            ColorSpace::Unknown,
        );
        FrameLease::from_visible_bytes(format, 5, &bytes).unwrap()
    }

    #[test]
    fn raw_frames_pack_to_10_bits_or_keep_their_top_8() {
        let f = rg16(8, 2, |i| (i as u16 * 97) & 0x3ff);
        assert!(convert(&f, RawStorage::AsIs).unwrap().is_none());
        let p = convert(&f, RawStorage::Packed10).unwrap().unwrap();
        assert_eq!(p.meta().format.code, FourCc::new(*b"pRAA"));
        assert_eq!(p.meta().timestamp, 5);
        let bytes = p.to_visible_vec().unwrap();
        assert_eq!(bytes.len(), 8 * 2 * 5 / 4);
        // Unpacked again: every 10-bit sample comes back.
        for i in 0..16 {
            let (group, k) = (i / 4, i % 4);
            let hi = u16::from(bytes[group * 5 + k]);
            let lo = u16::from(bytes[group * 5 + 4] >> (2 * k)) & 3;
            assert_eq!(hi << 2 | lo, (i as u16 * 97) & 0x3ff, "sample {i}");
        }
        let t = convert(&f, RawStorage::Top8).unwrap().unwrap();
        assert_eq!(t.meta().format.code, FourCc::RGGB);
        assert_eq!(
            t.to_visible_vec().unwrap()[3],
            (((3 * 97) & 0x3ff) >> 2) as u8
        );
        // Other formats are left alone.
        let grey = FrameLease::from_visible_bytes(
            MediaFormat::new(
                FourCc::GREY,
                Resolution::new(4, 1).unwrap(),
                ColorSpace::Unknown,
            ),
            0,
            &[1, 2, 3, 4],
        )
        .unwrap();
        assert!(convert(&grey, RawStorage::Top8).unwrap().is_none());
    }
}
