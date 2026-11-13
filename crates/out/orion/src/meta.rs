//! The frame's identity, format and plane layouts as a ring entry carries them: the bytes
//! `styx_socket::Producer` writes into each entry's metadata.
//!
//! `styx-socket` keeps its codec private, and this crate may not change it, so the read side
//! lives here. The layout is fixed and little-endian; the test in `tests/adopt.rs` reads the same
//! frames through a direct socket reader and an Orion subscriber and checks the decoded identity,
//! format and layouts match, so a change on either side fails there.
//!
//! | Bytes | Holds |
//! | --- | --- |
//! | 0 to 7 | The frame's id |
//! | 8 to 11 | Format four-character code |
//! | 12 to 15, 16 to 19 | Width, height |
//! | 20, 21 | Colour space code, colour range code (0 full, 1 limited, 2 unknown) |
//! | 22 | Planes |
//! | 24 + 24 * plane | Each plane: offset, stride, length, each a `u64` |

use core::num::NonZeroU32;

use styx_format::{ColorRange, ColorSpace, FourCc, MAX_PLANES, MediaFormat, Resolution};
use styx_frame::{Layouts, PlaneLayout};

const HEADER: usize = 24;
const PLANE: usize = 24;

/// What a ring entry's metadata names: the frame's id, format and plane layouts.
pub(crate) fn decode(bytes: &[u8]) -> Option<(u64, MediaFormat, Layouts)> {
    if bytes.len() < HEADER {
        return None;
    }
    let read_u32 = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let read_u64 = |at: usize| Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?));
    let id = read_u64(0)?;
    let code = FourCc::new(bytes.get(8..12)?.try_into().ok()?);
    let width = NonZeroU32::new(read_u32(12)?)?;
    let height = NonZeroU32::new(read_u32(16)?)?;
    let color = ColorSpace::from_code(bytes[20]);
    let range = match bytes[21] {
        0 => ColorRange::Full,
        1 => ColorRange::Limited,
        2 => ColorRange::Unknown,
        _ => return None,
    };
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
