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

The GPU column is `styx-gpuisp`, bit-exact with the CPU.

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| RAW10 unpack | pass | pass | pass | pass | pass | pass |
| RAW12 unpack | pass | pass | pass | pass | pass | pass |
| Front end (black, WB, gain, lens shading) | pass | pass | pass | pass | pass | pass |
| Demosaic, bilinear | pass | pass | pass | pass | pass | pass |
| Demosaic, Malvar-He-Cutler | pass | pass | pass | pass | pass | pass |
| Demosaic, edge-directed | pass | gap | gap | gap | gap | gap |
| Bayer luma (luma-only output) | pass | pass | pass | pass | pass | pass |
| Half-size quad RGB | pass | pass | pass | pass | pass | pass |
| Half-size quad luma | pass | pass | pass | pass | pass | pass |
| Colour matrix | pass | pass | pass | pass | pass | pass |
| Narrow 16 → 8 bit | pass | pass | pass | pass | pass | pass |
| Tone curve, 4096-entry table | pass | pass | pass | pass | pass | pass |
| Tone curve, polynomial | pass | pass | pass | pass | pass | gap¹ |
| Interleave RGB24 | pass | pass | pass | pass | pass | pass |
| RGB → Y | pass | pass | pass | pass | pass | pass |
| RGB → UV (4:2:0) | pass | pass | pass | pass | pass | pass |
| 3A zone sums | pass | pass | pass | pass | pass | pass |
| Luma histogram | pass | pass | pass | pass | pass | pass |
| fp16 path (8 kernels) | pass² | gap³ | pass | n/a⁴ | gap⁵ | gap⁶ |

### Software ISP, output stages (`styx_softisp::post`)

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| RGB → YCbCr planes | pass | pass | pass | pass | pass | gap |
| Luma denoise (sigma filter) | pass | pass | pass | pass | pass | gap |
| Chroma denoise | pass | pass | pass | pass | pass | gap |
| Sharpen (unsharp mask) | pass | pass | pass | pass | pass | gap |
| Planes → output format | pass | pass | pass | pass | pass | gap |

### Packed conversions, pyramid and orientation (`styx_core::simd`)

The GPU column for the pyramid is `styx-wgpu`'s `BoxPyramid`.

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| BGR ↔ RGB (24-bit) | pass | pass | pass | pass | pass | gap |
| BGRA ↔ RGBA (32-bit) | pass | pass | pass | pass | pass | gap |
| XRGB32 → RGB24 | pass | pass | pass | pass | pass | gap |
| RGB → luma | pass | pass | pass | pass | pass | gap |
| RGB24 → RGBA | pass | pass | pass | pass | pass | gap |
| GREY → RGB24 | pass | pass | pass | pass | pass | gap |
| Y16 → RGB24 | pass | pass | pass | pass | pass | gap |
| RGB48 → RGB24 | pass | pass | pass | pass | pass | gap |
| YUYV → luma | pass | pass | pass | pass | pass | gap |
| 2x2 box (luma pyramid) | pass | pass | pass | pass | pass | pass |
| Row reverse (mirror) | pass | pass | pass | pass | pass | gap |
| 8x8 transpose (90° rotation) | pass | pass | pass | pass | pass | gap |

### Codec conversions (`styx-codec` raw decoders)

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| YUYV → NV12 chroma | pass | gap | gap | gap | gap | gap |
| NV12 → RGB | pass | lib | lib | gap⁷ | gap⁷ | gap |
| YUYV → RGB | pass | lib | lib | gap⁷ | gap⁷ | gap |
| I420 → RGB | pass | lib | lib | gap⁷ | gap⁷ | gap |

### Codecs

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| JPEG encode | pass | lib | lib | lib | gap⁸ | gap |
| JPEG decode | pass | lib | lib | lib | gap⁸ | gap |
| H.264 / H.265 encode | pass | lib | lib | lib | gap⁸ | pass⁹ |

Notes:

1. The polynomial tone curve, as a GPU variant beside the table, to measure.
2. Emulated fp16 on CPUs without fp16 arithmetic. `Arithmetic::Auto` does not choose it there.
3. AVX-512 FP16 (Sapphire Rapids, Zen 5) has the arithmetic; F16C converts only. Build
   both, measure them, and keep them selectable.
4. The Cortex-A7 has no fp16 arithmetic.
5. Worth doing only if the K230D's C908 has Zvfh. Check that on the board.
6. GPUs have fp16 arithmetic: a `half` shader variant beside the bit-exact integer one.
7. yuvutils-rs has no armv7 or RVV code. Styx's own kernels, with all leaves, replace it there.
8. Upstream libjpeg-turbo and x264 have no RVV code. Styx needs its own colour and DCT
   leaves, or the board's encoder (see the fixed-function table below).
9. VA-API (`h264_vaapi`, `hevc_vaapi`) on x86 GPUs. There is no Vulkan Video encode yet.

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
| RGA (Rockchip 2D, a V4L2 M2M device on mainline) | colour conversion, scaling, rotation | gap: `V4l2M2mConverter` exists but the conversion stage does not pick it yet |
| Stateless VPUs (Hantro, rkvenc) | JPEG, H.264 encode | gap: `V4l2M2mEncoder` is stateful only |
| Rockchip MPP (vendor `/dev/mpp_service`) | H.264, H.265, JPEG encode | gap: needs a board |
| K230D VPU and 2D unit | H.264, H.265, JPEG encode; rotation, scaling | gap: needs a board |

## Totals

There are 43 stages, each in 6 columns, for 258 cells.

| Column | pass | gap | n/a |
|---|---|---|---|
| scalar | 43 | 0 | 0 |
| x86 | 40 | 3 | 0 |
| NEON64 | 41 | 2 | 0 |
| NEON32 | 37 | 5 | 1 |
| RVV | 34 | 9 | 0 |
| GPU | 18 | 25 | 0 |
| **all** | **213** | **44** | **1** |

Fixed-function units: 5 pass and 7 gaps, 6 of which need a board.

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
| RGA2 / RGA3 | scale, crop, convert, rotate | unused (board): mainline `rockchip-rga` fits `V4l2M2mConverter`; vendor multi-rga not written | `styx_core::simd` |
| MPP / rkvenc, Hantro | H.264, H.265, JPEG encode | unused (board) | libjpeg-turbo, FFmpeg |
| MPP / rkvdec | H.264, H.265 decode | unused (board) | FFmpeg software |
| IEP | denoise, enhancement (deinterlace n/a: cameras are progressive) | unused (board) | software ISP denoise |

### RV1106 (Flame)

| Block | Capability | State | Software path today |
|---|---|---|---|
| rkisp | full ISP and 3A statistics | unused (board) | software ISP (armv7 NEON integer path) |
| rkisp | selfpath scaler and 4x4/8x8 sampling outputs (pyramid) | unused (board) | `box2_row` (NEON) |
| rkvenc | H.264, H.265 encode | unused (board) | FFmpeg / libjpeg-turbo (NEON) |

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
scalers; the planner has no M2M scaling stage yet (`V4l2M2mConverter` is used directly), which
the Rockchip RGA item in `TODO.md` adds.
