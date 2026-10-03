//! Zero-copy: raw frames read in place from a dma-buf (a `/dev/dma_heap/system` buffer, as a
//! capture buffer would be), and outputs exported as dma-bufs that another mapping reads.
//! Skipped without Vulkan, without dma-buf support or without access to the heap.

mod common;

use std::os::fd::AsFd;

use common::{contexts, mosaic, pack};
use styx_gpuisp::{GpuIsp, Input, OutputKind};
use styx_kernel::dma_heap::{Access, DmaBuf, DmaHeap};
use styx_softisp::*;

fn params() -> IspParams {
    IspParams {
        black_level: Some(BlackLevel::uniform(64)),
        white_balance: Some(WhiteBalance {
            r: 1.8,
            g: 1.0,
            b: 1.5,
        }),
        lens_shading: Some(LensShading::radial(16, 12, 0.5)),
        ccm: Some(ColorMatrix::saturation(1.3)),
        tone: Some(ToneCurve::Srgb),
        stats: Some(StatsConfig::default()),
        arithmetic: Arithmetic::Int,
        ..IspParams::default()
    }
}

#[test]
fn import_and_export() {
    let heap = match DmaHeap::open("system") {
        Ok(h) => h,
        Err(e) => {
            eprintln!("no dma-heap ({e}): dma-buf test skipped");
            return;
        }
    };
    let (w, h) = (320usize, 240usize);
    let m = mosaic(w, h, CfaPattern::Bggr, |x, y| {
        [
            (x * 3 % 1024) as u16,
            ((x + y) % 1024) as u16,
            (y * 4 % 1024) as u16,
        ]
    });
    let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
    let format = RawFormat::new(w as u32, h as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    // The frame 4096 bytes into the buffer, as a buffer holding a header would have it.
    let offset = 4096;
    let buf = heap.allocate(offset + raw.len()).unwrap();
    {
        let mut map = buf.map().unwrap();
        let _ = buf.begin_cpu_access(Access::Write);
        map.as_mut_slice()[offset..][..raw.len()].copy_from_slice(&raw);
        let _ = buf.end_cpu_access(Access::Write);
    }
    let mut cpu = SoftIsp::new(format, params()).unwrap();
    let mut want = vec![0u8; w * h * 3 / 2];
    let (y, uv) = want.split_at_mut(w * h);
    let want_stats = cpu
        .process(
            &raw,
            stride,
            Scale::Full,
            OutputBuffers::Nv12 {
                y,
                y_stride: w,
                uv,
                uv_stride: w,
            },
        )
        .unwrap();
    for ctx in contexts() {
        if !ctx.info().dmabuf {
            eprintln!("{}: no dma-buf support, skipped", ctx.info().name);
            continue;
        }
        let mut gpu = GpuIsp::with_context(&ctx, format, params()).unwrap();
        let id = gpu.import_dmabuf(buf.as_fd(), buf.len()).unwrap();
        gpu.set_export_buffers(2);
        for k in 0..3 {
            let input = Input::DmaBuf { id, offset };
            let frame = gpu
                .process_to_export(input, stride, Scale::Full, OutputKind::Nv12)
                .unwrap();
            assert_eq!(frame.slot, k % 2);
            assert_eq!(frame.stats, want_stats, "{}", ctx.info().name);
            let l = frame.layout;
            assert_eq!((l.planes[0].offset, l.planes[1].offset), (0, w * h));
            assert!(
                gpu.exported(frame.slot).unwrap()[..l.size] == want[..],
                "{}: mapped output",
                ctx.info().name
            );
            // Another mapping of the exported dma-buf (as a consumer process would make).
            let fd = gpu.export_dmabuf(frame.slot).unwrap();
            let len = std::fs::File::from(fd.try_clone().unwrap())
                .metadata()
                .map(|m| m.len() as usize)
                .unwrap_or(0)
                .max(l.size);
            let out = DmaBuf::from_fd(fd, len);
            let map = out.map().unwrap();
            let _ = out.begin_cpu_access(Access::Read);
            assert!(
                map.as_slice()[..l.size] == want[..],
                "{}: exported dma-buf",
                ctx.info().name
            );
            let _ = out.end_cpu_access(Access::Read);
        }
        // The same frame through the copy path.
        let mut got = vec![0u8; w * h * 3 / 2];
        let (y, uv) = got.split_at_mut(w * h);
        let input = Input::DmaBuf { id, offset };
        let out = OutputBuffers::Nv12 {
            y,
            y_stride: w,
            uv,
            uv_stride: w,
        };
        gpu.process_input(input, stride, Scale::Full, out).unwrap();
        assert!(got == want, "{}", ctx.info().name);
        gpu.release_import(id);
        let input = Input::DmaBuf { id, offset };
        assert!(
            gpu.process_to_export(input, stride, Scale::Full, OutputKind::Nv12)
                .is_err()
        );
    }
}
