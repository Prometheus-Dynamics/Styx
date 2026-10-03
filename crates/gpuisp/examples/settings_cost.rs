//! Times `GpuIsp::set_params` with a 32x32 lens shading grid whose gains change every frame
//! (as the 3A loop's do), 1280x800.

use std::time::Instant;

use styx_gpuisp::{DeviceSelect, GpuContext, GpuIsp};
use styx_softisp::*;

fn main() {
    let ctx = GpuContext::open(DeviceSelect::Any).expect("a Vulkan device");
    let format = RawFormat::new(1280, 800, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let params = |k: u32| IspParams {
        lens_shading: Some(LensShading::radial(32, 32, 0.5)),
        digital_gain: 1.0 + k as f32 * 1e-3,
        tone: Some(ToneCurve::Gamma {
            gamma: 2.0 + k as f32 * 1e-3,
        }),
        ccm: Some(ColorMatrix::saturation(1.2)),
        stats: Some(StatsConfig::default()),
        ..IspParams::default()
    };
    let mut gpu = GpuIsp::with_context(&ctx, format, params(0)).unwrap();
    let mut cpu = SoftIsp::new(
        format,
        IspParams {
            arithmetic: Arithmetic::Int,
            ..params(0)
        },
    )
    .unwrap();
    let n = 200;
    let t = Instant::now();
    for k in 1..=n {
        gpu.set_params(params(k)).unwrap();
    }
    let g = t.elapsed() / n;
    let t = Instant::now();
    for k in 1..=n {
        cpu.set_params(IspParams {
            arithmetic: Arithmetic::Int,
            ..params(k)
        })
        .unwrap();
    }
    println!(
        "set_params: GPU ISP {g:?}, software ISP (integer) {:?}",
        t.elapsed() / n
    );
}
