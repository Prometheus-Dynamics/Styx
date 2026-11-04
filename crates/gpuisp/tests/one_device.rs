//! One Vulkan device per process: the ISP and a Daedalus engine run on the device
//! `GpuContext::shared` takes. A binary of its own, since `devices_created` counts every device
//! the process made. Skipped without a hardware GPU.
#![cfg(feature = "daedalus")]
#![allow(clippy::print_stderr)] // the skip reason

use daedalus::{
    engine::{Engine, EngineConfig, GpuBackend},
    gpu::devices_created,
    macros::{node, plugin},
    runtime::{ExecutionContext, NodeError, plugins::PluginRegistry},
};
use styx_gpuisp::{GpuContext, GpuError, GpuIsp};
use styx_softisp::*;

#[node(id = "test.on_gpu", inputs("tick"), outputs("seen"))]
fn on_gpu(tick: i64, ctx: &ExecutionContext) -> Result<i64, NodeError> {
    ctx.gpu
        .as_ref()
        .ok_or_else(|| NodeError::Handler("no GPU".into()))?;
    Ok(tick)
}

#[plugin(id = "test.on_gpu", nodes(on_gpu))]
struct OnGpuPlugin;

#[test]
fn an_engine_and_the_isp_share_one_device() {
    let ctx = match GpuContext::shared() {
        Ok(c) => c,
        Err(GpuError::Unsupported(why)) => return eprintln!("skipped: {why}"),
        Err(e) => panic!("{e}"),
    };
    let engine = Engine::new(EngineConfig::gpu(GpuBackend::Device)).expect("engine");
    let mut registry = PluginRegistry::new();
    let plugin = OnGpuPlugin::new();
    registry.install(&plugin).expect("install plugin");
    let node = plugin.on_gpu.alias("seen");
    let graph = registry
        .graph_builder()
        .expect("graph builder")
        .try_node(&node)
        .expect("node")
        .try_connect("in", &node.inputs.tick)
        .expect("input edge")
        .try_connect(&node.outputs.seen, "out")
        .expect("output edge")
        .build();
    let mut runtime = engine.compile_registry(&registry, graph).expect("compile");
    assert_eq!(
        runtime.run_direct_once::<i64, i64>("in", "out", 7).unwrap(),
        Some(7)
    );

    let (w, h) = (64usize, 64usize);
    let format = RawFormat::new(w as u32, h as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    let params = IspParams {
        arithmetic: Arithmetic::Int,
        ..IspParams::default()
    };
    let mut isp = GpuIsp::with_context(&ctx, format, params).unwrap();
    let stride = w * 5 / 4;
    let raw = vec![0x55u8; stride * h];
    let mut got = vec![0u8; w * h * 3];
    let out = OutputBuffers::Rgb24 {
        data: &mut got,
        stride: w * 3,
    };
    isp.process(&raw, stride, Scale::Full, out).unwrap();
    assert!(got.iter().any(|&v| v != 0), "the ISP wrote the frame");
    assert_eq!(devices_created(), 1, "one device in the process");
}
