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
| RAW10 unpack | pass | pass | pass | gap | gap | pass |
| RAW12 unpack | pass | pass | pass | gap | gap | pass |
| Front end (black, WB, gain, lens shading) | pass | pass | pass | gap | gap | pass |
| Demosaic, bilinear | pass | pass | pass | gap | gap | pass |
| Demosaic, Malvar-He-Cutler | pass | pass | pass | gap | gap | pass |
| Demosaic, edge-directed | pass | gap | gap | gap | gap | gap |
| Bayer luma (luma-only output) | pass | pass | pass | gap | gap | pass |
| Half-size quad RGB | pass | pass | pass | gap | gap | pass |
| Half-size quad luma | pass | pass | pass | gap | gap | pass |
| Colour matrix | pass | pass | pass | gap | gap | pass |
| Narrow 16 → 8 bit | pass | pass | pass | gap | gap | pass |
| Tone curve, 4096-entry table | pass | pass | pass | gap | gap | pass |
| Tone curve, polynomial | pass | pass | pass | gap | gap | gap¹ |
| Interleave RGB24 | pass | pass | pass | gap | gap | pass |
| RGB → Y | pass | pass | pass | gap | gap | pass |
| RGB → UV (4:2:0) | pass | pass | pass | gap | gap | pass |
| 3A zone sums | pass | pass | pass | gap | gap | pass |
| Luma histogram | pass | pass | pass | gap | gap | pass |
| fp16 path (8 kernels) | pass² | gap³ | pass | n/a⁴ | gap⁵ | gap⁶ |

### Software ISP, output stages (`styx_softisp::post`)

| Stage | scalar | x86 | NEON64 | NEON32 | RVV | GPU |
|---|---|---|---|---|---|---|
| RGB → YCbCr planes | pass | gap | gap | gap | gap | gap |
| Luma denoise (sigma filter) | pass | gap | gap | gap | gap | gap |
| Chroma denoise | pass | gap | gap | gap | gap | gap |
| Sharpen (unsharp mask) | pass | gap | gap | gap | gap | gap |
| Planes → output format | pass | gap | gap | gap | gap | gap |

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
| PiSP FE + BE (CM5) | unpack, front end, demosaic, colour matrix, tone, denoise, sharpen, YUV/RGB out, scaling (pyramid output 1), 3A statistics | pass |
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
| x86 | 35 | 8 | 0 |
| NEON64 | 36 | 7 | 0 |
| NEON32 | 15 | 27 | 1 |
| RVV | 12 | 31 | 0 |
| GPU | 18 | 25 | 0 |
| **all** | **159** | **98** | **1** |

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
| CDAF statistics | partly: fixed 8x8 grid over the whole frame; `AF_WINDOWS` only weights its zones | window weighting in `styx-algo` |
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
| Downscaled pyramid levels | partly: one level from output 1 (or one extra pass); deeper levels are box-filtered on the CPU | `box2_row` (`with_box_pyramid`) |
| Extra BE passes (regions) | partly: run one after another; scaled or other-format regions are not planned | — |
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
| VPU (`mvx`, V4L2 M2M) | H.264, H.265, JPEG encode | unused (board): `V4l2M2mEncoder` should drive it | libjpeg-turbo (no SIMD on RISC-V), FFmpeg |
| 2D unit (`nonai-2d`, V4L2 M2M) | scale, convert, rotate | unused (board): `V4l2M2mConverter` should drive it | `styx_core::simd` (RVV) |
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
| rkisp | full ISP and 3A statistics | unused (board) | software ISP (integer path still scalar on armv7) |
| rkisp | selfpath scaler and 4x4/8x8 sampling outputs (pyramid) | unused (board) | `box2_row` (NEON) |
| rkvenc | H.264, H.265 encode | unused (board) | FFmpeg / libjpeg-turbo (NEON) |

### Hardware-block totals

These totals cover the capability rows above.

| Board | used | partly | unused | n/a |
|---|---|---|---|---|
| CM5 (PiSP, HEVC) | 9 | 4 | 7 | 0 |
| K230D | 0 | 0 | 6 | 0 |
| Rockchip | 0 | 0 | 7 | 0 |
| RV1106 | 0 | 0 | 3 | 0 |

The rows that need no new board are all on the CM5:

- every pyramid level from the PiSP (extra BE passes at each level's size);
- CDAF windows that follow `AF_WINDOWS`;
- queued BE passes and scaled regions;
- HEVC decode verified on the CM5.

HDR stitch and tonemap need a sensor with two exposures, and CAC needs calibration.
