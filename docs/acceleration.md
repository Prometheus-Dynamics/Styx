# Acceleration matrix

The 3.0 rule: every stage that can be accelerated is accelerated, on every backend the
hardware offers. That means:

- the CPU's vector unit: x86 SSE2/SSSE3/AVX2, AArch64 NEON, armv7 NEON (Flame's Cortex-A7)
  and RVV (the K230D's C908);
- the GPU, through `styx-gpuisp` (Vulkan) and `styx-wgpu`, where there is one;
- each board's fixed-function units.

Every accelerated leaf is checked against the scalar kernel. The checks use guard bytes and
odd widths, and they run in the gate, on hardware or under QEMU for armv7 and riscv64.

Cell values:

- **pass**: a leaf exists and is conformance-tested.
- **gap**: still to do.
- **n/a**: physically impossible, because the hardware lacks the feature; the reason is
  given. "Probably slower" is not a reason: every variant is built, measured and kept as a
  selectable backend, with its numbers per device.

"lib" means a library's own SIMD: yuvutils-rs, libjpeg-turbo or x264.

## CPU and GPU

### Software ISP, integer path (`styx_softisp::simd`)

The GPU column is `styx-gpuisp`, bit-exact with the CPU except the fp16 row (within the CPU's
fp16 bounds, note 6).

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| RAW10 unpack | pass | pass | pass | pass | pass | pass |
| RAW12 unpack | pass | pass | pass | pass | pass | pass |
| Front end (black, WB, gain, lens shading) | pass | pass | pass | pass | pass | pass |
| Demosaic, bilinear | pass | pass | pass | pass | pass | pass |
| Demosaic, Malvar-He-Cutler | pass | pass | pass | pass | pass | pass |
| Demosaic, edge-directed | pass | pass | pass | pass | pass | pass |
| Bayer luma (luma-only output) | pass | pass | pass | pass | pass | pass |
| Half-size quad RGB | pass | pass | pass | pass | pass | pass |
| Half-size quad luma | pass | pass | pass | pass | pass | pass |
| Colour matrix | pass | pass | pass | pass | pass | pass |
| Narrow 16 → 8 bit | pass | pass | pass | pass | pass | pass |
| Tone curve, 4096-entry table | pass | pass | pass | pass | pass | pass |
| Tone curve, polynomial | pass | pass | pass | pass | pass | pass¹ |
| Interleave RGB24 | pass | pass | pass | pass | pass | pass |
| RGB → Y | pass | pass | pass | pass | pass | pass |
| RGB → UV (4:2:0) | pass | pass | pass | pass | pass | pass |
| 3A zone sums | pass | pass | pass | pass | pass | pass |
| Luma histogram | pass | pass | pass | pass | pass | pass |
| fp16 path (8 kernels) | pass² | gap³ | pass | n/a⁴ | pass⁵ | pass⁶ |

### Software ISP, output stages (`styx_softisp::post`)

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| RGB → YCbCr planes | pass | pass | pass | pass | pass | pass |
| Luma denoise (sigma filter) | pass | pass | pass | pass | pass | pass |
| Chroma denoise | pass | pass | pass | pass | pass | pass |
| Sharpen (unsharp mask) | pass | pass | pass | pass | pass | pass |
| Planes → output format | pass | pass | pass | pass | pass | pass |

### Packed conversions, pyramid and orientation (`styx_core::simd`)

The GPU column for the pyramid is `styx-wgpu`'s `BoxPyramid`; for the conversions (and the codec ones below) it is `styx_gpuisp::GpuConvert`.

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| BGR ↔ RGB (24-bit) | pass | pass | pass | pass | pass | pass |
| BGRA ↔ RGBA (32-bit) | pass | pass | pass | pass | pass | pass |
| XRGB32 → RGB24 | pass | pass | pass | pass | pass | pass |
| RGB → luma | pass | pass | pass | pass | pass | pass |
| RGB24 → RGBA | pass | pass | pass | pass | pass | pass |
| GREY → RGB24 | pass | pass | pass | pass | pass | pass |
| Y16 → RGB24 | pass | pass | pass | pass | pass | pass |
| RGB48 → RGB24 | pass | pass | pass | pass | pass | pass |
| YUYV → luma | pass | pass | pass | pass | pass | pass |
| 2x2 box (luma pyramid) | pass | pass | pass | pass | pass | pass |
| Row reverse (mirror) | pass | pass | pass | pass | pass | pass |
| 8x8 transpose (90° rotation) | pass | pass | pass | pass | pass | pass¹⁰ |

### Codec conversions (`styx-codec` raw decoders)

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| YUYV → NV12 chroma | pass | pass | pass | pass | pass | pass |
| NV12 → RGB | pass | lib | lib | pass⁷ | pass⁷ | pass |
| YUYV → RGB | pass | lib | lib | pass⁷ | pass⁷ | pass |
| I420 → RGB | pass | lib | lib | pass⁷ | pass⁷ | pass |

### Codecs

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| JPEG encode | pass | lib | lib | lib | pass⁸ | pass¹¹ |
| JPEG decode | pass | lib | lib | lib | gap⁸ | gap |
| H.264 / H.265 encode | pass | lib | lib | lib | pass⁸ | pass⁹ |

Notes:

1. The GPU's quadratics are the CPU's `IntPolyTone` (chosen by `Arithmetic::IntPolyTone`, the
   table where the fit misses), bit-exact on RADV, NVIDIA and llvmpipe (`tests/it/variants.rs`).
2. Emulated fp16 on CPUs without fp16 arithmetic. `Arithmetic::Auto` does not choose it there.
3. AVX-512 FP16 (Sapphire Rapids, Zen 5) has the arithmetic; F16C converts only. Build
   both, measure them, and keep them selectable.
4. The Cortex-A7 has no fp16 arithmetic.
5. RVV leaves with Zvfh (`simd::rvv::half`, the K230D's C908): the eight kernels (front end,
   RAW10 front, colour and half-size quad colour, luma and quad luma, quad statistics,
   `f32` to fp16), bit for bit with the oracle. They run only where the kernel reports Zvfh
   (`riscv_hwprobe`, Linux 6.5 and later; `AT_HWCAP` has no such letter), and
   `Arithmetic::Auto` still picks the integer path there. Conformance-tested under QEMU with
   `zvfh=true` at VLEN 128, 256 and 512 (`crates/softisp/src/simd/half_tests.rs`, run by
   `scripts/check-riscv.sh`). Written,
   untested on board: the C908's Zvfh, the probe's bit and the speed against the integer path are
   owed a board check.
6. GPUs have fp16 arithmetic: `full16` and `half16`, beside the integer shaders, opt-in
   (`GpuIsp::set_half_precision`) where the device has `shaderFloat16`. They run the front end,
   bilinear demosaic, colour matrix, tone curve and output in float16, within the CPU fp16 path's
   bounds (`tests/it/fp16.rs`). Not covered: the statistics (`quad_stats_row`) and the
   lens-shading row conversion (`from_f32_row`), which stay on the integer arithmetic.
7. yuvutils-rs has no armv7 or RVV code. There, Styx's own rows run instead
   (`styx_core::simd::{nv12,i420,yuyv}_rgb_row`), exact against scalar. They are tested under QEMU
   and written, untested on board.
8. Upstream libjpeg-turbo and x264 have no RVV code. JPEG encode: Styx's own encoder
   (`styx_codec::styx_jpeg`) with RVV leaves for JFIF colour (RGB → YCbCr 4:2:0) and the DCT
   with quantisation (`styx_core::simd::jpeg`), exact against the scalar oracle, tested under
   QEMU at VLEN 128, 256, 512 and 1024; Huffman coding is serial and scalar. Written, untested
   on board. H.264 encode: Styx's own encoder (`styx_codec::styx_h264`, Baseline CAVLC) with RVV
   leaves for motion-search SAD, the residual transform with quantisation and the
   reconstruction (`styx_core::simd::h264`), exact against the scalar oracle under QEMU at VLEN
   128, 256 and 1024; CAVLC is serial and scalar. Written, untested on board. H.265 has no RVV
   path beyond the board's VPU. JPEG decode still needs its own leaves, or the board's units
   (see the fixed-function table below).
9. VA-API (`h264_vaapi`, `hevc_vaapi`) on x86 GPUs. On any Vulkan device, Styx's H.264 encoder
   (`styx_codec::styx_h264::GpuStyxH264`, feature `gpu-h264`) starts each macroblock's motion
   search from the GPU's exhaustive whole-pel search (`shaders/h264_me.comp`, ±16, bit for bit
   `styx_core::simd::h264::full_search`); refinement and coding stay on the CPU. 1280x720 QP 26,
   11 px/picture of motion: CPU 25.0 ms and 3.49 MB for 20 pictures, RADV 16.3 ms and 0.35 MB,
   NVIDIA 17.6 ms, llvmpipe 125.7 ms. There is no Vulkan Video encode yet.
