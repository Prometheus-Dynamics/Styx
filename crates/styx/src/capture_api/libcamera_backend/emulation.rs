use styx_codec::Codec;
use styx_codec::prelude::{Nv12ToBgrDecoder, Nv12ToRgbDecoder, YuyvToRgbDecoder};
use styx_core::prelude::{FourCc, FrameLease, Resolution};

use crate::capture_api::CaptureError;

pub(super) enum Emulation {
    Nv12ToRgb(Nv12ToRgbDecoder),
    Nv12ToBgr(Nv12ToBgrDecoder),
    YuyvToRgb(YuyvToRgbDecoder),
}

impl Emulation {
    pub(super) fn for_request(
        enabled: bool,
        validated_code: FourCc,
        requested_code: FourCc,
        validated_res: Resolution,
    ) -> Option<Self> {
        if !enabled {
            return None;
        }

        match (
            &validated_code.to_u32().to_le_bytes(),
            &requested_code.to_u32().to_le_bytes(),
        ) {
            (b"NV12", b"RG24") => Some(Self::Nv12ToRgb(Nv12ToRgbDecoder::new(
                validated_res.width.get(),
                validated_res.height.get(),
            ))),
            (b"NV12", b"BG24") => Some(Self::Nv12ToBgr(Nv12ToBgrDecoder::new(
                validated_res.width.get(),
                validated_res.height.get(),
            ))),
            (b"YUYV", b"RG24") => Some(Self::YuyvToRgb(YuyvToRgbDecoder::new(
                validated_res.width.get(),
                validated_res.height.get(),
            ))),
            _ => None,
        }
    }

    pub(super) fn process(&self, frame: FrameLease) -> Result<FrameLease, CaptureError> {
        match self {
            Self::Nv12ToRgb(dec) => dec.process(frame).map_err(|err| {
                CaptureError::Backend(format!("nv12->rgb conversion failed: {err}"))
            }),
            Self::Nv12ToBgr(dec) => dec.process(frame).map_err(|err| {
                CaptureError::Backend(format!("nv12->bgr conversion failed: {err}"))
            }),
            Self::YuyvToRgb(dec) => dec.process(frame).map_err(|err| {
                CaptureError::Backend(format!("yuyv->rgb conversion failed: {err}"))
            }),
        }
    }
}
