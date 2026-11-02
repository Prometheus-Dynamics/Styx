//! The GPU ISP on Daedalus's Vulkan device (`GpuContext::from_daedalus`): frames submitted
//! through Daedalus's queue, read from a buffer of its own (`Input::Buffer`), bit for bit the
//! software ISP's. Skipped without a Vulkan device, or when Daedalus's device lacks 8-bit
//! storage buffers.

use crate::common::{mosaic, pack};
use daedalus_gpu::GpuStorage;
use styx_gpuisp::{ExternalBuffer, GpuContext, GpuError, GpuIsp, Input};
use styx_softisp::*;

#[test]
fn processes_on_daedalus_device_from_its_buffer() {
    let Some(gpu) = crate::common::daedalus_gpu() else {
        return;
    };
    let ctx = match GpuContext::from_daedalus(&gpu) {
        Ok(c) => c,
        Err(GpuError::Unsupported(why)) => {
            eprintln!("Daedalus's device: {why}; skipped");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let (w, h) = (128usize, 96usize);
    let m = mosaic(w, h, CfaPattern::Rggb, |x, y| {
        [
            (x * 7 % 1024) as u16,
            ((x + y) * 3 % 1024) as u16,
            (y * 9 % 1024) as u16,
        ]
    });
    let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
    let format = RawFormat::new(w as u32, h as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    let params = IspParams {
        black_level: Some(BlackLevel::uniform(64)),
        tone: Some(ToneCurve::Srgb),
        arithmetic: Arithmetic::Int,
        ..IspParams::default()
    };
    let mut want = vec![0u8; w * h * 3];
    let out = OutputBuffers::Rgb24 {
        data: &mut want,
        stride: w * 3,
    };
    SoftIsp::new(format, params.clone())
        .unwrap()
        .process(&raw, stride, Scale::Full, out)
        .unwrap();

    let mut isp = GpuIsp::with_context(&ctx, format, params).unwrap();
    // The frame 256 bytes into one of Daedalus's buffers.
    let mut bytes = vec![0u8; 256];
    bytes.extend_from_slice(&raw);
    let storage = GpuStorage::with_data(&gpu, &bytes, Some("raw")).unwrap();
    crate::common::flush_uploads(&gpu);
    // SAFETY: Daedalus's buffer on Daedalus's device, alive and unwritten for the call.
    let input = unsafe {
        let raw_vk = gpu.raw_vulkan().unwrap();
        let b = storage.raw_vk_buffer().unwrap();
        ExternalBuffer::new(raw_vk.device.handle(), b.buffer, 256, raw.len())
    };
    for _ in 0..2 {
        let mut got = vec![0u8; w * h * 3];
        let out = OutputBuffers::Rgb24 {
            data: &mut got,
            stride: w * 3,
        };
        isp.process_input(Input::Buffer(input), stride, Scale::Full, out)
            .unwrap();
        assert!(got == want, "{}", ctx.info().name);
    }
    // From memory too, on the same queue.
    let mut got = vec![0u8; w * h * 3];
    let out = OutputBuffers::Rgb24 {
        data: &mut got,
        stride: w * 3,
    };
    isp.process(&raw, stride, Scale::Full, out).unwrap();
    assert!(got == want);
}
