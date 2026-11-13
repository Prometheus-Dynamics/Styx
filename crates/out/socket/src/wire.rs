//! The bytes this crate puts on the ring and on the handshake socket. Little-endian, every field
//! at a fixed place. Styx defines these bytes; the ring carries them without reading them.

use core::num::NonZeroU32;

use styx_format::{ColorRange, ColorSpace, FourCc, MAX_PLANES, MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, PlaneLayout};

use crate::geometry::Geometry;

/// Bytes of a frame's ring metadata before its planes.
const HEADER: usize = 24;
/// Bytes of one plane: offset, stride and length, each a `u64`.
const PLANE: usize = 24;
/// The most metadata bytes one frame's ring entry carries.
pub(crate) const META_MAX: usize = HEADER + MAX_PLANES * PLANE;

/// Version of the handshake messages. A reader refuses any other.
pub(crate) const VERSION: u32 = 2;
/// The offer: version, buffers, the reader's slot, the most buffers it may hold, history, the
/// metadata size, the payload and the slack. Its descriptors are the ring's read end, the pool
/// page and the pool's one memfd, in that order.
pub(crate) const OFFER_LEN: usize = 4 * 6 + 8 * 2;
/// The reader's reply: version and its slot.
pub(crate) const ACK_LEN: usize = 8;

/// What an offer names: the reader's slot and limit, and the geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Offer {
    pub slot: u32,
    pub max_held: u32,
    pub geometry: Geometry,
}

/// Writes `offer` into `out`.
pub(crate) fn encode_offer(offer: &Offer) -> [u8; OFFER_LEN] {
    let geometry = &offer.geometry;
    let mut out = [0_u8; OFFER_LEN];
    out[0..4].copy_from_slice(&VERSION.to_le_bytes());
    out[4..8].copy_from_slice(&(geometry.buffers() as u32).to_le_bytes());
    out[8..12].copy_from_slice(&offer.slot.to_le_bytes());
    out[12..16].copy_from_slice(&offer.max_held.to_le_bytes());
    out[16..20].copy_from_slice(&geometry.history().to_le_bytes());
    out[20..24].copy_from_slice(&(geometry.meta_max() as u32).to_le_bytes());
    out[24..32].copy_from_slice(&(geometry.payload() as u64).to_le_bytes());
    out[32..40].copy_from_slice(&(geometry.slack() as u64).to_le_bytes());
    out
}

/// Reads an offer; `None` for one this build does not understand or whose geometry it refuses.
pub(crate) fn decode_offer(bytes: &[u8]) -> Option<Offer> {
    if bytes.len() != OFFER_LEN {
        return None;
    }
    let u32_at = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let u64_at = |at: usize| Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?));
    if u32_at(0)? != VERSION {
        return None;
    }
    let geometry = Geometry::checked(
        u32_at(16)?,
        usize::try_from(u32_at(20)?).ok()?,
        usize::try_from(u32_at(4)?).ok()?,
        usize::try_from(u64_at(24)?).ok()?,
        usize::try_from(u64_at(32)?).ok()?,
    )
    .ok()?;
    Some(Offer {
        slot: u32_at(8)?,
        max_held: u32_at(12)?,
        geometry,
    })
}

fn range_code(range: ColorRange) -> u8 {
    match range {
        ColorRange::Full => 0,
        ColorRange::Limited => 1,
        ColorRange::Unknown => 2,
    }
}

fn range_from_code(code: u8) -> Option<ColorRange> {
    Some(match code {
        0 => ColorRange::Full,
        1 => ColorRange::Limited,
        2 => ColorRange::Unknown,
        _ => return None,
    })
}

