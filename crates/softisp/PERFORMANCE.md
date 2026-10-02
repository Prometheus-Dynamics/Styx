# Performance

One 1280x800 RAW10 frame (`pBAA`, the OV9782), one thread unless noted; medians of
`cargo bench -p styx-softisp` (`benches/softisp.rs`). **CM5**: Raspberry Pi CM5, Cortex-A76 at
2.4 GHz, NEON (cross-compiled with `-C target-cpu=cortex-a76`), idle. **x86**: Ryzen 9 5900X,
AVX2, shared with heavy other work while measuring (load 12-40 on 24 threads; the better of
two runs): treat its numbers as good to about 20%. "Before" is the crate as of the native-stack
branch at `582d885`.

## Stages (a whole frame's rows, from rows in cache)

| Stage | Kernel | CM5 before | CM5 | x86 |
|---|---|---:|---:|---:|
| Unpack CSI-2 RAW10 | `unpack_raw10_row` | 0.22 ms | 0.22 ms | 0.07 ms |
| Black level + gains (+ lens shading gains) | `front_row` | 0.25 ms | 0.25 ms | 0.04 ms |
| Demosaic, bilinear | `demosaic_bilinear_row` | 0.60 ms | 0.61 ms | 0.12 ms |
| Demosaic, Malvar-He-Cutler | `demosaic_mhc_row` | 2.34 ms | 2.36 ms | 0.46 ms |
| Colour matrix | `ccm_row` | 1.07 ms | 1.06 ms | 0.12 ms |
| Tone curve, 3 channels | `lut_row` | 2.31 ms | 1.54 ms | 0.96 ms |
| 12 to 8 bit without tone curve, 3 channels | `narrow_row` | 0.21 ms | 0.21 ms | 0.09 ms |
| Planar to packed RGB24 | `interleave_rgb_row` | 0.14 ms | 0.14 ms | 0.09 ms |
| RGB to NV12 (Y and 4:2:0 UV) | `rgb_to_y_row`, `rgb_to_uv_row` | 0.41 ms | 0.41 ms | 0.12 ms |
| Luma from the mosaic (3x3 binomial) | `bayer_luma_row` | 0.50 ms | 0.50 ms | 0.13 ms |
| Quads to half-size RGB | `quad_rgb_row` | 0.11 ms | 0.11 ms | 0.04 ms |

In place, on the pipeline's settings (lens shading 32x32, statistics 16x12 zones and 256 bins
every second quad row, sRGB, a recorded frame), removing one stage at a time on the CM5:
statistics 0.39 ms (0.57 ms before the vector histogram bins, `luma_bins_row`), lens shading
0.33 ms, colour matrix 1.06 ms, tone curve 1.33 ms; without all of them (and without black
level and white balance) 1.84 ms remain of 4.97 ms. Building the lens shading tables
(`set_params`, on every frame whose white balance or digital gain changed) takes 0.47 ms
(3.19 ms before).

## End to end (`SoftIsp::process`)

"Tuned" is black level, white balance, colour matrix and the sRGB curve; "plain" is the
default parameters (no black level, unit gains, no matrix, linear). The input sits in cached
memory here; see below for uncached input.

