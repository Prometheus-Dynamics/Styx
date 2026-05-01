use super::*;

fn register_burst_source(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: &'static str,
    frames: u64,
) -> NodeHandle {
    registry
        .register_node_decl(
            NodeDecl::new(node_id).output(
                PortDecl::new("frame", framelease_type_key())
                    .schema(daedalus::data::model::TypeExpr::opaque(FRAMELEASE_TYPE_KEY)),
            ),
        )
        .expect("register burst source");
    registry
        .handlers
        .try_on(node_id, move |_node, _ctx, io| {
            for ts in 0..frames {
                io.push_payload("frame", framelease_payload(test_frame_with_timestamp(ts)));
            }
            Ok(())
        })
        .expect("register burst source handler");
    NodeHandle::new(node_id)
}

#[test]
fn fanout_graph_uses_branch_specific_media_policies() {
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let source = register_burst_source(&mut registry, "styx.test.fanout_source", 3).alias("source");
    let graph = registry
        .graph_builder()
        .expect("graph builder")
        .outputs(|g| {
            g.output("preview");
            g.output("recording");
            g.output("analysis");
        })
        .nodes(|g| {
            g.add_handle(&source);
        })
        .edges(|g| {
            g.connect_policy(&source.output("frame"), "preview", preview_policy());
            g.connect_policy(&source.output("frame"), "recording", recording_policy(8));
            g.connect_policy(&source.output("frame"), "analysis", analysis_policy(3, 99));
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
    let preview_output = preview_policy();
    runtime
        .set_output_policy("preview", preview_output.pressure, preview_output.freshness)
        .expect("set preview output policy");
    let recording_output = recording_policy(8);
    runtime
        .set_output_policy(
            "recording",
            recording_output.pressure,
            recording_output.freshness,
        )
        .expect("set recording output policy");
    let analysis_output = analysis_policy(3, 99);
    runtime
        .set_output_policy(
            "analysis",
            analysis_output.pressure,
            analysis_output.freshness,
        )
        .expect("set analysis output policy");
    let telemetry = runtime.tick().expect("tick fanout");
    let preview = drain_frame_timestamps(&runtime, "preview");
    let recording = drain_frame_timestamps(&runtime, "recording");
    let analysis = drain_frame_timestamps(&runtime, "analysis");

    assert_eq!(preview, vec![2]);
    assert_eq!(recording, vec![0, 1, 2]);
    assert_eq!(analysis, vec![0, 1, 2]);
    let copied_bytes = telemetry
        .edge_metrics
        .values()
        .map(|metrics| metrics.copied_bytes)
        .sum::<u64>();
    assert_eq!(copied_bytes, 0);
}

#[test]
fn preview_and_analysis_taps_borrow_frames_without_copying() {
    let preview_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let analysis_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let preview_seen_for_node = Arc::clone(&preview_seen);
    let preview = register_preview_node(&mut registry, "styx.test.preview", move |frame| {
        preview_seen_for_node
            .lock()
            .expect("preview lock")
            .push(frame.meta().timestamp);
    })
    .expect("install preview")
    .alias("preview");
    let analysis_seen_for_node = Arc::clone(&analysis_seen);
    let analysis = register_analysis_node(&mut registry, "styx.test.analysis", move |frame| {
        analysis_seen_for_node
            .lock()
            .expect("analysis lock")
            .push(frame.meta().timestamp);
    })
    .expect("install analysis")
    .alias("analysis");
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
            g.add_handle(&analysis);
        })
        .edges(|g| {
            g.connect("frame", &preview.input("frame"));
            g.connect(&preview.output("frame"), &analysis.input("frame"));
            g.connect(&analysis.output("frame"), "frame");
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
    runtime.push_payload("frame", framelease_payload(test_frame_with_timestamp(42)));
    let telemetry = runtime
        .tick_until_idle()
        .expect("tick taps")
        .expect("telemetry");
    let output = runtime
        .drain_owned::<FrameLease>("frame")
        .expect("drain frame");

    assert_eq!(output.len(), 1);
    assert_eq!(*preview_seen.lock().expect("preview lock"), vec![42]);
    assert_eq!(*analysis_seen.lock().expect("analysis lock"), vec![42]);
    let copied_bytes = telemetry
        .edge_metrics
        .values()
        .map(|metrics| metrics.copied_bytes)
        .sum::<u64>();
    assert_eq!(copied_bytes, 0);
}

#[test]
fn materialize_node_makes_branch_copy_explicit() {
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
        kind: "branch_source",
    });
    let frame = FrameLease::from_external(
        FrameMeta::new(format, 17),
        smallvec::smallvec![layout],
        backing,
    );
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let materialize = register_materialize_node(&mut registry, "styx.test.materialize")
        .expect("install materialize")
        .alias("materialize");
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
            g.add_handle(&materialize);
        })
        .edges(|g| {
            g.connect("frame", &materialize.input("frame"));
            g.connect(&materialize.output("frame"), "frame");
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
    runtime.tick_until_idle().expect("tick materialize");
    let output = runtime
        .drain_owned::<FrameLease>("frame")
        .expect("drain frame");

    assert_eq!(output.len(), 1);
    assert_eq!(output[0].residency(), FrameResidency::HostOwned);
}

fn drain_frame_timestamps(
    runtime: &daedalus::engine::HostGraph<daedalus::runtime::handler_registry::HandlerRegistry>,
    port: &str,
) -> Vec<u64> {
    runtime
        .drain_payloads(port)
        .into_iter()
        .map(|payload| {
            payload
                .get_ref::<FrameLease>()
                .expect("FrameLease payload")
                .meta()
                .timestamp
        })
        .collect()
}
