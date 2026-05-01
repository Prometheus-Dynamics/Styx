use super::*;

#[test]
fn capture_request_source_emits_framelease_without_graph_copy() {
    let device = virtual_device();
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let source = register_capture_request_source_with_policy(
        &mut registry,
        "styx.test.capture_source",
        CaptureRequest::new(&device).backend(BackendKind::Virtual),
        CaptureStartPolicy::default(),
        StyxCaptureSourceOptions::wait_one(Duration::from_millis(250)),
    )
    .expect("install capture source")
    .alias("source");
    let graph = registry
        .graph_builder()
        .expect("graph builder")
        .outputs(|g| {
            g.output("frame");
        })
        .nodes(|g| {
            g.add_handle(&source);
        })
        .edges(|g| {
            g.connect(&source.output("frame"), "frame");
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
    let mut output = Vec::new();
    let mut telemetry = None;
    for _ in 0..25 {
        let tick = runtime.tick().expect("tick capture source");
        let drained = runtime
            .drain_owned::<FrameLease>("frame")
            .expect("drain frame");
        if !drained.is_empty() {
            output = drained;
            telemetry = Some(tick);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let telemetry = telemetry.expect("capture source should emit a frame");

    assert_eq!(output.len(), 1);
    assert_eq!(output[0].meta().format.code, FourCc::RG24);
    let copied_bytes = telemetry
        .edge_metrics
        .values()
        .map(|metrics| metrics.copied_bytes)
        .sum::<u64>();
    assert_eq!(copied_bytes, 0);
}

#[test]
fn camera_request_can_register_limited_source_nodes() {
    let device = virtual_device();
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let sources = register_camera_sources_limit(
        &mut registry,
        "styx.test.camera_source",
        CameraRequest::from_devices(vec![device])
            .backend_priority([BackendKind::Virtual])
            .format_priority([*b"RG24"]),
        1,
    )
    .expect("install camera sources");

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].id(), "styx.test.camera_source.0");
}

#[test]
fn media_plugin_installs_capture_source_nodes() {
    let device = virtual_device();
    let capture = CaptureRequest::new(&device)
        .backend(BackendKind::Virtual)
        .start()
        .expect("start virtual capture");
    let mut plugin = StyxMediaPlugin::new();
    let source = plugin
        .add_capture_source_with_options(
            "styx.test.plugin_capture_source",
            capture,
            StyxCaptureSourceOptions::wait_one(Duration::from_millis(250)),
        )
        .alias("source");
    let descriptors = plugin.source_descriptors();
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    registry.install(&plugin).expect("install plugin");
    let manifest = registry
        .plugin_manifests
        .get(plugin.id())
        .expect("plugin manifest");

    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].node_id, "styx.test.plugin_capture_source");
    assert_eq!(descriptors[0].kind, StyxSourceKind::CaptureHandle);
    assert!(
        manifest
            .provided_nodes
            .iter()
            .any(|id| id.to_string() == "styx.test.plugin_capture_source")
    );

    let graph = registry
        .graph_builder()
        .expect("graph builder")
        .outputs(|g| {
            g.output("frame");
        })
        .nodes(|g| {
            g.add_handle(&source);
        })
        .edges(|g| {
            g.connect(&source.output("frame"), "frame");
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
    let telemetry = runtime.tick().expect("tick capture source");
    let output = runtime
        .drain_owned::<FrameLease>("frame")
        .expect("drain frame");

    assert_eq!(output.len(), 1);
    assert_eq!(output[0].meta().format.code, FourCc::RG24);
    assert_eq!(
        telemetry
            .edge_metrics
            .values()
            .map(|metrics| metrics.copied_bytes)
            .sum::<u64>(),
        0
    );
}
