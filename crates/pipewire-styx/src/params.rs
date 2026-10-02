//! PipeWire params for Styx offers: `EnumFormat` per format and size, and `Buffers`.

use pipewire::spa;
use spa::param::video::VideoFormat;
use spa::pod::{Object, Pod, Property, Value};
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

/// One `EnumFormat` pod per offer.
pub fn enum_format_pods(offers: &[Offer]) -> Vec<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    offers
        .iter()
        .filter_map(|offer| {
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
            Some(serialize(Object {
                type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                id: spa::param::ParamType::EnumFormat.as_raw(),
                properties: vec![
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
                ],
            }))
        })
        .collect()
}

/// The `Buffers` param for frames of `size` bytes with rows of `stride` bytes.
pub fn buffers_pod(size: u32, stride: u32) -> Vec<u8> {
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
                        default: 4,
                        min: 2,
                        max: 8,
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
