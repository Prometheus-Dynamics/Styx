use super::*;

#[test]
fn framelease_type_registration_uses_stable_key() {
    register_framelease_type();
    let type_expr = daedalus::data::typing::type_expr::<FrameLease>();
    let key = daedalus::runtime::transport::typeexpr_transport_key(&type_expr).unwrap();
    assert_eq!(key, framelease_type_key());
}

#[test]
fn framelease_payload_preserves_frame_without_plane_copy() {
    let frame = test_frame();
    let bytes = frame.payload_bytes();
    let payload = framelease_payload(frame);

    assert_eq!(payload.type_key(), &framelease_type_key());
    assert_eq!(payload.residency(), daedalus::transport::Residency::Cpu);
    assert_eq!(payload.bytes_estimate(), Some(bytes as u64));
    assert!(payload.get_ref::<FrameLease>().is_some());
}

#[test]
fn framelease_round_trips_through_preview_tap_without_copy() {
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let tap = register_preview_node(&mut registry, "styx.test.tap", |_| {})
        .expect("install tap")
        .alias("tap");
    let graph = registry
        .graph_builder()
        .expect("graph builder")
        .inputs(|g| {
            g.input("frame");
        })
        .outputs(|g| {
            g.output("frame");
        })
        .nodes(|g| {
            g.add_handle(&tap);
        })
        .edges(|g| {
            g.connect("frame", &tap.input("frame"));
            g.connect(&tap.output("frame"), "frame");
        })
        .build();

    let engine = daedalus::engine::Engine::new(
        daedalus::engine::EngineConfig::from(daedalus::engine::GpuBackend::Cpu)
            .with_metrics_level(daedalus::engine::MetricsLevel::Detailed),
    )
    .expect("engine");
    let mut runtime = engine
        .compile_registry(&registry, graph)
        .expect("compile graph");
    runtime.push_payload("frame", framelease_payload(test_frame()));
    let telemetry = runtime
        .tick_until_idle()
        .expect("tick graph")
        .expect("telemetry");
    let output = runtime
        .drain_owned::<FrameLease>("frame")
        .expect("drain frame");

    assert_eq!(output.len(), 1);
    let copied_bytes = telemetry
        .edge_metrics
        .values()
        .map(|metrics| metrics.copied_bytes)
        .sum::<u64>();
    assert_eq!(copied_bytes, 0);
}

#[test]
fn media_policy_presets_map_to_daedalus_policies() {
    assert!(latest_only().is_latest_only());

    let recording = bounded_blocking(8);
    assert!(matches!(
        recording.pressure,
        daedalus::transport::PressurePolicy::Bounded {
            capacity: 8,
            overflow: daedalus::transport::OverflowPolicy::Backpressure,
        }
    ));
    assert_eq!(
        recording.freshness,
        daedalus::transport::FreshnessPolicy::PreserveAll
    );

    let analysis = bounded_drop_oldest(2, 4);
    assert!(matches!(
        analysis.pressure,
        daedalus::transport::PressurePolicy::Bounded {
            capacity: 2,
            overflow: daedalus::transport::OverflowPolicy::DropOldest,
        }
    ));
    assert_eq!(
        analysis.freshness,
        daedalus::transport::FreshnessPolicy::MaxLag { frames: 4 }
    );
}

#[test]
fn media_plugin_installs_concrete_codec_nodes() {
    let codec = Arc::new(PassthroughDecoder::new(FourCc::RG24));
    let node_id = concrete_codec_node_id(codec.descriptor());
    let plugin = StyxMediaPlugin::new().with_codec(codec, codec_node_options());
    let descriptors = plugin.codec_descriptors();
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();

    registry.install(&plugin).expect("install plugin");

    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].node_id, node_id);
    assert_eq!(descriptors[0].kind, CodecKind::Decoder);
    assert_eq!(descriptors[0].input, "RG24");
    assert_eq!(descriptors[0].output, "RG24");
    assert_eq!(descriptors[0].implementation, "passthrough");
    let snapshot = registry.transport_capabilities.snapshot();
    assert!(
        snapshot
            .nodes
            .iter()
            .any(|decl| decl.id.to_string() == node_id)
    );
    let manifest = registry
        .plugin_manifests
        .get(plugin.id())
        .expect("plugin manifest");
    assert!(
        manifest
            .provided_nodes
            .iter()
            .any(|id| id.to_string() == node_id)
    );
}

