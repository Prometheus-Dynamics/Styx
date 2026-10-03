# Performance

One 1280x800 RAW10 frame (`pBAA`, the OV9782), one thread unless noted; medians of
`cargo bench -p styx-softisp` (`benches/softisp.rs`). **CM5**: Raspberry Pi CM5, Cortex-A76 at
2.4 GHz (performance governor), NEON and FP16 arithmetic, generic `aarch64` build (no
`target-cpu`; the fp16 kernels are chosen at run time), idle. "Before" is the native-stack
branch at `0d8638a` (the integer path after the first optimisation round, whose notes are kept
below). The live camera figures (CPU per frame of the whole process, latency) are in
`docs/native-stack/pipeline.md`, "Software path performance".

## What the Cortex-A76 does fast

Measured throughputs (cycles per instruction, independent instructions, `asm!` loops):

| | cycles |
|---|---:|
| fp16 `fmla` / `fadd` / `fmax`, 8 lanes | 0.5 |
| `tbl` 1-2 registers / 3 registers / 4 registers | 0.5 / 1 / 1.5 |
| `tbx` 4 registers | 2.5 |
| 8-bit widening multiply (`umull`, 8 lanes) | 1 |
| 16-bit multiply (`mul`, `sqrdmulh`, 8 lanes) | 2 (one pipe, half rate) |
| `ucvtf` / `fcvtnu` between u16 and fp16, 8 lanes | 4 |
| shifts, narrowing shifts | 1 (one pipe) |
| add, logic, `bsl`, `uzp`, `zip` | 0.5 |

16-bit integer arithmetic runs 4 multiply lanes a cycle and fp16 16, but converting to fp16
costs more than the multiplies it saves. Hence [`Arithmetic::Half`](crate::Arithmetic):

* **Front end without conversions.** A 10-bit sample ORed into the mantissa of 1024.0 *is* the
  fp16 `1024 + v`: black level, white balance x digital gain, lens shading (`a + d t` per
  column, one multiply-add) and the clamp are then fp16 operations, fused with the RAW10
  unpacking (`front_raw10_row`). Lens shading tables no longer carry the channel gains, so a
  white balance or gain change costs 12 us of `set_params` instead of a 0.3 ms rebuild.
* **Demosaic and colour matrix in one pass.** The bilinear demosaic's three sums per pixel go
  straight into the matrix, its averaging weights folded into per-column-parity
  coefficients: nine `fmla` per 8 pixels.
* **Tone curve from the fp16 bits.** The matrix output plus 16 (full scale is 4080, so white
  plus 16 is 4096, a segment boundary) has its exponent and two mantissa bits in the high
  byte: 48 segments, four per octave (1 code wide just above black, 512 at the top), looked
  up with two 3-register `tbl`s and interpolated with the low byte by an 8-bit multiply. Their
  bases and slopes are fitted to the curve (850 curve evaluations, 36 us, cached per curve).
  This replaces the 4096-entry table read with scalar loads (1.5 ms for three channels).
* **4:2:0 output from registers.** The first row of a pair writes its planes and luma; the
  second only luma and the pair's chroma, from the first row's planes and its own registers.