10. `GpuConvert::transform` gathers each output pixel rather than working on 8x8 tiles. It
    gives the bytes of `transform_packed` in all eight orientations, tested against it.
11. `GpuConvert::jpeg_blocks` (`shaders/jpeg.comp`): JFIF colour, the DCT and quantisation, one
    invocation per 8x8 block, bit-exact with `styx_core::simd::jpeg` (`tests/it/jpeg.rs`);
    Huffman coding stays on the CPU (`styx_codec::styx_jpeg::GpuStyxJpeg`, feature `gpu-jpeg`,
    the same bytes as the CPU encoder). 640x400 RGB at q70 on the dev PC: RADV (RX 6800 XT)
    1.41 ms p50, NVIDIA (RTX 4060) 6.3 ms, llvmpipe 3.9 ms, against 4.6 ms for the scalar CPU
    path. JPEG decode on the GPU is still a gap.

## Fixed-function units

| Unit (board) | Stages it takes | State |
|---|---|---|
| PiSP FE + BE (CM5) | unpack, front end, demosaic, colour matrix, tone, denoise, sharpen, YUV/RGB out, scaling (every pyramid level), 3A statistics | pass |
| Sensor flips (all) | mirror, vertical flip | pass (`HFLIP`/`VFLIP` controls) |
| Stateful V4L2 M2M encoders (Pi 4 bcm2835-codec, others) | H.264, H.265, JPEG encode | pass (`V4l2M2mEncoder`, FFmpeg `*_v4l2m2m`) |
| Stateful V4L2 M2M decoders | H.264, H.265 decode | pass (FFmpeg `*_v4l2m2m`) |
| VA-API (x86) | H.264, H.265 encode | pass |
| rkisp (RK3566, RK3562, RK3576) | the ISP stages and 3A statistics | gap: needs a board |
| rkisp (RV1106, Flame) | the ISP stages, pyramid levels | gap: needs a board and its BSP |
| ISP8000 via vvcam (K230D) | the ISP stages and 3A statistics | gap: needs a board |
| RGA (Rockchip 2D) | colour conversion, scaling, crop (no rotation yet) | written, untested on board: `styx_codec::RgaConverter` (feature `codec-rga`) over Daedalus's `daedalus-rga`, the one binding; the registry offers its format conversions as `CodecKind::Converter` and the planner picks them for raw-to-raw steps; scaling is not a planner stage yet |
| Stateless VPUs (Hantro, rkvenc) | JPEG, H.264 encode | gap: `V4l2M2mEncoder` is stateful only |
| Rockchip MPP (vendor `/dev/mpp_service`) | H.264, H.265, JPEG encode | written, untested on board: `RkMppEncoder` (`codec-rkmpp`) |
| K230D VPU and 2D unit | H.264, H.265, JPEG encode; rotation, scaling | written, untested on board: the V4L2 M2M codecs, frames imported by dma-buf |

