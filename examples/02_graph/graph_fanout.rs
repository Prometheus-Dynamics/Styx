#[cfg(feature = "graph-pipeline")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::num::NonZeroU32;

    use daedalus::registry::capability::{NodeDecl, PortDecl};
    use styx::graph::{
        FRAMELEASE_TYPE_KEY, bounded_blocking, bounded_drop_oldest, framelease_payload,
        framelease_type_key, latest_only,
    };
    use styx::prelude::{
        BufferPool, ColorSpace, FourCc, FrameLease, FrameMeta, MediaFormat, Resolution,
        plane_layout_from_dims,
    };

    fn frame(timestamp: u64) -> FrameLease {
        let format = MediaFormat::new(
            FourCc::RG24,
            Resolution::new(2, 2).unwrap(),
            ColorSpace::Srgb,
        );
        let layout =
            plane_layout_from_dims(NonZeroU32::new(2).unwrap(), NonZeroU32::new(2).unwrap(), 3);
        let pool = BufferPool::lazy(layout.len, 1);
        FrameLease::single_plane(
            FrameMeta::new(format, timestamp),
            pool.lease(),
            layout.len,
            layout.stride,
        )
    }

    let mut registry = daedalus::runtime::plugins::PluginRegistry::new();
    styx::graph::register_framelease_type();
    registry.register_node_decl(
        NodeDecl::new("styx.example.burst").output(
            PortDecl::new("frame", framelease_type_key())
                .schema(daedalus::data::model::TypeExpr::opaque(FRAMELEASE_TYPE_KEY)),
        ),
    )?;
    registry
        .handlers
        .try_on("styx.example.burst", |_node, _ctx, io| {
            for timestamp in 0..3 {
                io.push_payload("frame", framelease_payload(frame(timestamp)));
            }
            Ok(())
        })?;

    let source = daedalus::NodeHandle::new("styx.example.burst").alias("source");
    let graph = registry
        .graph_builder()?
        .outputs(|g| {
            g.output("preview");
            g.output("recording");
            g.output("analysis");
        })
        .nodes(|g| {
            g.add_handle(&source);
        })
        .edges(|g| {
            g.connect_policy(&source.output("frame"), "preview", latest_only());
            g.connect_policy(&source.output("frame"), "recording", bounded_blocking(8));
            g.connect_policy(
                &source.output("frame"),
                "analysis",
                bounded_drop_oldest(3, 99),
            );
        })
        .build();

    let engine = daedalus::engine::Engine::new(
        daedalus::engine::EngineConfig::from(daedalus::engine::GpuBackend::Cpu)
            .with_metrics_level(daedalus::engine::MetricsLevel::Detailed),
    )?;
    let mut runtime = engine.compile_registry(&registry, graph)?;
    for (port, policy) in [
        ("preview", latest_only()),
        ("recording", bounded_blocking(8)),
        ("analysis", bounded_drop_oldest(3, 99)),
    ] {
        runtime.set_output_policy(port, policy.pressure, policy.freshness)?;
    }
    let telemetry = runtime.tick()?;
    println!(
        "fanout ran nodes={} copied_bytes={}",
        telemetry.nodes_executed,
        telemetry
            .edge_metrics
            .values()
            .map(|metrics| metrics.copied_bytes)
            .sum::<u64>()
    );
    Ok(())
}

#[cfg(not(feature = "graph-pipeline"))]
fn main() {
    eprintln!("enable the graph-pipeline feature to run this example");
}
