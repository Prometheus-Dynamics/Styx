//! The raw sensor frames of a native camera's processed capture: the frame the PiSP reads, as
//! a companion of the processed one (`NativeIspConfig::sensor_raw`), for a consumer that asks
//! for the sensor's 16-bit Bayer format (`Frames::sensor_raw`). Other consumers share the
//! capture as usual; the raw consumer takes no ISP output.

use styx_codec::CodecRegistryHandle;
use styx_core::prelude::FourCc;

use super::request::FrameRequest;
use super::routes::{self, Candidate, Route};
use crate::prelude::{Mode, ProbedBackend};

/// The 16-bit Bayer formats, by colour filter: BGGR, GBRG, GRBG, RGGB (V4L2's SBGGR16...).
pub(crate) const SENSOR_RAW_CODES: [FourCc; 4] = [
    FourCc::new(*b"BYR2"),
    FourCc::new(*b"GB16"),
    FourCc::new(*b"GR16"),
    FourCc::new(*b"RG16"),
];

/// The 16-bit format of the colour filter of Bayer format `code`.
fn raw16(code: FourCc) -> Option<FourCc> {
    let order = match &code.to_u32().to_le_bytes() {
        b"BA81" | b"pBAA" | b"pBCC" | b"BG10" | b"BG12" | b"BG16" | b"BYR2" => 0,
        b"GBRG" | b"pGAA" | b"pGCC" | b"GB10" | b"GB12" | b"GB16" => 1,
        b"GRBG" | b"pgAA" | b"pgCC" | b"BA10" | b"BA12" | b"GR16" => 2,
        b"RGGB" | b"pRAA" | b"pRCC" | b"RG10" | b"RG12" | b"RG16" => 3,
        _ => return None,
    };
    Some(SENSOR_RAW_CODES[order])
}

/// The camera's raw frames' format through its PiSP, from its raw modes' colour filter.
fn sensor_raw_code(backend: &ProbedBackend) -> Option<FourCc> {
    backend
        .descriptor
        .modes
        .iter()
        .find_map(|m| raw16(m.format.code))
}

/// `req` taking the raw frames of `mode`, a processed mode of a native camera's PiSP: the
/// whole frame, at the mode's size, in the sensor's 16-bit Bayer format. `None` when the mode
/// is not one or `req` does not ask for that format.
pub(crate) fn candidate<'a>(
    backend: &'a ProbedBackend,
    mode: &Mode,
    req: &FrameRequest,
    registry: &CodecRegistryHandle,
) -> Option<Candidate<'a>> {
    if !routes::native_isp_outputs(backend, mode) {
        return None;
    }
    let code = sensor_raw_code(backend).filter(|&c| req.accepts(c))?;
    let mut alt = mode.clone();
    alt.format.code = code;
    alt.id.format.code = code;
    let mut c = routes::candidate(backend, &alt, req, registry).ok()?;
    if !matches!(c.route, Route::Direct) {
        return None;
    }
    // The capture runs the processed mode; this consumer's frames are its raw companions.
    c.mode = mode.clone();
    c.isp_output = None;
    c.isp_format = Some(code);
    c.sensor_raw = true;
    c.notes.push(format!(
        "{code}: the raw frame the PiSP reads, with the processed one (no copy)"
    ));
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_formats_map_to_their_16_bit_order() {
        assert_eq!(raw16(FourCc::new(*b"pRAA")), Some(FourCc::new(*b"RG16")));
        assert_eq!(raw16(FourCc::new(*b"BA81")), Some(FourCc::new(*b"BYR2")));
        assert_eq!(raw16(FourCc::new(*b"BA10")), Some(FourCc::new(*b"GR16")));
        assert_eq!(raw16(FourCc::NV12), None);
    }
}