## Totals

There are 43 stages, each in 6 columns, for 258 cells.

| Column | pass | gap | n/a |
|---|---|---|---|
| scalar | 43 | 0 | 0 |
| x86 | 42 | 1 | 0 |
| NEON64 | 43 | 0 | 0 |
| NEON32 | 42 | 0 | 1 |
| RVV | 40 | 3 | 0 |
| GPU | 41 | 2 | 0 |
| **all** | **251** | **6** | **1** |

Fixed-function units: 5 pass and 7 gaps, 6 of which need a board. "Written, untested on board"
rows count as gaps (the MPP, K230D and RGA rows), so the RGA row's change leaves the count as it was.

## Hardware blocks

This section takes each imaging block on the boards and lists each of its capabilities,
whether Styx uses it, and what does that job today when it doesn't. A software path of
"none" means nothing does the job yet.

States:

- **used**;
- **partly** (which part is missing is noted);
- **unused**;
- **n/a** (impossible or not applicable, with the reason).

### CM5: PiSP front end (FE)

| Capability | State | Software path today |
|---|---|---|
| AGC statistics (zones, histogram) | used | — |
| AWB statistics | used | — |
| CDAF statistics | used: the 8x8 grid moves to the AF windows' bounding box (`AfStatus::measure_area`, `Statistics::focus_area`; written, untested on board) | window weighting in `styx-algo` within the grid |
| Black level (BLA, BLC), DPC, decompand, decompress, LSC for statistics, RGBY, decimate, statistics crop | used | — |
| Output 0 (raw to memory, compressed) | used | — |
| Output 1 (cropped, downscaled raw: crop 1, downscale 1) | unused (builder-tested only) | none: no consumer wants low-resolution raw yet |

