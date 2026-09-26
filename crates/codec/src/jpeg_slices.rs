//! Split a baseline JPEG with restart markers into independently decodable horizontal slices.
//!
//! Restart markers reset the entropy decoder (DC predictors and bit buffer), so the scan data
//! between two markers that fall on MCU-row boundaries can be decoded on its own. Each slice is
//! re-wrapped as a standalone JPEG: the original headers with the frame height patched to the
//! slice height, the slice's entropy data with its restart markers renumbered from RST0, and
//! EOI. Many UVC cameras (e.g. Logitech C270) emit restart markers on every frame.

/// A standalone JPEG covering output rows `first_row..first_row + rows`.
pub(crate) struct JpegSlice {
    pub jpeg: Vec<u8>,
    pub first_row: usize,
    pub rows: usize,
}

struct ScanLayout {
    width: usize,
    height: usize,
    height_offset: usize,
    mcu_width: usize,
    mcu_height: usize,
    restart_interval: usize,
    scan_start: usize,
}

/// Split `jpeg` into at most `max_slices` slices of roughly equal height.
///
/// Returns `None` when the image cannot be split: progressive or multi-scan JPEGs, no restart
/// interval, restart intervals that never end on an MCU row, or inconsistent marker counts.
pub(crate) fn split_jpeg(jpeg: &[u8], max_slices: usize) -> Option<Vec<JpegSlice>> {
    if max_slices < 2 {
        return None;
    }
    let layout = parse_headers(jpeg)?;
    let (markers, eoi) = restart_markers(jpeg, layout.scan_start)?;

    let mcus_per_row = layout.width.div_ceil(layout.mcu_width);
    let mcu_rows = layout.height.div_ceil(layout.mcu_height);
    let total_mcus = mcus_per_row * mcu_rows;
    let intervals = total_mcus.div_ceil(layout.restart_interval);
    if markers.len() + 1 != intervals {
        return None;
    }

    // Interval boundaries (after interval k) that land exactly on an MCU row.
    let target = mcu_rows.div_ceil(max_slices).max(1);
    let mut cuts = Vec::new(); // (interval index after which to cut, mcu row)
    let mut last_row = 0;
    for k in 0..intervals - 1 {
        let mcus = (k + 1) * layout.restart_interval;
        if mcus % mcus_per_row == 0 {
            let row = mcus / mcus_per_row;
            if row - last_row >= target && mcu_rows - row > 0 {
                cuts.push((k, row));
                last_row = row;
            }
        }
    }
    if cuts.is_empty() {
        return None;
    }

    let interval_start = |k: usize| {
        if k == 0 {
            layout.scan_start
        } else {
            markers[k - 1] + 2
        }
    };
    let mut slices = Vec::with_capacity(cuts.len() + 1);
    let mut first_interval = 0;
    let mut first_mcu_row = 0;
    for (end_interval, mcu_row) in cuts
        .iter()
        .copied()
        .chain(std::iter::once((intervals - 1, mcu_rows)))
    {
        let data_end = if end_interval + 1 < intervals {
            markers[end_interval]
        } else {
            eoi
        };
        let first_row = first_mcu_row * layout.mcu_height;
        let rows = (mcu_row * layout.mcu_height).min(layout.height) - first_row;
        slices.push(JpegSlice {
            jpeg: build_slice(
                jpeg,
                &layout,
                interval_start(first_interval),
                data_end,
                rows,
            )?,
            first_row,
            rows,
        });
        first_interval = end_interval + 1;
        first_mcu_row = mcu_row;
    }
    Some(slices)
}

fn build_slice(
    jpeg: &[u8],
    layout: &ScanLayout,
    data_start: usize,
    data_end: usize,
    rows: usize,
) -> Option<Vec<u8>> {
    let rows = u16::try_from(rows).ok()?;
    let mut out = Vec::with_capacity(layout.scan_start + (data_end - data_start) + 2);
    out.extend_from_slice(&jpeg[..layout.scan_start]);
    out[layout.height_offset..layout.height_offset + 2].copy_from_slice(&rows.to_be_bytes());
    let data_offset = out.len();
    out.extend_from_slice(&jpeg[data_start..data_end]);
    // Renumber restart markers so the slice's first one is RST0, as the decoder expects.
    let mut next = 0u8;
    let mut i = data_offset;
    while i + 1 < out.len() {
        if out[i] == 0xFF && (0xD0..=0xD7).contains(&out[i + 1]) {
            out[i + 1] = 0xD0 + next;
            next = (next + 1) % 8;
            i += 2;
        } else {
            i += 1;
        }
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    Some(out)
}

fn be16(b: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]) as usize)
}

fn parse_headers(jpeg: &[u8]) -> Option<ScanLayout> {
    if jpeg.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    let mut frame: Option<(usize, usize, usize, usize, usize)> = None; // w, h, h_off, mcu_w, mcu_h
    let mut restart_interval = 0;
    loop {
        while *jpeg.get(i)? == 0xFF && *jpeg.get(i + 1)? == 0xFF {
            i += 1; // fill bytes
        }
        if *jpeg.get(i)? != 0xFF {
            return None;
        }
        let marker = *jpeg.get(i + 1)?;
        let len = be16(jpeg, i + 2)?;
        let body = i + 4;
        match marker {
            // Baseline / extended sequential Huffman.
            0xC0 | 0xC1 => {
                let height = be16(jpeg, body + 1)?;
                let width = be16(jpeg, body + 3)?;
                let components = *jpeg.get(body + 5)? as usize;
                let (mut max_h, mut max_v) = (1, 1);
                for c in 0..components {
                    let sampling = *jpeg.get(body + 6 + c * 3 + 1)?;
                    max_h = max_h.max((sampling >> 4) as usize);
                    max_v = max_v.max((sampling & 0x0F) as usize);
                }
                frame = Some((width, height, body + 1, 8 * max_h, 8 * max_v));
            }
            // Progressive, lossless, arithmetic: not splittable here.
            0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return None,
            0xDD => restart_interval = be16(jpeg, body)?,
            0xDA => {
                let (width, height, height_offset, mcu_width, mcu_height) = frame?;
                if restart_interval == 0 || width == 0 || height == 0 {
                    return None;
                }
                return Some(ScanLayout {
                    width,
                    height,
                    height_offset,
                    mcu_width,
                    mcu_height,
                    restart_interval,
                    scan_start: i + 2 + len,
                });
            }
            0xD9 => return None,
            _ => {}
        }
        i += 2 + len;
    }
}

/// Offsets of restart markers in the scan and of the EOI marker. Fails on a second scan.
fn restart_markers(jpeg: &[u8], scan_start: usize) -> Option<(Vec<usize>, usize)> {
    let mut markers = Vec::new();
    let mut i = scan_start;
    while i + 1 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        match jpeg[i + 1] {
            0x00 | 0xFF => i += 1,
            0xD0..=0xD7 => {
                markers.push(i);
                i += 2;
            }
            0xD9 => return Some((markers, i)),
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_jpegs_without_restart_markers() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xD9];
        assert!(split_jpeg(&jpeg, 4).is_none());
    }
}
