# Performance

One 1280x800 RAW10 frame (`pBAA`, the OV9782), one thread unless noted; medians of
`cargo bench -p styx-softisp --features rayon` (`benches/softisp.rs`). **CM5**: Raspberry Pi
CM5, Cortex-A76 at 2.4 GHz, NEON (cross-compiled with `-C target-cpu=cortex-a76`). **x86**:
Ryzen 9 5900X, AVX2. Both machines were shared with other work while measuring (the x86 host
heavily), so treat the numbers as upper bounds.

## Stages (a whole frame's rows, from rows in cache)

| Stage | Kernel | CM5 | x86 |
|---|---|---:|---:|
| Unpack CSI-2 RAW10 | `unpack_raw10_row` | 0.23 ms | 0.07 ms |
| Black level + gains (+ lens shading gains) | `front_row` | 0.26 ms | 0.06 ms |
| Demosaic, bilinear | `demosaic_bilinear_row` | 0.63 ms | 0.14 ms |
| Demosaic, Malvar-He-Cutler | `demosaic_mhc_row` | 2.41 ms | 0.44 ms |
| Colour matrix | `ccm_row` | 1.10 ms | 0.13 ms |
| Tone curve, 3 channels (scalar table) | `lut_row` | 2.37 ms | 1.01 ms |
| 12 to 8 bit without tone curve, 3 channels | `narrow_row` | 0.21 ms | 0.09 ms |
| Planar to packed RGB24 | `interleave_rgb_row` | 0.14 ms | 0.09 ms |
| RGB to NV12 (Y and 4:2:0 UV) | `rgb_to_y_row`, `rgb_to_uv_row` | 0.42 ms | 0.14 ms |
| Luma from the mosaic (3x3 binomial) | `bayer_luma_row` | 0.51 ms | 0.10 ms |
| Quads to half-size RGB | `quad_rgb_row` | 0.12 ms | 0.03 ms |

## End to end (`SoftIsp::process`)

"Tuned" is black level, white balance, colour matrix and the sRGB curve; "plain" is the
default parameters (no black level, unit gains, no matrix, linear).

| Output | CM5 | x86 |
|---|---:|---:|
| RGB24, plain | 1.92 ms | 0.50 ms |
| RGB24, tuned | 5.26 ms | 1.80 ms |
| NV12, tuned | 5.46 ms | 1.70 ms |
| NV12, tuned + statistics (16x12 zones, 256-bin histogram) | 6.72 ms | 2.20 ms |
| NV12, tuned + lens shading + statistics | 7.16 ms | 2.54 ms |
| RGB24, tuned, MHC demosaic | 7.11 ms | 1.95 ms |
| NV12, tuned, MHC demosaic | 7.34 ms | 2.12 ms |
| Luma, plain | 1.26 ms | 0.32 ms |
| Luma, tuned (with the sRGB curve) | 1.99 ms | 0.63 ms |
| Half size (640x400) RGB24, tuned | 1.66 ms | 0.55 ms |
| Half size NV12, tuned | 1.69 ms | 0.56 ms |
| Half size luma, plain | 0.67 ms | 0.17 ms |
| NV12, tuned + statistics, 4 threads (`rayon`) | 2.95 ms | 1.05 ms |
| NV12, tuned, MHC, 4 threads (`rayon`) | 3.15 ms | 1.01 ms |

The tone curve is the largest single cost on the A76: three table lookups per pixel, which
NEON cannot vectorise (a `tbl`-based interpolation of the 257-node curve measured slower than
scalar loads). Skip it (`tone: None`) for linear output, or ask for luma, which needs one
lookup per pixel. Statistics add about 1.3 ms, most of it the per-quad histogram; set
`StatsConfig::row_step` to 2 to halve that.
