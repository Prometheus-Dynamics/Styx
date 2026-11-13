//! Selection order, the reasons every candidate gets, and the error for a missing implementation.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use styx_core::{
    Backend, Candidate, Codec, CodecDescriptor, CodecError, CodecKind, Encoder, FourCc, FrameLease,
    Implementation, Registry, Request, SelectError, Verdict,
};

/// A do-nothing encoder: selection is what these tests look at.
struct Stub {
    descriptor: CodecDescriptor,
}

impl Codec for Stub {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl Encoder for Stub {
    fn encode(&mut self, _input: &FrameLease, _output: &mut FrameLease) -> Result<(), CodecError> {
        Ok(())
    }
}

fn desc(
    kind: CodecKind,
    input: FourCc,
    output: FourCc,
    backend: Backend,
    name: &'static str,
) -> CodecDescriptor {
    CodecDescriptor {
        kind,
        input,
        output,
        backend,
        name,
    }
}

/// A candidate whose constructor builds a [`Stub`] with its own descriptor.
fn ok(descriptor: CodecDescriptor) -> Candidate {
    let built = descriptor.clone();
    Candidate::new(descriptor, move || {
        Ok(Implementation::Encoder(Box::new(Stub {
            descriptor: built.clone(),
        })))
    })
}

/// A candidate that cannot run on this machine.
fn unavailable(descriptor: CodecDescriptor, why: &'static str) -> Candidate {
    Candidate::new(descriptor, move || {
        Err(CodecError::Unavailable(why.to_string()))
    })
}

fn yuyv_to_mjpg() -> Request {
    Request::new(CodecKind::Encoder, FourCc::YUYV, FourCc::MJPG)
}

#[test]
fn native_is_tried_before_ffmpeg_even_when_registered_after_it() {
    let mut registry = Registry::new();
    registry.register(ok(desc(
        CodecKind::Encoder,
        FourCc::YUYV,
        FourCc::MJPG,
        Backend::Ffmpeg,
        "ffmpeg-mjpeg",
    )));
    registry.register(ok(desc(
        CodecKind::Encoder,
        FourCc::YUYV,
        FourCc::MJPG,
        Backend::Native,
        "styx-jpeg",
    )));

    let selection = registry.select(yuyv_to_mjpg()).unwrap();
    assert_eq!(selection.implementation().descriptor().name, "styx-jpeg");
    assert_eq!(selection.decisions().len(), 2);
    assert_eq!(selection.decisions()[0].descriptor.name, "styx-jpeg");
    assert_eq!(selection.decisions()[0].verdict, Verdict::Chosen);
    assert_eq!(selection.decisions()[1].descriptor.name, "ffmpeg-mjpeg");
    assert_eq!(selection.decisions()[1].verdict, Verdict::NotTried);
}

#[test]
fn ffmpeg_is_chosen_with_the_reason_each_native_candidate_was_skipped() {
    let mut registry = Registry::new();
    // Native, wrong formats.
    registry.register(ok(desc(
        CodecKind::Encoder,
        FourCc::GREY,
        FourCc::H264,
        Backend::Native,
        "styx-h264",
    )));
    // Native, right formats, cannot run here.
    registry.register(unavailable(
        desc(
            CodecKind::Encoder,
            FourCc::YUYV,
            FourCc::MJPG,
            Backend::Native,
            "rkmpp-jpeg",
        ),
        "no encoder device",
    ));
    // Native, right formats, wrong kind.
    registry.register(ok(desc(
        CodecKind::Decoder,
        FourCc::YUYV,
        FourCc::MJPG,
        Backend::Native,
        "wrong-kind",
    )));
    registry.register(ok(desc(
        CodecKind::Encoder,
        FourCc::YUYV,
        FourCc::MJPG,
        Backend::Ffmpeg,
        "ffmpeg-mjpeg",
    )));

    let selection = registry.select(yuyv_to_mjpg()).unwrap();
    assert_eq!(selection.implementation().descriptor().name, "ffmpeg-mjpeg");
    assert_eq!(selection.implementation().kind(), CodecKind::Encoder);

    let verdicts: Vec<(&str, &Verdict)> = selection
        .decisions()
        .iter()
        .map(|d| (d.descriptor.name, &d.verdict))
        .collect();
    assert_eq!(
        verdicts,
        vec![
            (
                "styx-h264",
                &Verdict::Skipped("it converts GREY to H264, not YUYV to MJPG".to_string()),
            ),
            (
                "rkmpp-jpeg",
                &Verdict::Skipped("unavailable: no encoder device".to_string()),
            ),
            (
                "wrong-kind",
                &Verdict::Skipped("its kind is decoder, not encoder".to_string()),
            ),
            ("ffmpeg-mjpeg", &Verdict::Chosen),
        ]
    );
    // The reasons print as data a caller can log.
    let line = selection.decisions()[1].to_string();
    assert_eq!(
        line,
        "native rkmpp-jpeg (encoder): skipped, unavailable: no encoder device"
    );
}

#[test]
fn only_matching_candidates_are_constructed_and_construction_stops_at_the_first_success() {
    let built = Arc::new(AtomicUsize::new(0));
    let counted = |descriptor: CodecDescriptor| {
        let built = built.clone();
        let stub = descriptor.clone();
        Candidate::new(descriptor, move || {
            built.fetch_add(1, Ordering::SeqCst);
            Ok(Implementation::Encoder(Box::new(Stub {
                descriptor: stub.clone(),
            })))
        })
    };
    let mut registry = Registry::new();
    registry.register(counted(desc(
        CodecKind::Encoder,
        FourCc::GREY,
        FourCc::H264,
        Backend::Native,
        "other-format",
    )));
    registry.register(counted(desc(
        CodecKind::Encoder,
        FourCc::YUYV,
        FourCc::MJPG,
        Backend::Native,
        "native",
    )));
    registry.register(counted(desc(
        CodecKind::Encoder,
        FourCc::YUYV,
        FourCc::MJPG,
        Backend::Ffmpeg,
        "ffmpeg",
    )));

    registry.select(yuyv_to_mjpg()).unwrap();
    assert_eq!(built.load(Ordering::SeqCst), 1);
}

#[test]
fn a_constructor_that_builds_another_implementation_is_skipped() {
    let mut registry = Registry::new();
    let lying = Candidate::new(
        desc(
            CodecKind::Encoder,
            FourCc::YUYV,
            FourCc::MJPG,
            Backend::Native,
            "claims-jpeg",
        ),
        || {
            Ok(Implementation::Encoder(Box::new(Stub {
                descriptor: desc(
                    CodecKind::Encoder,
                    FourCc::YUYV,
                    FourCc::H264,
                    Backend::Native,
                    "h264-inside",
                ),
            })))
        },
    );
    registry.register(lying);
    let error = registry.select(yuyv_to_mjpg()).unwrap_err();
    assert_eq!(
        error.decisions[0].verdict,
        Verdict::Skipped("its constructor built h264-inside instead".to_string())
    );
}

#[test]
fn a_missing_implementation_fails_and_names_the_formats() {
    let mut registry = Registry::new();
    registry.register(ok(desc(
        CodecKind::Encoder,
        FourCc::GREY,
        FourCc::H264,
        Backend::Native,
        "styx-h264",
    )));
    registry.register(unavailable(
        desc(
            CodecKind::Encoder,
            FourCc::YUYV,
            FourCc::MJPG,
            Backend::Ffmpeg,
            "ffmpeg-mjpeg",
        ),
        "FFmpeg not built in",
    ));

    let error = registry.select(yuyv_to_mjpg()).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("encoder YUYV to MJPG"), "{text}");
    assert!(
        text.contains("ffmpeg ffmpeg-mjpeg (encoder): skipped, unavailable: FFmpeg not built in"),
        "{text}"
    );
    assert_eq!(error.decisions.len(), 2);
    assert_eq!(error.request, yuyv_to_mjpg());
}

#[test]
fn an_empty_registry_fails_and_names_the_formats() {
    let registry: Registry = Registry::new();
    let error: SelectError = registry
        .select(Request::new(CodecKind::Decoder, FourCc::MJPG, FourCc::YUYV))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "no decoder MJPG to YUYV available: none registered"
    );
}
