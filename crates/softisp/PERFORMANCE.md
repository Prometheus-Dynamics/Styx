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

The integer path is unchanged (bit for bit) and remains the reference and the path of CPUs
without FP16 arithmetic (Cortex-A72, A53: Raspberry Pi 4, 3); on x86 `Auto` runs it with the
tone curve as fixed-point quadratics (see "x86: the tone curve").

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

| Output | CM5 before | CM5 now | x86 (integer, before the x86 round below) |
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

## Regions and binned overviews

`SoftIsp::process_window` makes a window of the frame from its rows and columns plus the
demosaic's neighbours (front rows from a multiple of 4 columns; 202 front rows for a 200-row
window inside the frame), the same pixels as that window of the whole picture bit for bit
(`tests/regions.rs`); `process_binned` makes the whole frame binned by an even factor with the
statistics (the front end on 2/`factor` of the rows); `statistics` gathers them alone (the front
end on the quad rows they sample). One 1280x800 RAW10 frame, NV12, lens shading, statistics on
every fourth quad row, one thread (`benches/regions.rs`):

| | CM5 | x86 (Zen 3) |
|---|---:|---:|
| whole frame | 2.92 ms | 1.47 ms |
| window 320x200 / 640x400 | 0.19 / 0.69 ms | 0.10 / 0.37 ms |
| binned by 2 / 4 / 8, with the statistics | 1.36 / 0.82 / 0.45 ms | 0.74 / 0.45 / 0.24 ms |
| binned by 2 / 4 / 8, without | 1.13 / 0.60 / 0.22 ms | 0.67 / 0.34 / 0.14 ms |
| statistics alone | 0.37 ms | 0.18 ms |
| window 320x200 + binned by 4 | 1.01 ms | 0.52 ms |

