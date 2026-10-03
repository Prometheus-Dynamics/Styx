# styx-gpuisp

The software ISP's pipeline (`styx-softisp`) as Vulkan compute shaders. `GpuIsp` takes the
same `IspParams`, raw formats, `OutputBuffers` and returns the same `IspStats` as `SoftIsp`,
and computes `SoftIsp`'s integer arithmetic (`Arithmetic::Int`) **bit for bit**: a pipeline can
swap one for the other (`styx-pipeline`'s `SoftLoop::use_gpu`, feature `gpu`).

Stages: unpacking (8-bit, 16-bit LE, CSI-2 RAW10/RAW12), black level per CFA cell, white
balance and digital gain, lens shading (the integer path's Q12 gain rows, interpolated per row
in Q15), bilinear or Malvar-He-Cutler demosaic, Q12 colour matrix with 16-bit saturating sums,
the 4096-entry tone table, RGB24 / NV12 / I420 / luma (BT.601 full or BT.709 limited) at full
or half size (each quad a pixel), and the 3A statistics: per-zone R/G/B sums of unclipped
quads, counts, zone luma, luma histogram, on every `row_step`-th quad row.

The crate is optional: nothing in Styx depends on it by default, and it builds without a C
toolchain or the Vulkan SDK.

## Design

* **ash, not wgpu.** The ISP is three compute dispatches; what matters is dma-buf import and
  export (`VK_EXT_external_memory_dma_buf` + `VK_KHR_external_memory_fd`) and the per-frame
  overhead. `ash` is a thin binding: the external-memory structures are plain Vulkan, a frame
  is one command buffer re-recorded (a dozen commands) and one `vkQueueSubmit`. wgpu has no
  public dma-buf buffer import (it would mean dropping to `wgpu-hal` and its Vulkan handles
  anyway), and brings naga, resource tracking and validation on every submission. `ash` is
  loaded with its `loaded` feature: `libvulkan.so.1` is `dlopen`ed at run time, so binaries
  link nothing Vulkan and start (and fall back to the CPU) where there is no Vulkan.
* **Shaders.** GLSL in `shaders/` (`common.glsl`: parameters and the front end;
  `full.comp`, `half.comp`, `stats.comp`), compiled to SPIR-V by `shaders/build.sh` (glslc,
  Vulkan 1.2) and committed, so building the crate needs no shader compiler. Integer
  arithmetic only, the same as `styx-softisp`'s scalar oracle. Bytes are read and written
  through 8-bit storage buffers (`storageBuffer8BitAccess`, required).
* **Full size**: a workgroup of 16x8 invocations makes a 32x16 block, each invocation one
  2x2 quad (one 4:2:0 chroma sample). The block and a 2-sample border (36x20) go through the
  front end once into shared memory (reflected at the frame edges as the CPU's rows are);
  demosaic, colour matrix, tone table and the output conversion read from there. **Half
  size**: each invocation turns 2x2 quads into 2x2 pixels, no shared memory. **Statistics**:
  one workgroup per zone, 256 invocations, front end recomputed on the sampled quads (every
  fourth quad row in the pipeline), sums reduced in 64 bits in shared memory, histogram in
  shared memory (up to 1024 bins; global atomics above).
* **One submission per frame, one wait.** The command buffer: clear the histogram, copy the
  raw frame into device memory (devices with their own memory: the copy engine; others read
  the frame where it is), the picture, the statistics, copy the results to host memory. The
  CPU then waits on the frame's fence once, and copies the image into the caller's
  `OutputBuffers` (or leaves it in an exportable buffer). Buffers, descriptor sets (one per
  buffer, made once), pipelines, the command pool and the fence are made once; the
  parameter-dependent tables (tone, lens shading) are uploaded only when they change.
* **Zero-copy.** `GpuIsp::import_dmabuf` registers a capture buffer (a V4L2 / dma-heap
  dma-buf) and `Input::DmaBuf` processes frames in it without the CPU touching them (no
  mapping, no cache maintenance); `GpuIsp::process_to_export` writes into a ring of
  exportable buffers and `GpuIsp::export_dmabuf` hands them out as dma-bufs. `SoftPipeline`
  (`styx-pipeline`, features `device` + `gpu`) imports the camera's capture buffers. Without
  the extension, frames are copied in and out (`Input::Bytes`, `OutputBuffers`).
* **Devices**: Vulkan 1.2, `storageBuffer8BitAccess`, a compute queue. `DeviceSelect::Auto`
  (and `STYX_GPUISP_DEVICE`: an index into `devices()` or part of a name) takes a discrete,
  then integrated, then virtual GPU, never a software rasteriser unless named; tests run on
  every device found (llvmpipe included, useful in CI) and pass with a note without Vulkan.

## Quality

Against `styx-softisp` (this host: Ryzen, x86 with AVX2):

| | integer arithmetic | tone quadratics (`IntPolyTone`, x86 default) | fp16 (`Half`, Cortex-A76 default) |
|---|---|---|---|
| synthetic chart (`tests/quality.rs`), R/G/B/Y/U/V | identical | 56.5-64 dB, max 1 code | 54.8-61.4 dB, max 2-5 codes, < 0.1% more than 2 apart |
| 160 recorded frames (OV9782 BGGR 1280x800, the loop's settings, `native-pipeline gpu-quality`) | identical, statistics identical on 160/160 | 56.2-57.6 dB, max 1, statistics identical | 53.7-55.2 dB, max 2 codes (155 frames: 5 need gains above the integer path's limit of 16) |

The fp16 figures are fp16 against the integer arithmetic (the GPU computes the latter); the
statistics differ from fp16's as the integer arithmetic's do (zone sums up to 5.7% apart in
the darkest zones, 0.6% of the histogram in a neighbouring bin). Bit-exactness is tested on
every output, scale, demosaic, colour pattern and packing, with and without lens shading,
colour matrix and tone curve, odd sizes, both histogram paths, parameter changes between
frames and skipped statistics (`tests/equivalence.rs`), through dma-buf import and export
(`tests/dmabuf.rs`), and through the whole 3A loop (`styx-pipeline/tests/gpu_loop.rs`), on
RADV and llvmpipe.

## Performance

AMD Radeon RX 6800 XT (RADV, Mesa 26.2.3) against `styx-softisp` on the same host,
`native-pipeline gpu-bench` over the recording with the HeliOS tuning (lens shading 32x32,
CCM, adaptive contrast, statistics every fourth row, 3A at 15 Hz once settled), frames paced
at the rate, 300 frames after 10 of warm-up. CPU is the whole process per frame (ISP,
settings, 3A, the Vulkan driver's threads), wall the loop's `process` call:

| 1280x800 | CPU, software ISP 1 thread | CPU, 4 threads | CPU, GPU ISP | wall: 1 / 4 threads / GPU | GPU time |
|---|---|---|---|---|---|
| NV12, 30 fps | 1.77 ms (5.3% of a core) | 1.96 ms | **0.47 ms (1.4%)** | 1.83 / 0.72 / 0.77 ms | 0.21 ms |
| NV12, 120 fps | 1.70 ms (20.4%) | 1.87 ms | **0.46 ms (5.5%)** | 1.74 / 0.69 / 0.79 ms | 0.24 ms |
| RGB24, 30 fps | 1.80 ms (5.4%) | 2.09 ms | **0.62 ms (1.9%)** | 1.88 / 0.73 / 0.99 ms | 0.29 ms |
| RGB24, 120 fps | 1.69 ms (20.3%) | 1.89 ms | **0.67 ms (8.1%)** | 1.73 / 0.70 / 1.10 ms | 0.32 ms |
| NV12 640x400 (binned), 30 fps | 1.02 ms | 1.24 ms | 0.44 ms | 1.05 / 0.53 / 0.66 ms | 0.14 ms |

(The binned row comes from a second, noisier run on a loaded host.) The GPU takes a quarter
of a millisecond; the GPU ISP's CPU time is the frame's copy into a mapped buffer (1.3 MB),
the output's copy out (1.5-3 MB from cached host memory), the tables (0.2 ms of `set_params`
when adaptive lens shading and the gains change, as they do on most frames: the integer path
rebuilds its gain rows then, 0.16 ms in `styx-softisp`) and the submission and wait. Its
latency equals four CPU threads' at a quarter of one thread's CPU. With a capture dma-buf
(`Input::DmaBuf`) the input copy goes; with an exported output, the output copy.

## Raspberry Pi 5 / CM5 (V3D 7.1)

The HeliOS image has no Vulkan driver. Mesa's **v3dv** supports the Pi 5's V3D 7.1 (Vulkan
1.3, `storageBuffer8BitAccess`, `VK_EXT_external_memory_dma_buf`). An image would need:
Mesa with `-Dvulkan-drivers=broadcom` (Buildroot `BR2_PACKAGE_MESA3D` +
`BR2_PACKAGE_MESA3D_VULKAN_DRIVER_BROADCOM`; no Gallium/GL needed), the Vulkan loader
(`BR2_PACKAGE_VULKAN_LOADER`, `libvulkan.so.1`) and the ICD file
(`/usr/share/vulkan/icd.d/broadcom_icd.*.json`), the `v3d` DRM driver (`CONFIG_DRM_V3D`, the
`vc4-kms-v3d` overlay or the v3d node enabled) and access to `/dev/dri/renderD128` for the
camera service's user. About 10-15 MB of image.

Estimate, not measured (no v3dv here): V3D 7.1 has 12 QPUs (3 slices) of 16-lane SIMD at
about 1 GHz, issuing roughly 4 lane-operations per cycle each (~50 G lane-ops/s), with texture
and memory units (TMUs) shared per slice. The integer full-size shader costs about 150-250
operations and 10-14 memory accesses (byte loads of the packed raw, table lookups, byte
stores) per pixel: 1.0 M pixels need 150-250 M lane-ops (3-5 ms) and 10-14 M TMU accesses
(at about one per cycle per slice, 3.5-5 ms), so 4-8 ms of GPU per 1280x800 frame, against
2.6-3.0 ms for the fp16 NEON path on one A76 core. The GPU would take the ISP off the CPU
(an estimated 0.3-0.5 ms of CPU per frame left for submission, waits and the tables: about
2.5% of a core at 30 fps instead of 9-10%) but add 1-5 ms of latency, and it shares the
memory bandwidth with the CPU. It would not beat the A76's NEON path on time, and on the CM5
the PiSP does the job at 0.3 ms of CPU anyway; it is worth it for boards with a capable GPU
and no ISP (or Pi 4-class CPUs, whose integer path costs 5.3 ms per frame).

## Use

```rust
use styx_gpuisp::{GpuContext, DeviceSelect, GpuIsp};
use styx_softisp::*;

let ctx = GpuContext::open(DeviceSelect::Auto)?;
let format = RawFormat::new(1280, 800, CfaPattern::Bggr, RawPacking::Csi2Raw10);
let mut isp = GpuIsp::with_context(&ctx, format, IspParams::default())?;
let stats = isp.process(&raw, 1600, Scale::Full, OutputBuffers::Rgb24 { data: &mut rgb, stride: 3840 })?;
```

`cargo run -p styx-gpuisp --example devices` lists the usable devices; `shaders/build.sh`
recompiles the SPIR-V after a shader change.