### CM5: PiSP back end (BE)

| Capability | State | Software path today |
|---|---|---|
| DPC, GEQ, SDN, BLC, LSC, WBG, CDN, demosaic | used | — |
| TDN, with compressed TDN buffers | used | — |
| Stitch (HDR merge of two exposures) | unused: the builder refuses it | none: no HDR mode |
| Tonemap (local tone mapping for HDR) | unused | the global gamma curve only |
| CAC (chromatic aberration correction) | unused: the builder refuses it | none |
| Debin | unused | none: binned modes run without it |
| CCM, YCbCr, false colour, sharpen, YCbCr inverse, gamma, CSC0/CSC1 | used | — |
| Saturation control | unused | the saturation is folded into the CCM |
| Output 0 and its downscale/resample | used | — |
| Output 1 and its scaler | used: one pyramid level, regions, overview | — |
| Downscaled pyramid levels | used: level 1 from output 1, each deeper level from an extra pass at its size (`pyramid_all_levels`, which the planner picks for a hardware pyramid; written, untested on board) | `box2_row` (`with_box_pyramid`) when the PiSP is not asked |
| Extra BE passes (regions) | used: one job at a time (CM5, 3 passes: 0.28 ms/frame), queued back to back refused (`pass_jobs`: 0.40 ms with shared indices, wedged `pisp_be` with fixed ones; driver fix written); regions scaled by the pass (`NativeRegion::size`) or in the second output's format | — |
| HOG output | unused | none |

### CM5: HEVC decoder

| Capability | State | Software path today |
|---|---|---|
| Stateless HEVC decode (`rpi-hevc-dec`) | partly: wired through FFmpeg's `v4l2request` hwaccel, which only Raspberry Pi's FFmpeg build has; not verified on the CM5 | FFmpeg software HEVC |

### K230D

