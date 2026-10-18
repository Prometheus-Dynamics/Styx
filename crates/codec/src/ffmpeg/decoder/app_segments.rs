//! JPEG APP-segment stripping for FFmpeg's MJPEG decoder.

use std::borrow::Cow;

/// Drops APP0..APP15 segments (EXIF, JFIF, ...) ahead of the scan so FFmpeg's MJPEG decoder sees
/// a clean header. Returns the input unchanged, without copying, when there is nothing to strip
/// or the marker layout is not a clean run of segments.
pub(super) fn strip_app_segments(data: &[u8]) -> Cow<'_, [u8]> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Cow::Borrowed(data);
    }
    // Pass 1 (headers only, no copy): most frames carry no APP segment at all.
    let mut has_app = false;
    let well_formed = walk_segments(data, |marker, _, _| has_app |= is_app_marker(marker));
    if well_formed.is_none() || !has_app {
        return Cow::Borrowed(data);
    }
    // Pass 2: copy SOI, the kept segments, then the entropy-coded tail verbatim.
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..2]);
    let Some(tail) = walk_segments(data, |marker, start, end| {
        if !is_app_marker(marker) {
            out.extend_from_slice(&data[start..end]);
        }
    }) else {
        return Cow::Borrowed(data);
    };
    out.extend_from_slice(&data[tail..]);
    Cow::Owned(out)
}

fn is_app_marker(marker: u8) -> bool {
    (0xE0..=0xEF).contains(&marker)
}

/// Visits each marker segment between SOI and the start of scan (or EOI), passing its marker
/// byte and `start..end` range (from the `0xFF`). Returns the offset of the SOS/EOI marker, or
/// `None` when the layout is malformed or ends before a scan.
fn walk_segments(data: &[u8], mut visit: impl FnMut(u8, usize, usize)) -> Option<usize> {
    let mut pos = 2usize;
    while pos + 3 < data.len() {
        if data[pos] != 0xFF {
            return None;
        }
        let marker = data[pos + 1];
        let start = pos;
        pos += 2;
        if marker == 0xD9 || marker == 0xDA {
            return Some(start);
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        if len < 2 || pos + len > data.len() {
            return None;
        }
        pos += len;
        visit(marker, start, pos);
    }
    None
}
