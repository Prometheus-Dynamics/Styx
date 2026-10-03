//! PipeWire params for Styx offers: `EnumFormat` per format and size, and `Buffers`.
//!
//! Each offer is listed plain (buffers are memfds every consumer maps) and, when the node can
//! make dma-bufs, again with the linear DRM modifier (`SPA_FORMAT_VIDEO_modifier`, mandatory) for
//! consumers that ask for dma-bufs, such as `pipewiresrc` with `memory:DMABuf` caps.

use pipewire::spa;
use spa::param::video::VideoFormat;
use spa::pod::{Object, Pod, Property, PropertyFlags, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle};
use styx::prelude::FourCc;

use crate::formats::Offer;

const MAP: [(FourCc, VideoFormat); 9] = [
    (FourCc::YUYV, VideoFormat::YUY2),
    (FourCc::UYVY, VideoFormat::UYVY),
    (FourCc::NV12, VideoFormat::NV12),
    (FourCc::YU12, VideoFormat::I420),
    (FourCc::RG24, VideoFormat::RGB),
    (FourCc::BG24, VideoFormat::BGR),
    (FourCc::RGBA, VideoFormat::RGBA),
    (FourCc::BGRA, VideoFormat::BGRA),
    (FourCc::GREY, VideoFormat::GRAY8),
];

pub fn spa_format(code: FourCc) -> Option<VideoFormat> {
    MAP.iter().find(|(c, _)| *c == code).map(|(_, f)| *f)
}

pub fn styx_format(format: VideoFormat) -> Option<FourCc> {
    MAP.iter().find(|(_, f)| *f == format).map(|(c, _)| *c)
}

fn serialize(object: Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .expect("pod serializes")
    .0
    .into_inner()
}

fn id_property(key: u32, id: u32) -> Property {
    Property::new(key, Value::Id(Id(id)))
}

/// `DRM_FORMAT_MOD_LINEAR`.
const MOD_LINEAR: i64 = 0;

/// One `EnumFormat` pod per offer, and with `dmabuf` one more per offer with the linear
/// modifier.
pub fn enum_format_pods(offers: &[Offer], dmabuf: bool) -> Vec<Vec<u8>> {
    let modifiers: &[bool] = if dmabuf { &[false, true] } else { &[false] };
    modifiers
        .iter()
        .flat_map(|&modifier| offers.iter().filter_map(move |o| format_pod(o, modifier)))
        .collect()
}

fn format_pod(offer: &Offer, modifier: bool) -> Option<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    let format = spa_format(offer.fourcc)?;
    let size = Rectangle {
        width: offer.width,
        height: offer.height,
    };
    let rates: Vec<Fraction> = offer
        .rates
        .iter()
        .map(|&(num, denom)| Fraction { num, denom })
        .collect();
    let framerate = match rates.as_slice() {
        [] => Value::Choice(spa::pod::ChoiceValue::Fraction(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Range {
                default: Fraction { num: 30, denom: 1 },
                min: Fraction { num: 0, denom: 1 },
                max: Fraction {
                    num: 1000,
                    denom: 1,
                },
            },
        ))),
        [one] => Value::Fraction(*one),
        [first, ..] => Value::Choice(spa::pod::ChoiceValue::Fraction(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Enum {
                default: *first,
                alternatives: rates.clone(),
            },
        ))),
    };
    let mut properties = vec![
        id_property(
            FormatProperties::MediaType.as_raw(),
            MediaType::Video.as_raw(),
        ),
        id_property(
            FormatProperties::MediaSubtype.as_raw(),
            MediaSubtype::Raw.as_raw(),
        ),
        id_property(FormatProperties::VideoFormat.as_raw(), format.as_raw()),
        Property::new(FormatProperties::VideoSize.as_raw(), Value::Rectangle(size)),
        Property::new(FormatProperties::VideoFramerate.as_raw(), framerate),
    ];
    if modifier {
        properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY,
            value: Value::Long(MOD_LINEAR),
        });
    }
    Some(serialize(Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties,
    }))
}

/// Buffers the node allocates (consumers return them one frame later, the camera needs a few
/// queued).
const BUFFERS: (i32, i32, i32) = (6, 2, 16);

/// The `Buffers` param for frames of `size` bytes with rows of `stride` bytes, in dma-bufs or
/// memfds (the node allocates them either way).
pub fn buffers_pod(size: u32, stride: u32, dmabuf: bool) -> Vec<u8> {
    let data_type = if dmabuf {
        1 << spa::sys::SPA_DATA_DmaBuf
    } else {
        1 << spa::sys::SPA_DATA_MemFd
    };
    let int = |v: u32| Value::Int(v as i32);
    serialize(Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(spa::pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: BUFFERS.0,
                        min: BUFFERS.1,
                        max: BUFFERS.2,
                    },
                ))),
            ),
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(spa::pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: data_type,
                        flags: Vec::new(),
                    },
                ))),
            ),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, int(1)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_size, int(size)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, int(stride)),
        ],
    })
}

/// View serialized pods as the `&Pod`s PipeWire calls take.
pub fn as_pods(bytes: &[Vec<u8>]) -> Vec<&Pod> {
    bytes.iter().filter_map(|b| Pod::from_bytes(b)).collect()
}