| Output | CM5 before | CM5 | x86 before | x86 |
|---|---:|---:|---:|---:|
| RGB24, plain | 1.68 ms | 1.77 ms | 0.44 ms | 0.54 ms |
| RGB24, tuned | 4.86 ms | 4.20 ms | 1.47 ms | 1.51 ms |
| NV12, tuned | 5.07 ms | 4.41 ms | 1.48 ms | 1.60 ms |
| NV12, tuned + statistics (16x12 zones, 256-bin histogram) | 6.26 ms | 5.16 ms | 2.01 ms | 1.93 ms |
| NV12, tuned + lens shading + statistics | 6.66 ms | 5.48 ms | 2.34 ms | 2.22 ms |
| RGB24, tuned, MHC demosaic | 6.69 ms | 6.15 ms | 1.82 ms | 1.93 ms |
| NV12, tuned, MHC demosaic | 6.85 ms | 6.17 ms | 1.82 ms | 1.92 ms |
| Luma, plain | 1.16 ms | 1.23 ms | 0.27 ms | 0.30 ms |
| Luma, tuned (with the sRGB curve) | 1.87 ms | 1.70 ms | 0.62 ms | 0.63 ms |
| Half size (640x400) RGB24, tuned | 1.54 ms | 1.41 ms | 0.47 ms | 0.50 ms |
| Half size NV12, tuned | 1.60 ms | 1.46 ms | 0.63 ms | 0.57 ms |
| Half size luma, plain | 0.63 ms | 0.68 ms | 0.15 ms | 0.17 ms |
| NV12, tuned + lens shading + statistics, 2 threads | - | 2.76 ms | - | 1.36 ms |
| NV12, tuned + lens shading + statistics, 4 threads | - | 1.57 ms | - | 0.73 ms |
| NV12, tuned + statistics, 4 threads (before: `rayon`) | 1.72 ms | 1.48 ms | 0.77 ms | 0.64 ms |
| NV12, tuned, MHC, 4 threads (before: `rayon`) | 3.22 ms | 1.73 ms | 0.70 ms | 0.67 ms |

The plain cases lose 0.05-0.1 ms to the input copy below, which pays for itself many times
over when the input is uncached.

## Uncached input

Receivers' V4L2 MMAP buffers are often mapped uncached (write-combined) into the process, as
`rp1-cfe`'s are on the CM5. The unpack kernels read with small overlapping loads, each a bus
transaction there: the pipeline's frame took 10.7 ms instead of 5.9 ms. Input rows are now
copied 16 KiB at a time into a cached buffer first (`SoftIsp::set_copy_input`, on by default):
5.6 ms from write-combined memory, 5.0 ms from cached memory (a single `memcpy` of the whole
frame out of write-combined memory takes 0.78 ms; reading it with the kernels' loads cost
about 5 ms). With 2 / 3 / 4 threads the frame takes 2.8 / 1.9 / 1.5 ms from write-combined
memory: the threads' copies overlap.

## Threads

`SoftIsp::with_threads(n)` runs the calling thread and `n - 1` helper threads the ISP starts on
first use and keeps, asleep on a condition variable between frames (no per-frame spawn, no
rayon; the `rayon` feature is a no-op kept for compatibility). The frame is cut into two row
bands per thread, claimed in order by whichever thread is free; each band recomputes the one
or two front-end rows above and below it. On the CM5 the pipeline's frame scales 1 : 1.9 :
2.8 : 3.4 over 1 to 4 threads; in a 30 fps camera run four threads cost 0.4 ms more CPU per
frame than one.

## Notes

* The tone curve is a 4096-entry table (the 257-node curve expanded). NEON clamps eight
  samples in a vector, moves them to two general registers and looks the eight bytes up with
  shifts and masks, storing them as one word: 1.5x the scalar loop. Interpolating the nodes
  with `tbl` (eight 4-register lookups per 16 pixels) is slower than scalar loads on the A76
  (3.1 ms for three channels); AVX2 gathers measured 2x slower than scalar loads on Zen 3, so
  x86 stays scalar, where the curve is two thirds of a tuned frame.
* A fused colour row (demosaic, matrix and tone table for 8 pixels in registers, no 16-bit
  rows in between) was no faster on the A76 (5.20 vs 5.17 ms RGB24, NV12 slower), in one loop
  or in 64-pixel chunks: the path is bound by instruction throughput (the A76 issues integer
  vector multiplies on one pipe), not by L1 traffic, and one long dependency chain per 8 pixels
  starves the out-of-order window. LLVM also turns table bytes headed for a vector register
  into eight dependent lane loads unless the move goes through `asm!`. The rows between stages
  stay in L1 (a ring of eight front-end rows and a few output rows per thread).
* Statistics: zone sums in vectors, histogram bins in vectors (`luma_bins_row`), counting in
  four interleaved copies of the histogram. `StatsConfig::row_step` 2 halves their cost.
* Output is unchanged bit for bit by all of the above (`tests/golden.rs`).
