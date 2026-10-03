use super::*;

struct TestCodec {
    descriptor: CodecDescriptor,
}

impl TestCodec {
    fn decoder(impl_name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::MJPG,
                output: FourCc::RG24,
                name: "mjpeg",
                impl_name,
            },
        })
    }
}

impl Codec for TestCodec {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, crate::CodecError> {
        Ok(input)
    }
}

#[test]
fn policy_ordered_impls_normalize_to_typed_ids() {
    let policy = CodecPolicy::builder(FourCc::MJPG)
        .ordered_impls([" SOFT-CPU "])
        .build();

    assert_eq!(
        policy.ordered_impls[0],
        CodecImplementationId::new("soft-cpu")
    );
}

#[test]
fn auto_lookup_uses_policy_priority_before_hardware_bias() {
    let registry = CodecRegistry::new();
    registry.register(FourCc::MJPG, TestCodec::decoder("h264-v4l2m2m"));
    registry.register(FourCc::MJPG, TestCodec::decoder("soft-cpu"));
    let handle = registry.handle();

    assert_eq!(
        handle
            .lookup_auto(FourCc::MJPG)
            .unwrap()
            .descriptor()
            .impl_name,
        "h264-v4l2m2m"
    );

    handle.set_policy(
        CodecPolicy::builder(FourCc::MJPG)
            .prefer_hardware(false)
            .priority(" SOFT-CPU ", 0)
            .build(),
    );

    assert_eq!(
        handle
            .lookup_auto(FourCc::MJPG)
            .unwrap()
            .descriptor()
            .impl_name,
        "soft-cpu"
    );
}

#[test]
fn preference_accepts_ergonomic_strings_but_stores_typed_ids() {
    let preference = Preference::hardware_biased([" SOFT-CPU ", "h264-v4l2m2m"]);

    assert_eq!(
        preference.impls,
        vec![
            CodecImplementationId::new("soft-cpu"),
            CodecImplementationId::new("h264-v4l2m2m"),
        ]
    );
    assert!(preference.prefer_hardware);
}

#[test]
fn preferred_lookup_accepts_typed_impl_ids() {
    let registry = CodecRegistry::new();
    registry.register(FourCc::MJPG, TestCodec::decoder("h264-v4l2m2m"));
    registry.register(FourCc::MJPG, TestCodec::decoder("soft-cpu"));
    let handle = registry.handle();

    let codec = handle
        .lookup_preferred_ids(
            FourCc::MJPG,
            &[CodecImplementationId::new(" SOFT-CPU ")],
            true,
        )
        .unwrap();

    assert_eq!(codec.descriptor().impl_name, "soft-cpu");
}

#[test]
fn codec_registry_config_sanitizes_dimensions() {
    assert_eq!(
        CodecRegistryConfig::new(0, 720),
        CodecRegistryConfig {
            max_width: 1,
            max_height: 720,
        }
    );
}

fn codec(kind: CodecKind, output: FourCc, impl_name: &'static str) -> Arc<TestCodec> {
    Arc::new(TestCodec {
        descriptor: CodecDescriptor {
            kind,
            input: FourCc::YUYV,
            output,
            name: "test",
            impl_name,
        },
    })
}

#[test]
fn lookups_without_a_kind_pick_a_decoder_over_an_encoder() {
    // As registered with FFmpeg: YUYV has encoders ("ffmpeg" sorts before "yuyv-cpu") and a
    // decoder. HeliOS's `process(YUYV, frame)` got the MJPEG encoder.
    let registry = CodecRegistry::new();
    registry.register(
        FourCc::YUYV,
        codec(CodecKind::Encoder, FourCc::MJPG, "ffmpeg"),
    );
    registry.register(
        FourCc::YUYV,
        codec(CodecKind::Encoder, FourCc::H264, "h264-v4l2m2m"),
    );
    registry.register(
        FourCc::YUYV,
        codec(CodecKind::Decoder, FourCc::RG24, "yuyv-cpu"),
    );
    let handle = registry.handle();
    let out = |c: Arc<dyn Codec>| c.descriptor().output;
    assert_eq!(out(handle.lookup(FourCc::YUYV).unwrap()), FourCc::RG24);
    assert_eq!(out(handle.lookup_auto(FourCc::YUYV).unwrap()), FourCc::RG24);
    assert_eq!(
        out(handle.lookup_preferred(FourCc::YUYV, &[], true).unwrap()),
        FourCc::RG24
    );
    assert_eq!(
        out(handle
            .lookup_preferred_kind(FourCc::YUYV, CodecKind::Encoder, true)
            .unwrap()),
        FourCc::H264
    );
    assert_eq!(
        out(handle
            .lookup_preferred_kind(FourCc::YUYV, CodecKind::Decoder, true)
            .unwrap()),
        FourCc::RG24
    );
    assert!(
        handle
            .lookup_preferred_kind(FourCc::MJPG, CodecKind::Decoder, false)
            .is_err()
    );
}
