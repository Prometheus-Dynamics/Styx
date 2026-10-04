//! What a plan's frames are, in the camera service's accept message: format, size, rate,
//! pyramid, inter-coding and unmet requirements.

use styx_core::prelude::*;

use super::{IpcError, Reader, Writer};
use crate::planner::{Delivered, Unmet};

/// Most unmet requirements an accept message carries.
const MAX_UNMET: u8 = 8;

pub(super) fn write_delivered(w: &mut Writer, d: &Delivered) {
    w.u32(d.format.to_u32());
    w.size(d.size);
    w.opt(d.fps, |w, fps| w.u32(fps.to_bits()));
    w.u8(d.pyramid_levels);
    w.opt(d.hardware_pyramid_level, Writer::u8);
    w.bool(d.inter_coded);
    w.u8(d.unmet.len().min(usize::from(MAX_UNMET)) as u8);
    for unmet in d.unmet.iter().take(usize::from(MAX_UNMET)) {
        match unmet {
            Unmet::Size { wanted, delivered } => {
                w.u8(1);
                w.size(*wanted);
                w.size(*delivered);
            }
        }
    }
}

pub(super) fn read_delivered(r: &mut Reader<'_>) -> Result<Delivered, IpcError> {
    let format = FourCc::new(r.u32()?.to_le_bytes());
    let size = r.size()?;
    let fps = r.opt(|r| Ok(f32::from_bits(r.u32()?)))?;
    let pyramid_levels = r.u8()?;
    let hardware_pyramid_level = r.opt(Reader::u8)?;
    let inter_coded = r.bool()?;
    let count = r.u8()?;
    if count > MAX_UNMET {
        return Err(IpcError::Malformed("too many unmet requirements"));
    }
    let unmet = (0..count)
        .map(|_| match r.u8()? {
            1 => Ok(Unmet::Size {
                wanted: r.size()?,
                delivered: r.size()?,
            }),
            _ => Err(IpcError::Malformed("unknown unmet requirement")),
        })
        .collect::<Result<_, IpcError>>()?;
    Ok(Delivered {
        format,
        size,
        fps,
        pyramid_levels,
        hardware_pyramid_level,
        inter_coded,
        unmet,
    })
}