/// Writes `entry`'s identity, format and plane layouts into `out`; returns the length.
pub(crate) fn encode_meta(entry: &FrameEntry, out: &mut [u8; META_MAX]) -> usize {
    let planes = entry.layouts.len();
    out[0..8].copy_from_slice(&entry.id.to_le_bytes());
    out[8..12].copy_from_slice(&entry.format.code.bytes());
    out[12..16].copy_from_slice(&entry.format.resolution.width.get().to_le_bytes());
    out[16..20].copy_from_slice(&entry.format.resolution.height.get().to_le_bytes());
    out[20] = entry.format.color.code();
    out[21] = range_code(entry.format.range);
    out[22] = planes as u8;
    out[23] = 0;
    for (index, plane) in entry.layouts.iter().enumerate() {
        let at = HEADER + index * PLANE;
        out[at..at + 8].copy_from_slice(&(plane.offset as u64).to_le_bytes());
        out[at + 8..at + 16].copy_from_slice(&(plane.stride as u64).to_le_bytes());
        out[at + 16..at + 24].copy_from_slice(&(plane.len as u64).to_le_bytes());
    }
    HEADER + planes * PLANE
}

/// What [`encode_meta`] wrote, read back. `None` for bytes this build does not understand.
pub(crate) fn decode_meta(bytes: &[u8]) -> Option<(u64, MediaFormat, Layouts)> {
    if bytes.len() < HEADER {
        return None;
    }
    let read_u32 = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let read_u64 = |at: usize| Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?));
    let id: u64 = read_u64(0)?;
    let code = FourCc::new(bytes.get(8..12)?.try_into().ok()?);
    let width = NonZeroU32::new(read_u32(12)?)?;
    let height = NonZeroU32::new(read_u32(16)?)?;
    let color = ColorSpace::from_code(bytes[20]);
    let range = range_from_code(bytes[21])?;
    let planes = usize::from(bytes[22]);
    if planes > MAX_PLANES || bytes.len() < HEADER + planes * PLANE {
        return None;
    }
    let mut items = [PlaneLayout::default(); MAX_PLANES];
    for (index, item) in items.iter_mut().take(planes).enumerate() {
        let at = HEADER + index * PLANE;
        *item = PlaneLayout::new(
            usize::try_from(read_u64(at)?).ok()?,
            usize::try_from(read_u64(at + 8)?).ok()?,
            usize::try_from(read_u64(at + 16)?).ok()?,
        );
    }
    let layouts = Layouts::new(&items[..planes])?;
    let format = MediaFormat::new(code, Resolution { width, height }, color, range);
    Some((id, format, layouts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_round_trips_format_and_planes() {
        let format = MediaFormat::new(
            FourCc::NV12,
            Resolution::new(640, 480).expect("resolution"),
            ColorSpace::Bt709,
            ColorRange::Limited,
        );
        let layouts =
            styx_frame::packed_layouts(FourCc::NV12, 640, 480, None, None).expect("layouts");
        let entry = FrameEntry::new(42, 1_234_567, format, layouts);
        let mut out = [0_u8; META_MAX];
        let len = encode_meta(&entry, &mut out);
        let (id, decoded, planes) = decode_meta(&out[..len]).expect("decodes");
        assert_eq!(id, 42);
        assert_eq!(decoded, format);
        assert_eq!(planes, layouts);
    }

    #[test]
    fn an_offer_round_trips_its_slot_limit_and_geometry() {
        let geometry = crate::geometry::Geometry::new(4096, 300, 8).expect("geometry");
        let offer = Offer {
            slot: 3,
            max_held: 5,
            geometry,
        };
        assert_eq!(decode_offer(&encode_offer(&offer)), Some(offer));
        let mut bytes = encode_offer(&offer);
        bytes[0] = 1;
        assert!(decode_offer(&bytes).is_none(), "another version is refused");
    }

    #[test]
    fn metadata_with_an_unknown_range_or_too_many_planes_is_refused() {
        let mut bytes = [0_u8; META_MAX];
        bytes[12..16].copy_from_slice(&8_u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&8_u32.to_le_bytes());
        bytes[21] = 9;
        assert!(decode_meta(&bytes).is_none());
        bytes[21] = 2;
        bytes[22] = (MAX_PLANES + 1) as u8;
        assert!(decode_meta(&bytes).is_none());
        assert!(decode_meta(&bytes[..10]).is_none());
    }
}
