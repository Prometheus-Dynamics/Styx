use super::*;

#[test]
fn styx_capability_inventory_registers_planner_visible_metadata() {
    let device = virtual_device();
    let inventory = crate::capabilities::styx_capability_inventory(&[device]);
    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    let plugin = StyxMediaPlugin::new();
    let node = plugin
        .register_capabilities(&mut registry, &inventory)
        .expect("register capabilities");
    let snapshot = registry.transport_capabilities.snapshot();
    let decl = snapshot
        .nodes
        .iter()
        .find(|decl| decl.id.to_string() == node.id())
        .expect("capability metadata node");

    assert_eq!(
        decl.metadata_json.get("styx.capture_backends"),
        Some(&"1".to_string())
    );
    assert_eq!(
        decl.metadata_json.get("styx.capture_formats"),
        Some(&"RG24".to_string())
    );
}

#[test]
fn plugin_preview_and_analysis_sinks_borrow_frames() {
    let preview_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let analysis_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let service = Arc::new(std::sync::Mutex::new(
        crate::service::StyxServiceRuntime::new(),
    ));
    let mut cursor = service.lock().expect("service lock").subscribe_from_start();
    let mut plugin = StyxMediaPlugin::new().with_service_runtime(Arc::clone(&service));
    let preview_seen_for_sink = Arc::clone(&preview_seen);
    let preview = plugin
        .add_preview_sink("styx.test.preview_sink", move |frame| {
            preview_seen_for_sink
                .lock()
                .expect("preview lock")
                .push(frame.meta().timestamp);
        })
        .alias("preview_sink");
    let analysis_seen_for_sink = Arc::clone(&analysis_seen);
    let analysis = plugin
        .add_analysis_sink("styx.test.analysis_sink", move |frame| {
            analysis_seen_for_sink
                .lock()
                .expect("analysis lock")
                .push(frame.meta().timestamp);
        })
        .alias("analysis_sink");
    let descriptors = plugin.sink_descriptors();
    assert!(descriptors.iter().any(|sink| {
        sink.node_id == "styx.test.preview_sink" && sink.kind == crate::service::SinkKind::Preview
    }));
    assert!(descriptors.iter().any(|sink| {
        sink.node_id == "styx.test.analysis_sink" && sink.kind == crate::service::SinkKind::Analysis
    }));

    let (output, telemetry) = {
        let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
        registry.install(&plugin).expect("install sink plugin");
        let mut runtime = compile_linear_sink_graph(&registry, &[preview, analysis]);
        runtime.push_payload("frame", framelease_payload(test_frame_with_timestamp(101)));
        let telemetry = runtime
            .tick_until_idle()
            .expect("tick sinks")
            .expect("telemetry");
        let output = runtime
            .drain_owned::<FrameLease>("frame")
            .expect("drain frame");
        (output, telemetry)
    };
    let events = {
        let service = service.lock().expect("service lock");
        service.poll_events(&mut cursor).events().to_vec()
    };
    let preview_started = events.iter().any(|event| {
        matches!(
            &event.event,
            crate::service::StyxServiceEvent::Sink(crate::service::SinkLifecycleEvent::Started {
                sink_id,
                kind: crate::service::SinkKind::Preview,
            }) if sink_id == "styx.test.preview_sink"
        )
    });
    let analysis_stopped = events.iter().any(|event| {
        matches!(
            &event.event,
            crate::service::StyxServiceEvent::Sink(crate::service::SinkLifecycleEvent::Stopped {
                sink_id,
                kind: crate::service::SinkKind::Analysis,
            }) if sink_id == "styx.test.analysis_sink"
        )
    });

    assert_eq!(output.len(), 1);
    assert_eq!(*preview_seen.lock().expect("preview lock"), vec![101]);
    assert_eq!(*analysis_seen.lock().expect("analysis lock"), vec![101]);
    assert!(preview_started);
    assert!(analysis_stopped);
    assert_eq!(
        telemetry
            .edge_metrics
            .values()
            .map(|metrics| metrics.copied_bytes)
            .sum::<u64>(),
        0
    );
}

