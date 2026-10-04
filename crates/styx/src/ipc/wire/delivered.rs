//! What a plan's frames are, in the camera service's accept message: format, size, rate,
//! pyramid, inter-coding, region of interest, overview and unmet requirements.

use styx_core::prelude::*;

use super::{IpcError, Reader, Writer};
use crate::planner::{Delivered, MAX_REGIONS, RoiCrop, Unmet};

/// Most unmet requirements an accept message carries.
const MAX_UNMET: u8 = 8;

fn crop_code(crop: RoiCrop) -> u8 {
    match crop {
        RoiCrop::Isp => 1,
        RoiCrop::View => 2,
        RoiCrop::IspPass => 3,
    }
}

fn read_crop(r: &mut Reader<'_>) -> Result<RoiCrop, IpcError> {
    match r.u8()? {
        1 => Ok(RoiCrop::Isp),
        2 => Ok(RoiCrop::View),
        3 => Ok(RoiCrop::IspPass),
        _ => Err(IpcError::Malformed("unknown region crop")),
    }
}

pub(super) fn write_delivered(w: &mut Writer, d: &Delivered) {
    w.u32(d.format.to_u32());
    w.size(d.size);
    w.opt(d.fps, |w, fps| w.u32(fps.to_bits()));
    w.u8(d.pyramid_levels);
    w.opt(d.hardware_pyramid_level, Writer::u8);
    w.bool(d.inter_coded);
    w.opt(d.roi, |w, roi| w.u8(crop_code(roi)));
    let regions = &d.regions[..d.regions.len().min(MAX_REGIONS)];
    w.u8(regions.len() as u8);
    for crop in regions {
        w.opt(*crop, |w, crop| w.u8(crop_code(crop)));
    }
    w.opt(d.overview, Writer::size);
    w.bool(d.hardware_overview);
    w.u8(d.unmet.len().min(usize::from(MAX_UNMET)) as u8);
    for unmet in d.unmet.iter().take(usize::from(MAX_UNMET)) {
        match unmet {
            Unmet::Size { wanted, delivered } => {
                w.u8(1);
                w.size(*wanted);
                w.size(*delivered);
            }
            Unmet::Roi => w.u8(2),
            Unmet::Overview { wanted, delivered } => {
                w.u8(3);
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
    let roi = r.opt(read_crop)?;
    let count = usize::from(r.u8()?);
    if count > MAX_REGIONS {
        return Err(IpcError::Malformed("too many regions"));
    }
    let regions = (0..count)
        .map(|_| r.opt(read_crop))
        .collect::<Result<_, IpcError>>()?;
    let overview = r.opt(Reader::size)?;
    let hardware_overview = r.bool()?;
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
            2 => Ok(Unmet::Roi),
            3 => Ok(Unmet::Overview {
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
        roi,
        regions,
        overview,
        hardware_overview,
        unmet,
    })
}
