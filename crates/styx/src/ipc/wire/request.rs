//! A client's `FrameRequest` in the camera service's request message.

use styx_core::prelude::*;

use super::{IpcError, KIND_REQUEST, MAX_NAMES, Reader, Writer, read_regions};
use crate::ipc::ClientPriority;
use crate::planner::{Delivery, FrameRate, FrameRequest, Hardware};

/// Tag of the request's priority trailer: older services stop reading before it, so a
/// low-priority request reaches them as a normal one.
const PRIORITY_TAG: u8 = 1;

pub(in crate::ipc) fn encode_request(
    req: &FrameRequest,
    camera: Option<&str>,
    priority: ClientPriority,
) -> Vec<u8> {
    let mut w = Writer::new(KIND_REQUEST);
    w.opt(camera, Writer::text);
    match &req.format {
        OutputFormat::Luma => w.u8(0),
        OutputFormat::Formats(formats) => {
            w.u8(1);
            w.u8(formats.len().min(usize::from(MAX_NAMES)) as u8);
            for code in formats.iter().take(usize::from(MAX_NAMES)) {
                w.u32(code.to_u32());
            }
        }
        OutputFormat::Any => w.u8(2),
    }
    w.opt(req.size, Writer::size);
    w.opt(req.min_size, Writer::size);
    w.opt(req.max_size, Writer::size);
    match req.fps {
        FrameRate::CameraDefault => w.u8(0),
        FrameRate::Exactly(fps) => {
            w.u8(1);
            w.u32(fps);
        }
        FrameRate::AtLeast(fps) => {
            w.u8(2);
            w.u32(fps);
        }
        FrameRate::Between(min, max) => {
            w.u8(3);
            w.u32(min);
            w.u32(max);
        }
    }
    match req.delivery {
        Delivery::Latest => w.u8(0),
        Delivery::EveryFrame(n) => {
            w.u8(1);
            w.usize(n);
        }
    }
    w.opt(req.pyramid, |w, p| {
        w.u8(p.levels);
        w.u8(match p.source {
            PyramidSource::PreferHardware => 0,
            PyramidSource::HardwareOnly => 1,
            PyramidSource::Software => 2,
        });
    });
    w.opt(req.roi, Writer::rect);
    let extra = &req.extra_regions[..req.extra_regions.len().min(crate::planner::MAX_REGIONS - 1)];
    w.u8(extra.len() as u8);
    for rect in extra {
        w.rect(*rect);
    }
    w.bool(req.skip_stale_regions);
    w.opt(req.overview, Writer::size);
    w.opt(req.row_alignment, Writer::usize);
    w.opt(req.backend.map(|b| b.to_string()).as_deref(), Writer::text);
    w.u8(match req.hardware {
        Hardware::Auto => 0,
        Hardware::Required => 1,
        Hardware::Off => 2,
    });
    w.opt(req.decoder.as_deref(), Writer::text);
    w.u8(req.forbid.len().min(usize::from(MAX_NAMES)) as u8);
    for name in req.forbid.iter().take(usize::from(MAX_NAMES)) {
        w.text(name);
    }
    w.opt(req.decode_threads, Writer::usize);
    w.bool(req.strict);
    // Normal requests stay as they were: no trailer.
    if priority == ClientPriority::Low {
        w.u8(PRIORITY_TAG);
        w.u8(1);
    }
    w.0
}

/// The priority trailer after a request (none: normal; an unknown tag is ignored).
pub(super) fn read_priority(r: &mut Reader<'_>) -> Result<ClientPriority, IpcError> {
    if r.0.is_empty() || r.u8()? != PRIORITY_TAG {
        return Ok(ClientPriority::Normal);
    }
    Ok(match r.u8()? {
        0 => ClientPriority::Normal,
        _ => ClientPriority::Low,
    })
}

pub(super) fn read_request(r: &mut Reader<'_>) -> Result<FrameRequest, IpcError> {
    let mut req = match r.u8()? {
        0 => FrameRequest::new(OutputFormat::Luma),
        1 => {
            let count = r.u8()?;
            if count > MAX_NAMES {
                return Err(IpcError::Malformed("too many formats"));
            }
            let formats = (0..count)
                .map(|_| Ok(FourCc::new(r.u32()?.to_le_bytes())))
                .collect::<Result<Vec<_>, IpcError>>()?;
            FrameRequest::formats(formats)
        }
        2 => FrameRequest::new(OutputFormat::Any),
        _ => return Err(IpcError::Malformed("unknown output")),
    };
    req.size = r.opt(Reader::size)?;
    req.min_size = r.opt(Reader::size)?;
    req.max_size = r.opt(Reader::size)?;
    req.fps = match r.u8()? {
        0 => FrameRate::CameraDefault,
        1 => FrameRate::Exactly(r.u32()?),
        2 => FrameRate::AtLeast(r.u32()?),
        3 => FrameRate::Between(r.u32()?, r.u32()?),
        _ => return Err(IpcError::Malformed("unknown frame rate")),
    };
    req.delivery = match r.u8()? {
        0 => Delivery::Latest,
        1 => Delivery::EveryFrame(r.usize()?),
        _ => return Err(IpcError::Malformed("unknown delivery")),
    };
    req.pyramid = r.opt(|r| {
        Ok(PyramidRequest {
            levels: r.u8()?,
            source: match r.u8()? {
                1 => PyramidSource::HardwareOnly,
                2 => PyramidSource::Software,
                _ => PyramidSource::PreferHardware,
            },
        })
    })?;
    req.roi = r.opt(Reader::rect)?;
    req.extra_regions = read_regions(r, crate::planner::MAX_REGIONS - 1)?;
    req.skip_stale_regions = r.bool()?;
    req.overview = r.opt(Reader::size)?;
    req.row_alignment = r.opt(Reader::usize)?;
    req.backend = r
        .opt(Reader::text)?
        .map(|name| name.parse())
        .transpose()
        .map_err(|_| IpcError::Malformed("unknown backend"))?;
    req.hardware = match r.u8()? {
        1 => Hardware::Required,
        2 => Hardware::Off,
        _ => Hardware::Auto,
    };
    req.decoder = r.opt(Reader::text)?;
    let forbidden = r.u8()?;
    if forbidden > MAX_NAMES {
        return Err(IpcError::Malformed("too many forbidden codecs"));
    }
    req.forbid = (0..forbidden)
        .map(|_| r.text())
        .collect::<Result<_, IpcError>>()?;
    req.decode_threads = r.opt(Reader::usize)?;
    req.strict = r.bool()?;
    Ok(req)
}