#[cfg(feature = "hooks")]
#[test]
fn plugin_recorder_and_file_sequence_sinks_write_frames() {
    let base = std::env::temp_dir().join(format!(
        "styx-graph-sinks-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let recorder_dir = base.join("recorder");
    let sequence_dir = base.join("sequence");
    std::fs::create_dir_all(&base).expect("temp dir");

    let service = Arc::new(std::sync::Mutex::new(
        crate::service::StyxServiceRuntime::new(),
    ));
    let mut cursor = service.lock().expect("service lock").subscribe_from_start();
    let mut plugin = StyxMediaPlugin::new();
    plugin.set_service_runtime(Arc::clone(&service));
    let recorder = crate::recording::FrameRecorder::new(
        &recorder_dir,
        crate::recording::RecordingOptions {
            prefix: "rec".into(),
            format: crate::recording::RecordingFormat::Png,
            ..Default::default()
        },
    )
    .expect("recorder");
    let recorder_node = plugin
        .add_recorder_sink("styx.test.recorder_sink", recorder)
        .alias("recorder_sink");
    let sequence_node = plugin
        .add_file_sequence_sink(
            "styx.test.file_sequence_sink",
            &sequence_dir,
            crate::recording::RecordingOptions {
                prefix: "seq".into(),
                format: crate::recording::RecordingFormat::Png,
                ..Default::default()
            },
        )
        .alias("file_sequence_sink");

    let output = {
        let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
        registry.install(&plugin).expect("install recorder sinks");
        let mut runtime = compile_linear_sink_graph(&registry, &[recorder_node, sequence_node]);
        runtime.push_payload("frame", framelease_payload(test_frame_with_timestamp(202)));
        runtime.tick_until_idle().expect("tick recorder sinks");
        runtime
            .drain_owned::<FrameLease>("frame")
            .expect("drain frame")
    };
    let events = {
        let service = service.lock().expect("service lock");
        service.poll_events(&mut cursor).events().to_vec()
    };
    let frame_indexed = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                crate::service::StyxServiceEvent::Recording(
                    crate::service::RecordingLifecycleEvent::FrameIndexed { .. }
                )
            )
        })
        .count();
    let sink_stopped = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                crate::service::StyxServiceEvent::Sink(
                    crate::service::SinkLifecycleEvent::Stopped { .. }
                )
            )
        })
        .count();
    let recording_stopped = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                crate::service::StyxServiceEvent::Recording(
                    crate::service::RecordingLifecycleEvent::Stopped { .. }
                )
            )
        })
        .count();

    assert_eq!(output.len(), 1);
    assert_eq!(count_image_entries(&recorder_dir), 1);
    assert_eq!(count_image_entries(&sequence_dir), 1);
    assert_eq!(frame_indexed, 2);
    assert_eq!(sink_stopped, 2);
    assert_eq!(recording_stopped, 2);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn plugin_network_stream_sink_writes_length_prefixed_frame_bytes() {
    #[derive(Clone)]
    struct SharedWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("writer lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let written = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut plugin = StyxMediaPlugin::new();
    let network = plugin
        .add_network_stream_sink(
            "styx.test.network_stream_sink",
            SharedWriter(Arc::clone(&written)),
            NetworkStreamSinkOptions::default(),
        )
        .alias("network_stream_sink");

    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    registry.install(&plugin).expect("install network sink");
    let mut runtime = compile_linear_sink_graph(&registry, &[network]);
    let frame = test_frame_with_timestamp(303);
    let bytes = frame.payload_bytes() as u64;
    runtime.push_payload("frame", framelease_payload(frame));
    runtime.tick_until_idle().expect("tick network sink");
    let output = runtime
        .drain_owned::<FrameLease>("frame")
        .expect("drain frame");

    let written = written.lock().expect("writer lock");
    assert_eq!(output.len(), 1);
    assert_eq!(&written[..8], &bytes.to_le_bytes());
    assert_eq!(written.len(), 8 + bytes as usize);
}

fn compile_linear_sink_graph(
    registry: &daedalus::runtime::plugins::PluginRegistry,
    nodes: &[daedalus::NodeHandle],
) -> daedalus::engine::HostGraph<daedalus::runtime::handler_registry::HandlerRegistry> {
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
            for node in nodes {
                g.add_handle(node);
            }
        })
        .edges(|g| {
            for (idx, node) in nodes.iter().enumerate() {
                if idx == 0 {
                    g.connect("frame", &node.input("frame"));
                } else {
                    g.connect(&nodes[idx - 1].output("frame"), &node.input("frame"));
                }
                if idx + 1 == nodes.len() {
                    g.connect(&node.output("frame"), "frame");
                }
            }
        })
        .build();
    let engine = daedalus::engine::Engine::new(
        daedalus::engine::EngineConfig::from(daedalus::engine::GpuBackend::Cpu)
            .with_metrics_level(daedalus::engine::MetricsLevel::Detailed),
    )
    .expect("engine");
    engine
        .compile_registry(registry, graph)
        .expect("compile graph")
}

#[cfg(feature = "hooks")]
fn count_image_entries(path: &std::path::Path) -> usize {
    std::fs::read_dir(path)
        .expect("read dir")
        .filter(|entry| {
            entry
                .as_ref()
                .ok()
                .and_then(|entry| entry.path().extension().map(|ext| ext == "png"))
                .unwrap_or(false)
        })
        .count()
}