#[test]
fn media_plugin_installs_all_codec_registry_nodes_as_concrete_nodes() {
    let registry = styx_codec::prelude::CodecRegistry::new();
    registry.register(
        FourCc::RG24,
        Arc::new(PassthroughDecoder::new(FourCc::RG24)),
    );
    registry.register(FourCc::RG24, Arc::new(Rg24Encoder::new()));
    let mut plugin = StyxMediaPlugin::new();
    plugin
        .add_codec_registry(&registry.handle(), codec_node_options())
        .expect("add codec registry");
    let descriptors = plugin.codec_descriptors();
    let mut graph_registry = daedalus::runtime::plugins::PluginRegistry::new();

    graph_registry.install(&plugin).expect("install plugin");

    assert_eq!(descriptors.len(), 2);
    assert!(descriptors.iter().any(|descriptor| {
        descriptor.kind == CodecKind::Decoder
            && descriptor.node_id == "styx.codec.decoder.rg24.rg24.passthrough"
    }));
    assert!(descriptors.iter().any(|descriptor| {
        descriptor.kind == CodecKind::Encoder
            && descriptor.node_id == "styx.codec.encoder.rg24.rg24.graph_test"
    }));
    let manifest = graph_registry
        .plugin_manifests
        .get(plugin.id())
        .expect("plugin manifest");
    for descriptor in descriptors {
        assert!(
            manifest
                .provided_nodes
                .iter()
                .any(|id| id.to_string() == descriptor.node_id),
            "missing concrete codec node {}",
            descriptor.node_id
        );
    }
}

#[test]
fn framelease_runs_through_decode_transform_encode_graph() {
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let decode = register_concrete_codec_node(
        &mut registry,
        Arc::new(PassthroughDecoder::new(FourCc::RG24)),
        codec_node_options(),
    )
    .expect("install decode")
    .alias("decode");
    let transform = register_transform_node(
        &mut registry,
        "styx.test.transform",
        FrameTransform {
            rotation: Rotation90::Deg180,
            mirror: false,
        },
    )
    .expect("install transform")
    .alias("transform");
    let encode = register_concrete_codec_node(
        &mut registry,
        Arc::new(Rg24Encoder::new()),
        codec_node_options(),
    )
    .expect("install encode")
    .alias("encode");
    let graph = registry
        .graph_builder()
        .expect("graph builder")
        .inputs(|g| {
            g.input("frame");
        })
        .outputs(|g| {
            g.output("frame");
        })
        .nodes(|g| {
            g.add_handle(&decode);
            g.add_handle(&transform);
            g.add_handle(&encode);
        })
        .edges(|g| {
            g.connect("frame", &decode.input("frame"));
            g.connect(&decode.output("frame"), &transform.input("frame"));
            g.connect(&transform.output("frame"), &encode.input("frame"));
            g.connect(&encode.output("frame"), "frame");
        })
        .build();

    let engine = daedalus::engine::Engine::new(
        daedalus::engine::EngineConfig::from(daedalus::engine::GpuBackend::Cpu)
            .with_metrics_level(daedalus::engine::MetricsLevel::Detailed),
    )
    .expect("engine");
    let mut runtime = engine
        .compile_registry(&registry, graph)
        .expect("compile graph");
    runtime.push_payload("frame", framelease_payload(test_frame()));
    let telemetry = runtime
        .tick_until_idle()
        .expect("tick graph")
        .expect("telemetry");
    let output = runtime
        .drain_owned::<FrameLease>("frame")
        .expect("drain frame");

    assert_eq!(output.len(), 1);
    assert_eq!(output[0].meta().format.code, FourCc::RG24);
    assert_eq!(output[0].meta().format.resolution.width.get(), 2);
    assert_eq!(output[0].meta().format.resolution.height.get(), 2);
    let copied_bytes = telemetry
        .edge_metrics
        .values()
        .map(|metrics| metrics.copied_bytes)
        .sum::<u64>();
    assert_eq!(copied_bytes, 0);
}

#[test]
fn v4l2_mmap_style_external_frame_passes_graph_without_copy() {
    let format = MediaFormat::new(
        FourCc::YUYV,
        Resolution::new(2, 2).unwrap(),
        ColorSpace::Unknown,
    );
    let layout = crate::core::prelude::plane_layout_from_dims(
        NonZeroU32::new(2).unwrap(),
        NonZeroU32::new(2).unwrap(),
        2,
    );
    let backing: Arc<dyn crate::core::prelude::ExternalBacking> = Arc::new(TestExternalBacking {
        data: vec![0x5a; layout.len].into(),
        kind: "v4l2_mmap",
    });
    let frame = FrameLease::from_external(
        FrameMeta::new(format, 11),
        smallvec::smallvec![layout],
        backing,
    );

    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let tap = register_preview_node(&mut registry, "styx.test.v4l2_tap", |_| {})
        .expect("install tap")
        .alias("tap");
    let graph = registry
        .graph_builder()
        .expect("graph builder")
        .inputs(|g| {
            g.input("frame");
        })
        .outputs(|g| {
            g.output("frame");
        })
        .nodes(|g| {
            g.add_handle(&tap);
        })
        .edges(|g| {
            g.connect("frame", &tap.input("frame"));
            g.connect(&tap.output("frame"), "frame");
        })
        .build();
    let engine = daedalus::engine::Engine::new(
        daedalus::engine::EngineConfig::from(daedalus::engine::GpuBackend::Cpu)
            .with_metrics_level(daedalus::engine::MetricsLevel::Detailed),
    )
    .expect("engine");
    let mut runtime = engine
        .compile_registry(&registry, graph)
        .expect("compile graph");
    runtime.push_payload("frame", framelease_payload(frame));
    let telemetry = runtime
        .tick_until_idle()
        .expect("tick graph")
        .expect("telemetry");
    let output = runtime
        .drain_owned::<FrameLease>("frame")
        .expect("drain frame");

    assert_eq!(output.len(), 1);
    assert_eq!(output[0].residency(), FrameResidency::HostExternal);
    assert_eq!(output[0].external_backing_kind(), Some("v4l2_mmap"));
    let copied_bytes = telemetry
        .edge_metrics
        .values()
        .map(|metrics| metrics.copied_bytes)
        .sum::<u64>();
    assert_eq!(copied_bytes, 0);
}