| Block | Capability | State | Software path today |
|---|---|---|---|
| ISP8000 | demosaic, colour, tone, YUV out | unused (board) | RVV software ISP (integer path still scalar on RVV) |
| ISP8000 | 3A statistics | unused (board) | software ISP statistics |
| ISP8000 | scaled channels (pyramid levels) | unused (board) | `box2_row` (RVV) |
| VPU (`mvx`, V4L2 M2M) | H.264, H.265, JPEG encode | written, untested on board: `V4l2M2mEncoder`, frames imported by dma-buf | libjpeg-turbo (no SIMD on RISC-V), FFmpeg |
| 2D unit (`nonai-2d`, V4L2 M2M) | scale, convert, rotate | written, untested on board: `V4l2M2mConverter`, frames imported by dma-buf; no planner stage picks it yet | `styx_core::simd` (RVV) |
| DPU (structured-light depth) | depth maps | unused (board) | none: Styx has no depth output |

### Rockchip (RK3566, RK3562, RK3576, RK3588S)

| Block | Capability | State | Software path today |
|---|---|---|---|
| rkisp | main path (full ISP) | unused (board): parameter and statistics code exists for ISP30 only | software ISP |
| rkisp | 3A statistics | unused (board) | software ISP statistics |
| rkisp | self path (second scaled output) | unused (board) | `box2_row`, software scaling |
| RGA2 / RGA3 | scale, crop, convert (no rotation yet) | written, untested on board: `styx_codec::RgaConverter` (feature `codec-rga`) over Daedalus's `daedalus-rga` (mainline `rockchip-rga` over V4L2 M2M, RGA3 through librga) | `styx_core::simd` |
| MPP / rkvenc, Hantro | H.264, H.265, JPEG encode | written, untested on board: `RkMppEncoder`, ISP NV12 imported by dma-buf | libjpeg-turbo, FFmpeg |
| MPP / rkvdec | H.264, H.265 decode | unused (board) | FFmpeg software |
| IEP | denoise, enhancement (deinterlace n/a: cameras are progressive) | unused (board) | software ISP denoise |

### RV1106 (Flame)

| Block | Capability | State | Software path today |
|---|---|---|---|
| rkisp | full ISP and 3A statistics | unused (board) | software ISP (armv7 NEON integer path) |
| rkisp | selfpath scaler and 4x4/8x8 sampling outputs (pyramid) | unused (board) | `box2_row` (NEON) |
| rkvenc | H.264, H.265 encode | written, untested on board: `RkMppEncoder` | FFmpeg / libjpeg-turbo (NEON) |

### Hardware-block totals

These totals cover the capability rows above.

| Board | used | partly | unused | n/a |
|---|---|---|---|---|
| CM5 (PiSP, HEVC) | 12 | 1 | 7 | 0 |
| K230D | 0 | 0 | 6 | 0 |
| Rockchip | 0 | 0 | 7 | 0 |
| RV1106 | 0 | 0 | 3 | 0 |

The row that needs no new board is on the CM5: HEVC decode verified on the CM5.

HDR stitch and tonemap need a sensor with two exposures, and CAC needs calibration.

## Choosing the hardware blocks per device

`StyxConfig::accelerators` (`with_accelerators`, a `planner::Accelerators`) has one knob per stage,
each a `Hardware`: `Auto` (the default: the hardware where it exists), `Off` (the CPU path) or
`Required` (fail rather than fall back). A camera service applies its capture config's knobs to
every client that sets none; a direct plan takes them with `FrameRequest::accelerators`, and its
capture starts with them.

| Knob | `Off` | `Required` |
|------|-------|------------|
| `isp` | a native camera runs the software ISP | a native camera with only the software ISP is rejected, in the plan and at capture |
| `encoder` | software encoders only | hardware encoders only |
| `scaler` | no scaling in the ISP | a smaller `size` must come from the ISP |
| `pyramid` | levels from the CPU's box filter (levels asked of the GPU stay there) | levels from the ISP (`PyramidSource::HardwareOnly`) |

The request-wide `FrameRequest::hardware` still applies on top. The scaler knob covers the ISP's
scalers; the planner has no M2M scaling stage yet (`V4l2M2mConverter` is used directly, and
`RgaConverter` only converts formats in the planner), which the Rockchip RGA item in `TODO.md` adds.