* **Statistics** come from the fp16 front rows (quads converted with `fcvtnu`, scaled to the
  integer path's 12-bit range so that both report the same sums).

The integer path is unchanged (bit for bit) and remains the reference, the x86 path and the
path of CPUs without FP16 arithmetic (Cortex-A72, A53: Raspberry Pi 4, 3).

## Stages

| | Kernel | CM5 |
|---|---|---:|
| Front end: unpack RAW10, black level, gains, clamp (fp16) | `half::front_raw10_row` | 0.44 ms |
| (integer: unpack, front end) | `unpack_raw10_row`, `front_row` | 0.23 + 0.25 ms |
| Front end from unpacked rows, with lens shading (fp16) | `half::front_row` | 0.35 ms (0.27 without) |
| Demosaic, matrix, tone curve, packed RGB24 (fp16) | `half::colour_row` | 1.60 ms |
| (integer: demosaic, matrix, tone curve x3, interleave) | | 0.58 + 1.08 + 1.51 + 0.14 ms |
| Luma from the mosaic with the tone curve (fp16) | `half::luma_row` | 0.78 ms |
| Statistics quads, every 4th quad row (fp16; before the zone sums) | `half::quad_stats_row` | 0.06 ms |
| `set_params`: new gains / new tone curve / new lens shading grid (fp16) | | 0.012 / 0.049 / 0.31 ms |
| `set_params`, any change (integer: the gains are in the lens shading tables) | | 0.31 ms |

## End to end (`SoftIsp::process`)

"Tuned" is black level, white balance, colour matrix and the sRGB curve; "lsc" lens shading
16x12; "stats" 16x12 zones with a 256-bin histogram on every quad row (the pipeline takes every
fourth: `row_step` 4, 0.23 instead of 0.92 ms). Input in cached memory, staged as below.

| Output | CM5 before | CM5 now | x86 (integer, unchanged) |
|---|---:|---:|---:|
| RGB24, plain (integer in both: no matrix, no curve) | 1.74 ms | 1.74 ms | 0.54 ms |
| RGB24, tuned | 4.10 ms | 2.29 ms | 1.51 ms |
| RGB24, tuned + lsc | | 2.43 ms | |
| RGB24, tuned + lsc + stats on every 4th quad row | | 2.67 ms | |
| NV12, tuned | 4.34 ms | 2.60 ms | 1.60 ms |
| NV12, tuned + lsc + stats (every row) | 5.39 ms | 3.61 ms | 2.22 ms |
| NV12, tuned + lsc + stats, 2 / 4 threads | 2.73 / 1.44 ms | 1.83 / 1.00 ms | |
| Luma, tuned | 1.65 ms | 1.39 ms | 0.63 ms |
| Half size RGB24, tuned | 1.40 ms | 0.95 ms | 0.50 ms |
| Half size NV12, tuned | 1.46 ms | 1.05 ms | 0.57 ms |
| Half size luma, plain | 0.69 ms | 0.66 ms | 0.17 ms |
| RGB24 / NV12, tuned, MHC demosaic (integer in both) | 5.79 / 5.97 ms | 5.81 / 5.99 ms | 1.93 ms |

[`Arithmetic::Auto`](crate::Arithmetic) runs fp16 only with a colour matrix or a tone curve:
without them the integer path does less (no matrix, a plain narrowing; 1.74 against 2.29 ms
for plain RGB24). The MHC demosaic, RAW12 and inputs above 10 bits stay integer.

## Quality

fp16 against the integer reference, PSNR and largest difference in 8-bit codes:

* **Recorded frames** (55 OV9782 frames processed with the HeliOS tuning's settings as the 3A
  loop applied them: lens shading, CCM, adaptive contrast curve; `native-pipeline quality`):
  R 53.9 dB, G 54.0 dB, B 54.0 dB, NV12 Y 54.6 dB, UV 55.3 dB; largest difference 2 codes, one
  sample in a million more than 1 code apart. Five more frames of the recording (the dark
  start, digital gain 3.7-4) need white balance x digital gain x lens shading above 16 in the
  corners, which the integer path's Q12 gains clip and fp16 applies: they are left out.
* **Synthetic chart** (`tests/quality.rs`: colour patches, ramps, a zone plate, clipped
  highlights and noise; three CFA orders, sRGB and a Raspberry Pi contrast curve, with and
  without lens shading, both scales, RGB24 and NV12): 54.7-61.4 dB; at most 2 codes on 99.9%
  of the samples of every channel (the test's bound), at most 2 codes everywhere with the
  contrast curve; up to 5 codes on 0.08% of the samples with the sRGB curve, where the matrix
  subtracts a clipped channel from a dark one (the large terms round to 1-4 units in fp16, to
  half a unit in the integer path, and sRGB's steep start magnifies both).
* **Statistics**: per-zone means within 0.05%; zone sums differ where a quad near the
  saturation threshold falls on the other side (up to 5% of a zone with few counted quads).
* **Pinned**: `tests/golden.rs` pins both arithmetics' output bit for bit (the integer hashes
  are those of the code before this work); the fp16 hashes are the same from the scalar
  oracle on x86 and from the FP16 leaves on the A76.

## Uncached input

Receivers' V4L2 MMAP buffers are often mapped uncached (write-combined) into the process, as
`rp1-cfe`'s are on the CM5. The unpack kernels read with small overlapping loads, each a bus
transaction there: the pipeline's frame took 10.7 ms instead of 5.9 ms. Input rows are copied
16 KiB at a time into a cached buffer first (`SoftIsp::set_copy_input`, on by default). The
software path now captures into cached CMA dma-heap buffers by default (0.6 ms less per frame
than from the MMAP buffers); the staging copy stays on for them too: reading them directly
with the kernels' loads measured 1 ms slower per frame.

## Threads

`SoftIsp::with_threads(n)` runs the calling thread and `n - 1` helper threads the ISP starts on
first use and keeps, asleep on a condition variable between frames. The frame is cut into two
row bands per thread, claimed in order by whichever thread is free; each band recomputes the one
or two front-end rows above and below it. With fp16, four threads take a 1280x800 NV12 frame in
1.0 ms but, live, cost 0.6 ms more CPU than one thread (RGB24: 1.4 ms more): the four cores
share the memory bandwidth for the 1.3 MB input and the 1.5-3 MB output.

## Notes: tried and dropped

This round (fp16):

* A tone curve of 8 segments per octave (a 4-register `tbl` and a clamp) was more accurate but
  1.3x slower than 4 per octave; 32 segments (2-register `tbl`) need a clamp on the fp16 value
  that costs what the cheaper lookup saves.
* An fp16 colour matrix on converted inputs (`ucvtf`): 0.66 ms for the matrix alone, bound by
  the 4-cycle conversions; folding the demosaic into the matrix avoids them.
* `vqrdmlah` (ARMv8.1 RDM) for the integer matrix: 1.05 against 1.15 ms, not worth a path.
* Finishing rows with the scalar oracle: 50 ns a pixel in software fp16, 0.7 ms per frame for
  the RAW10 front end's last group of each row. Leaves now end with an overlapping block.
* The end-of-row block as a closure, and one loop matching the output kind per block: both
  spilled the colour kernel's tables and coefficients (2.1 instead of 1.6 ms); a macro and one
  loop per kind keep them in registers.
* Chroma as 8-bit widening multiplies split by coefficient sign: slower than the 16-bit
  multiplies as compiled (+0.3 ms per NV12 frame).
* Reading the CMA capture buffers directly instead of staging them: 1 ms slower.
* Luma computed in the colour kernel saves little against a separate pass over L1 rows (kept:
  it also lets the second row of a pair skip its planes).

First round (integer path):

* The tone curve is a 4096-entry table (the 257-node curve expanded). NEON clamps eight
  samples in a vector, moves them to two general registers and looks the eight bytes up with
  shifts and masks, storing them as one word: 1.5x the scalar loop. Interpolating the nodes
  with `tbl` (eight 4-register lookups per 16 pixels) is slower than scalar loads on the A76
  (3.1 ms for three channels); AVX2 gathers measured 2x slower than scalar loads on Zen 3, so
  x86 stays scalar, where the curve is two thirds of a tuned frame.
* A fused integer colour row (demosaic, matrix and tone table for 8 pixels in registers) was
  no faster on the A76 (5.20 vs 5.17 ms RGB24): bound by the integer multiplies on one pipe at
  half rate (see the table above) and the scalar table lookups.
* Statistics: zone sums in vectors, histogram bins in vectors (`luma_bins_row`), counting in
  four interleaved copies of the histogram. `StatsConfig::row_step` 2 halves their cost, 4
  quarters it.
