//! A raw frame in a dma-buf from the system heap is imported (once per buffer) and read in place
//! by the GPU ISP, and the picture equals the software ISP's on the same frame. Skips with a
//! printed reason without a Vulkan device or a system dma-heap.
#![cfg(feature = "vulkan")]

#[path = "common/mod.rs"]
mod common;

use std::fs::File;
use std::os::fd::AsFd;

use common::*;
use styx_format::FourCc;
use styx_frame::{BufferMemory, BufferSize, FdMemory, FramePool};
use styx_gpu::{DeviceSelect, GpuContext, GpuIspStage};
use styx_soft::SoftIspStage;
use styx_soft::params::{Demosaic, YuvMatrix};

const LEN: usize = 4096;

fn rgb_bytes(lease: &styx_frame::FrameLease<()>) -> Vec<u8> {
    let read = lease.read().unwrap();
    let plane = read.plane(0).unwrap();
    (0..H as usize)
        .flat_map(|row| plane.row_bytes(row, W as usize * 3).unwrap().to_vec())
        .collect()
}

#[test]
fn dmabuf_raw_is_read_in_place_and_equals_soft() {
    if styx_gpu::devices().is_empty() {
        eprintln!("skipped: no suitable Vulkan device on this host");
        return;
    }
    let Ok(ctx) = GpuContext::open(DeviceSelect::Auto) else {
        eprintln!("skipped: the Vulkan device did not open");
        return;
    };
    let Ok(heap) = File::open("/dev/dma_heap/system") else {
        eprintln!("skipped: no /dev/dma_heap/system on this host");
        return;
    };
    let fd = match lemnos_linux_sys::dma_heap::allocate(heap.as_fd(), LEN as u64) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("skipped: dma-heap allocation failed: {e}");
            return;
        }
    };
    let memory = FdMemory::dmabuf(fd, LEN, true).expect("map the dma-buf");
    assert_eq!(memory.kind(), styx_frame::MemoryKind::Dmabuf);
    let dma_pool = FramePool::new(
        BufferSize::new(LEN, 0),
        vec![Box::new(memory) as Box<dyn BufferMemory>],
    )
    .expect("pool over the dma-buf");

    let settings = settings(true, true);
    let options = || styx_soft::Options {
        demosaic: Demosaic::Bilinear,
        yuv: YuvMatrix::Bt709Limited,
        arithmetic: styx_soft::params::Arithmetic::Auto,
        stats: None,
    };
    let host_pool = picture_pool(RAW, W, H, 1);
    let host_raw = raw_lease(&host_pool, 1);
    let mut cpu = SoftIspStage::new(RAW, W, H, &settings, options()).unwrap();
    let mut gpu = GpuIspStage::with_context(Some(&ctx), RAW, W, H, &settings, options()).unwrap();

    let out_pool = picture_pool(FourCc::RGB3, W, H, 2);
    let mut cpu_out = picture(&out_pool, FourCc::RGB3, W, H, 2);
    cpu.process(&host_raw, &mut cpu_out, None).unwrap();
    let want = rgb_bytes(&cpu_out);

    // The raw bytes go into the dma-buf (through its CPU mapping, bracketed by the sync calls).
    let dma_raw = raw_lease(&dma_pool, 1);
    assert_eq!(dma_raw.memory_kind(), styx_frame::MemoryKind::Dmabuf);
    for id in 2..4u64 {
        let mut gpu_out = picture(&out_pool, FourCc::RGB3, W, H, 2 + id);
        gpu.process(&dma_raw, &mut gpu_out, None).unwrap();
        assert!(
            rgb_bytes(&gpu_out) == want,
            "frame {id}: dma-buf GPU picture differs from soft"
        );
    }
    eprintln!("dmabuf: GPU picture from the dma-buf equals soft (2 frames, one import)");
}
