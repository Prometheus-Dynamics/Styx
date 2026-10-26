# styx-wgpu

Optional GPU frame preparation for Styx on [wgpu](https://wgpu.rs) (Vulkan, Metal, DX12,
GLES). The CPU and the ISP stay Styx's default path. This crate is for applications that want
work off the CPU and have a GPU to spare. Its kernels compute exactly what the CPU path
computes, so switching between them changes no pixel.

- `Gpu`: a headless device and queue. `Gpu::new` picks the preferred adapter (`WGPU_BACKEND`
  and `WGPU_ADAPTER_NAME` select another). `Gpu::from_device` shares the application's own
  wgpu device, e.g. Daedalus's or Bevy's (same wgpu version and features as Daedalus's GPU
  crate).
- `BoxPyramid`: pyramid levels of an 8-bit luma image. Each level is the level above halved
  by `(a + b + c + d + 2) >> 2`, with sizes rounded down: styx-core's `box_downscale_luma`
  bit for bit. All levels go in one submission and are read back together.

In Styx: feature `gpu-wgpu`, then `PyramidSource::Gpu` on a frame request
(`.pyramid(3).pyramid_source(PyramidSource::Gpu)`). The plan shows the levels as `gpu` steps.
Where there is no GPU, or a GPU pass fails, the CPU makes the levels.

## Status

- Exact against the CPU on RADV (AMD) and llvmpipe: every level, word-aligned and odd strides,
  odd sizes.
- Frames are copied in (`write_buffer`) and the levels copied out. Zero-copy dma-buf import
  (as styx-gpuisp does with ash) is not in wgpu's public API.
- Exact on the Raspberry Pi 5's V3D 7.1 (Mesa v3dv) too. There, levels 1 to 3 of 1280x800
  take 1.67 ms (p50; p95 1.98 ms) and 0.38 ms of the calling thread's CPU (`device` profile,
  2026-10-26, `--ignored time_three_levels_of_1280x800`). The CPU's box filters take less of
  both, so on the Pi 5 the pyramid is a correctness reference for the backend, not a saving.
  Upload and readback dominate: work worth moving to this GPU must be much heavier per pixel.