The statistics' zone sums and histogram (0.23 ms on the CM5) are what a frame's statistics
cost wherever they come from. A 4:2:0 pair's statistics are gathered straight after each row
of the pair, while its front rows are still in the ring (binned by 8, the second row's rows
evicted the first's: 0.59 → 0.45 ms).

## Uncached input

Receivers' V4L2 MMAP buffers are often mapped uncached (write-combined) into the process, as
`rp1-cfe`'s are on the CM5. The unpack kernels read with small overlapping loads, each a bus
transaction there: the pipeline's frame took 10.7 ms instead of 5.9 ms. Input rows are copied
16 KiB at a time into a cached buffer first (`SoftIsp::set_copy_input`, on by default). The
software path captures into cached CMA dma-heap buffers by default (0.6 ms less per frame
than from the MMAP buffers) and reads those in place: `SoftPipeline` turns the staging copy
off for dma-heap buffers, 0.15 ms less CPU per 1280x800 frame live (ISP 2.74 -> 2.58 ms). An
earlier measurement had the direct reads 1 ms slower; on the current kernel and with the
fp16 front end they are faster (`STYX_SOFT_COPY=1` in `native-pipeline soft` compares).

## x86: the tone curve

The integer path's tone curve is a 4096-entry table (257 nodes interpolated), and on x86 a
table lookup per sample is all scalar loads: two thirds of a tuned 1280x800 frame on Zen 3.
Two kernels now (Ryzen 9 5900X, AVX2, no AVX-512; medians of the benches on a busy host):

* **Exact, AVX2** (`Arithmetic::Int`): both nodes of every sample (`n[i]`, `n[i + 1]`, 256
  entries each) looked up with `vpshufb` cascades of sixteen 16-entry tables (the index
  lowered by 16 per step; steps below the index's block XOR in differences that telescope,
  steps above it have the top bit set and give 0; the upper half on `i ^ 0x80`, a blend), the
  interpolation one `vpmaddubsw`. Bit for bit the table. Bound by the shuffle and logic ops (16
  shuffles per 32 bytes per node array): 1.05 -> 0.81 ms for three channels of a frame.
  Unrolled, LLVM copied the 32 tables to the stack and spilled the 16 indices (1.4x slower):
  the level loop's trip count is opaque to it (`black_box`), and two 32-pixel blocks share
  each table load.
* **Quadratics, fixed point** (`Arithmetic::IntPolyTone`, picked by `Auto` on x86 with AVX2):
  the table as one quadratic per half octave of `x + 32` (14 segments), 16-bit fixed point:
  the octave from two 16-entry byte tables (`v >> 4`, `v >> 8`), the position `t` within it by
  a multiply with a looked-up power of two, the coefficients with one `vpshufb` per half and
  a blend, Horner with `vpmulhrsw` and saturating adds. Fitted by least squares (reweighted
  towards the minimax fit where a segment misses by more than a code) and used only if all
  4096 inputs land within one code of the table: the sRGB, gamma 1.8-2.2 and Raspberry Pi
  contrast curves do, and all 150 adaptive contrast curves of the recorded OV9782 session
  (whole octaves missed by 2 codes on 149 of them); gamma 3 does not and keeps the table.
  1.05 -> 0.58 ms for three channels; the fit costs 26 us when the curve changes. Against
  `Int`: at most 1 code everywhere, 81-93% of the samples equal (`tests/quality.rs`,
  RGB24/NV12/luma). Its scalar oracle, AVX2 and NEON leaves agree bit for bit.

| x86 (Zen 3) | before | now |
|---|---:|---:|
| tone curve, three channels of 1280x800 (scalar / AVX2 exact / quadratics) | 0.99 ms | 1.05 / 0.81 / 0.58 ms |
| RGB24, tuned (`Auto`: quadratics) | 1.59 ms | 1.11 ms |
| RGB24, tuned, `Int` (exact AVX2 table) | 1.63 ms | 1.35 ms |
| NV12, tuned | 1.56 ms | 1.13 ms |
| NV12, tuned + lsc + stats (`Auto` / `Int`) | 2.26 / 2.28 ms | 1.69 / 1.97 ms |
| luma, tuned | 0.64 ms | 0.46 ms |
| half size RGB24, tuned | 0.50 ms | 0.38 ms |

Tried: an fp32 version of the quadratics (8 octaves, coefficients by `vpermps` from the float's
exponent, FMA): 0.61 ms, bound by the lane-crossing `vpermps` (and `vpmovzxwd`) sharing the
shuffle unit; the 16-bit form looks its coefficients up in-lane. Seven whole-octave segments
ran at 0.43 ms but missed the adaptive contrast curves by 2 codes. AVX-512 VBMI (`vpermi2b`,
a 128-entry byte lookup per instruction) would make the exact table two lookups and a blend
per node array; the host has no AVX-512, so it is not written.

## Cortex-A72 class (no FP16 arithmetic): the integer path

Without FP16 (Raspberry Pi 4) the integer path runs. Measured on the CM5 (A76) with the
integer path forced, built generic and with `-C target-cpu=cortex-a72` (A76 timings with
A72-scheduled code: no A72 was at hand, and the A72's narrower core will be slower):

| CM5, `Arithmetic::Int` | generic before | generic now | `cortex-a72` before | `cortex-a72` now |
|---|---:|---:|---:|---:|
| RGB24 tuned (bench) | 4.16 ms | 4.19 ms | 4.23 ms | 4.27 ms |
| NV12 tuned + lsc + stats (bench) | 5.45 ms | 5.42 ms | 5.51 ms | 5.51 ms |
| `set_params`, new gains (rebuilds the shading tables) | 0.31 ms | 0.21 ms | 0.31 ms | 0.21 ms |
| live RGB24, CPU per frame (ISP / settings) | | | 5.48 ms (4.87 / 0.34) | 5.33 ms (4.83 / 0.25) |
| live NV12, CPU per frame (ISP / settings) | | | 5.54 ms (5.00 / 0.34) | 5.39 ms (4.96 / 0.25) |

The kernels' code is the same either way (A72 scheduling gains nothing on the A76). The
stages: front end 0.25, demosaic 0.58, colour matrix 1.08 (16-bit multiplies at half rate on
one pipe), tone table 1.51 ms (NEON clamps and moves the indices to general registers, 1.5x
the scalar loop). What changed for this path: the integer gain tables (lens shading x white
balance x digital gain, rebuilt whenever a gain changes, so on most frames) no longer round
with `f32::round` per entry but by truncation and a compare (the same result; x86 without
SSE4.1 called `roundf`), and fill each row's nodes run by run for one vector loop over the
row: 0.31 -> 0.21 ms; the staging copy is gone as on the A76. The quadratic tone curve's
NEON leaf (`tbl` from two registers, `sqrdmulh`) is slower than the table there (2.74 against
1.53 ms for three channels on the A76: its 16-bit multiplies run on one pipe at half rate),
so `Auto` keeps the table on AArch64.

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
* Reading the CMA capture buffers directly instead of staging them measured 1 ms slower
  then; on the current kernel it is 0.15 ms faster and is the default (see above).
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

## The live frame outside the image maths (CM5, RGB24, one thread)

Measured with `native-pipeline soft` and ftrace's function profiler on the process: the two
`DMA_BUF_IOCTL_SYNC` calls per frame cost 20 us (start: invalidate the 1.3 MB buffer, needed)
and 22 us (end: a clean of lines the CPU only read; dropped, see `NativeFrame`); `DQBUF` and
`QBUF` 2-3 us; the rest of the dequeue, wake-up and requeue about 25 us; the tool's own
per-frame output level 80 us. The staging copy was the large part (0.15 ms, now gone).
What is left in the ISP beyond its kernels (about 0.3 ms of 2.58) is memory traffic: the
1.3 MB input arrives from DRAM after the invalidate and the 3 MB of RGB24 output go back.