#[test]
fn overloaded_preview_policy_keeps_newest_frame() {
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let preview = register_preview_node(&mut registry, "styx.test.latest_only", |_| {})
        .expect("install preview")
        .alias("preview");
    let graph = registry
        .graph_builder()
        .expect("graph builder")
        .inputs(|g| {
            g.input("frame");
        })
        .outputs(|g| {
            g.output("frame");
        })
        .nodes(|g| {
            g.add_handle(&preview);
        })
        .edges(|g| {
            g.connect("frame", &preview.input("frame"));
            g.connect(&preview.output("frame"), "frame");
        })
        .build();
    let engine = daedalus::engine::Engine::new(
        daedalus::engine::EngineConfig::from(daedalus::engine::GpuBackend::Cpu)
            .with_metrics_level(daedalus::engine::MetricsLevel::Detailed),
    )
    .expect("engine");
    let mut runtime = engine
        .compile_registry(&registry, graph)
        .expect("compile graph");
    let policy = latest_only();
    runtime
        .set_input_policy("frame", policy.pressure, policy.freshness)
        .expect("set preview input policy");
    assert!(matches!(
        runtime.push_payload("frame", framelease_payload(test_frame_with_timestamp(0))),
        daedalus::transport::FeedOutcome::Accepted { .. }
    ));
    assert!(matches!(
        runtime.push_payload("frame", framelease_payload(test_frame_with_timestamp(1))),
        daedalus::transport::FeedOutcome::Replaced { .. }
    ));
    assert!(matches!(
        runtime.push_payload("frame", framelease_payload(test_frame_with_timestamp(2))),
        daedalus::transport::FeedOutcome::Replaced { .. }
    ));
    let telemetry = runtime
        .tick_until_idle()
        .expect("tick graph")
        .expect("telemetry");
    let output = runtime
        .drain_owned::<FrameLease>("frame")
        .expect("drain frame");

    assert_eq!(output.len(), 1);
    assert_eq!(output[0].meta().timestamp, 2);
    let copied_bytes = telemetry
        .edge_metrics
        .values()
        .map(|metrics| metrics.copied_bytes)
        .sum::<u64>();
    assert_eq!(copied_bytes, 0);
}

#[test]
fn overloaded_recording_policy_backpressures_instead_of_dropping() {
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let recording = register_preview_node(&mut registry, "styx.test.bounded_blocking", |_| {})
        .expect("install recording")
        .alias("recording");
    let graph = registry
        .graph_builder()
        .expect("graph builder")
        .inputs(|g| {
            g.input("frame");
        })
        .outputs(|g| {
            g.output("frame");
        })
        .nodes(|g| {
            g.add_handle(&recording);
        })
        .edges(|g| {
            g.connect("frame", &recording.input("frame"));
            g.connect(&recording.output("frame"), "frame");
        })
        .build();
    let engine = daedalus::engine::Engine::new(
        daedalus::engine::EngineConfig::from(daedalus::engine::GpuBackend::Cpu)
            .with_metrics_level(daedalus::engine::MetricsLevel::Detailed),
    )
    .expect("engine");
    let runtime = engine
        .compile_registry(&registry, graph)
        .expect("compile graph");
    let policy = bounded_blocking(1);
    runtime
        .set_input_policy("frame", policy.pressure, policy.freshness)
        .expect("set recording input policy");

    assert!(matches!(
        runtime.push_payload("frame", framelease_payload(test_frame_with_timestamp(0))),
        daedalus::transport::FeedOutcome::Accepted { .. }
    ));
    assert!(matches!(
        runtime.push_payload("frame", framelease_payload(test_frame_with_timestamp(1))),
        daedalus::transport::FeedOutcome::Backpressured
    ));
}
