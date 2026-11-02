//! Converters (`CodecKind::Converter`, e.g. the Rockchip RGA) in route choice: taken for
//! raw-to-raw steps like a decoder, never as an encoder.

use std::sync::Arc;

use styx_codec::{Codec, CodecDescriptor, CodecError, CodecKind, CodecRegistry};
use styx_core::prelude::*;

use super::request::FrameRequest;
use super::routes::{encode_route, pick_decoder};

struct Fake(CodecDescriptor);

impl Codec for Fake {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.0
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        Ok(input)
    }
}

fn fake(kind: CodecKind, input: FourCc, output: FourCc, impl_name: &'static str) -> Arc<Fake> {
    Arc::new(Fake(CodecDescriptor {
        kind,
        input,
        output,
        name: "fake",
        impl_name,
    }))
}

#[test]
fn a_converter_turns_raw_frames_into_the_requested_format() {
    let registry = CodecRegistry::new();
    registry.register(
        FourCc::NV12,
        fake(CodecKind::Converter, FourCc::NV12, FourCc::BGRA, "rga"),
    );
    let req = FrameRequest::formats([FourCc::BGRA]);
    let (codec, target) = pick_decoder(FourCc::NV12, &req, &registry.handle(), false).unwrap();
    assert_eq!(target, FourCc::BGRA);
    assert_eq!(codec.descriptor().kind, CodecKind::Converter);
}

#[test]
fn encoder_lookups_never_take_a_converter() {
    let registry = CodecRegistry::new();
    // A converter whose output is a compressed format still is no encoder.
    registry.register(
        FourCc::NV12,
        fake(CodecKind::Converter, FourCc::NV12, FourCc::MJPG, "rga"),
    );
    let req = FrameRequest::formats([FourCc::MJPG]);
    let mut steps = Vec::new();
    assert!(encode_route(FourCc::NV12, &req, &registry.handle(), 1.0, &mut steps).is_err());

    // With an encoder from RG24, the converter is the step before it.
    registry.register(
        FourCc::NV12,
        fake(CodecKind::Converter, FourCc::NV12, FourCc::RG24, "rga"),
    );
    registry.register(
        FourCc::RG24,
        fake(CodecKind::Encoder, FourCc::RG24, FourCc::MJPG, "soft-jpeg"),
    );
    let route = encode_route(FourCc::NV12, &req, &registry.handle(), 1.0, &mut steps)
        .unwrap()
        .expect("a route");
    let super::routes::Route::Encode {
        decoder, encoder, ..
    } = route
    else {
        panic!("not an encode route");
    };
    assert_eq!(decoder.unwrap().descriptor().kind, CodecKind::Converter);
    assert_eq!(encoder.descriptor().kind, CodecKind::Encoder);
}

#[test]
fn converter_kind_parses_and_converts() {
    assert_eq!(
        "converter".parse::<CodecKind>().unwrap(),
        CodecKind::Converter
    );
    assert!(CodecKind::Converter.converts() && CodecKind::Decoder.converts());
    assert!(!CodecKind::Encoder.converts());
}
