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
- **n/a**: impossible or meaningless, with the reason given.

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
| Tone curve, polynomial | pass | pass | pass | gap | gap | n/a¹ |
| Interleave RGB24 | pass | pass | pass | gap | gap | pass |
| RGB → Y | pass | pass | pass | gap | gap | pass |
| RGB → UV (4:2:0) | pass | pass | pass | gap | gap | pass |
| 3A zone sums | pass | pass | pass | gap | gap | pass |
| Luma histogram | pass | pass | pass | gap | gap | pass |
| fp16 path (8 kernels) | pass² | n/a³ | pass | n/a⁴ | gap⁵ | n/a⁶ |

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

1. The polynomial tone curve is an AVX2 speed variant of the same curve. The GPU computes
   that curve from the table.
2. Emulated fp16 on CPUs without fp16 arithmetic. `Arithmetic::Auto` does not choose it there.
3. x86 has fp16 arithmetic only from AVX-512 FP16; F16C only converts. `Auto` picks the
   integer or polynomial path, which is faster on x86 without it.
4. The Cortex-A7 has no fp16 arithmetic.
5. Worth doing only if the K230D's C908 has Zvfh. Check that on the board.
6. The GPU runs the integer path, bit-exact. fp16 is a CPU speed choice.
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
| x86 | 35 | 7 | 1 |
| NEON64 | 36 | 7 | 0 |
| NEON32 | 15 | 27 | 1 |
| RVV | 12 | 31 | 0 |
| GPU | 18 | 23 | 2 |
| **all** | **159** | **95** | **4** |

Fixed-function units: 5 pass and 7 gaps, 6 of which need a board.
