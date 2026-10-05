# Styx on microcontrollers: footprint

How much flash and RAM Styx takes in a firmware image, what fits on common parts, and which
features and profile to use. The numbers come from real linked images, not libraries:
[`examples/mcu-footprint`](../examples/mcu-footprint/src/lib.rs) builds four configurations
as `#![no_std] #![no_main]` binaries (`cortex-m-rt`, `embedded-alloc`'s linked-list heap, a
panic handler that stops the core) for `thumbv7em-none-eabihf` (Cortex-M4F/M7) and
`thumbv6m-none-eabi` (Cortex-M0+), over a small mock board (a DCMI-like receiver, the OV9782
on a register bus, a clock) that a real port replaces. KB here is 1000 bytes.

| Configuration | What it runs |
|---|---|
| A | `styx-core` alone: three DMA buffers lent in place to `FrameLease`s (`MemoryRegion`, released back to the receiver), a bounded queue keeping the newest frame, a consumer |
| B | A + the sensor: the OV9782 description compiled at build time, its driver over a register bus, the frame-exact control schedule served at frame starts, embedded data reported, `styx-runtime`'s `Camera` streaming raw frames to the consumer as `FrameLease`s; a manual exposure ramp the consumer checks lands frame-exactly |
| C | B + AE and AWB on statistics of the raw frames, no ISP (a mono or YUV sensor whose own ISP needs only exposure control and white balance gains) |
| D | C + the software ISP (Bayer to NV12) with the full 3A (black level, AWB, AE, ALSC, CCM, contrast) through the pipeline core's `SoftLoop`, a -1/0/+1 EV still bracket reprocessed at full quality (MHC demosaic), the metrics counters |

## Flash and static RAM

`scripts/mcu-size.sh` (profile `mcu`: `opt-level = "z"`, fat LTO, one codegen unit,
`panic = "abort"`). Flash is `.vector_table + .text + .rodata + .data`; static RAM is
`.data + .bss` (the heap arena sits in `.uninit` and is counted under [Heap](#heap)).

| Image | Before | After | Static RAM |
|---|---|---|---|
| A, Cortex-M7/M4F | 11.5 KB | 11.5 KB | 2.2 KB |
| B, Cortex-M7/M4F | 116.2 KB | 89.5 KB (-23%) | 2.2 KB |
| C, Cortex-M7/M4F | 191.4 KB | 127.6 KB (-33%) | 2.2 KB |
| D, Cortex-M7/M4F | 298.3 KB | 205.3 KB (-31%) | 2.2 KB |
| A, Cortex-M0+ | did not build (queues) | 11.4 KB | 36 B |
| B, Cortex-M0+ | did not build (`Arc`, TOML) | 89.4 KB | 40 B |
| C, Cortex-M0+ | did not build | 128.5 KB | 40 B |
| D, Cortex-M0+ | did not build | 227.1 KB | 48 B |

"Before" is `dev` at 0bee9e4 with the same configurations and the features it had (every
algorithm in, descriptions validated again on the device). The 2.2 KB of static RAM on the
Cortex-M4F/M7 is `portable-atomic`'s lock table for 64-bit atomics (the counters); a
single-core part builds with `RUSTFLAGS="--cfg portable_atomic_unsafe_assume_single_core"`
and has 36-40 bytes (and 0.3-0.9 KB less flash). The Cortex-M0+ goes through
`critical-section` anyway. With `opt-level = "s"` (profile `mcu-speed`) the images are larger:
A 12.3, B 103.7, C 151.1, D 244.4 KB (M7); D 262.7 KB (M0+). A Cortex-M7 with its
double-precision FPU (`-C target-cpu=cortex-m7`) is not smaller (D 209.0 KB): the soft-float
`f64` routines are a few KB, the rest is the same code.

What the optional parts cost (Cortex-M7, added to the C and D images above):

| Feature | C | D | What it is |
|---|---|---|---|
| `styx-algo/af` | +0.4 KB | +11.4 KB | autofocus (contrast and PDAF scans) |
| `styx-algo/awb-bayes` | +4.4 KB | +4.4 KB | Bayesian AWB along the CT curve (else grey world) |
| `styx-algo/flicker` | +19.9 KB | +22.0 KB | flicker fits, 50/60 Hz detection, deflicker (sin/cos, QR) |
| `styx-algo/denoise` | 0 | +2.5 KB | denoise / sharpening parameters (ISP blocks the software ISP has not) |
| `styx-algo/lux` | +0.5 KB | +1.2 KB | lux estimation |
| all five | +25.2 KB | +40.7 KB | (D 245.9 KB) |
| `styx-softisp/fp16` | | +11.1 KB | fp16 arithmetic (fast only on ARMv8.2+ cores) |
| `styx-softisp/poly-tone` | | +2.7 KB | tone curve as quadratics (fast with AVX2) |
| `toml` (`styx-algo`, `styx-sensor`) | +0.2 KB | +0.9 KB | the TOML formats; the parser is linked only when called |

`alsc` is in D above (C has no lens shading). With `std` every one of these is on, so Linux
builds are unchanged.

### Where the flash goes

By symbol (`llvm-nm`), after fat LTO: much is inlined into the configuration's own `run`
function, so the per-crate split is approximate.

| D, Cortex-M7 (200.7 KB of code and data) | |
|---|---|
| the configuration's loop with what it inlined (camera start, bring-up, the ISP and 3A drivers) | 35.8 KB |
| `styx-algo` (AE 4.9, AWB 2.3, ALSC 2.7, contrast 1.4, tuning checks, statistics, curves) | 35.5 KB |
| `styx-softisp` (row kernels, statistics, `Prepared::new` tables 10.9) | 20.8 KB |
| postcard / serde decoding of the compiled description | 16.3 KB |
| `core` (slices, `Duration`, iterators) and `alloc` (`Vec`, `BTreeMap`, `Arc`) | 26.9 KB |
| `styx-pipeline` (`SoftLoop::new` 10.5, controller, still runner, ISP settings) | 14.4 KB |
| `styx-sensor` (driver, schedule, gain codes, timing) | 12.4 KB |
| compiler-builtins (soft-float `f64`, `memcpy`/`memmove`, 64-bit division) | 10.1 KB |
| `styx-runtime`, libm, `core::fmt`, `styx-core`, allocator, start-up | 23.3 KB |

B (84.6 KB): the loop with the inlined sensor and camera code 20.5 KB, description decoding
16.4 KB, `styx-sensor` 13.5 KB, compiler-builtins 9.6 KB, `core`/`alloc` 11.3 KB,
`core::fmt` 4.9 KB (integer formatting for error messages), `styx-runtime` 3.9 KB.

No row kernel is instantiated per pixel format (the integer path is chosen at run time with
one copy of each kernel); the largest duplications are serde's `next_element` per field type
(4.1 KB over 22 copies) and `BTreeMap` clone/drop (2.7 KB) for tuning maps.

### What was cut

Step by step on the Cortex-M7 images (B / C / D):

| Cut | B | C | D |
|---|---|---|---|
| Before | 116.2 KB | 191.4 KB | 298.3 KB |
| Optional algorithms behind features (AF, flicker fits and deflicker, Bayesian AWB, denoise, lux): `Pipeline::from_tuning` pushed AF always and the others whenever tuned, so they were linked | 116.1 | 161.2 | 269.9 |
| `SensorDescription::from_compiled` (the build validated the description, the device validated it again: the validator, its messages, core's float formatting) and no float in the runtime's invalid-gain message | 88.9 | 134.1 | 242.6 |
| One sort (grey world and the curves sorted through three stable-sort instantiations; now one `sort_unstable` on (key, index) pairs, the same order), no `{:?}` on tuning names (Unicode escaping tables), deflicker inert without `flicker` (the pipeline's deflicker gains pulled sin/cos, `fmod` and the model) | 88.9 | 127.4 | 219.6 |
| `fp16` and `poly-tone` arithmetics behind features (`Arith::Half` has no value without them, so every arm on it drops out) | 89.3 | 127.7 | 206.5 |
| After (with the heap changes below; B and C also report embedded data through `black_box`, so its decoding is not folded away) | 89.5 | 127.6 | 205.3 |

On the M0+ the cuts are what made it build at all: the queues have a critical-section ring,
the sensor driver, runtime and fp16 tables use `styx-core`'s `Arc`, TOML is optional.

## Heap

`cargo test -p styx-mcu-footprint --release --test heap -- --nocapture` runs the same `no_std`
code under a counting allocator: high-water marks over 60 frames, frame buffers (the
receiver's three buffers and the ISP's output, allocated through `frame_memory`) apart from
everything else. It checks what each configuration did, too (every frame delivered, B's ramp
landing frame-exactly, AE locked in C and D, three bracket shots). The numbers below are from
the same test built for `wasm32-wasip1` (32-bit pointers and `usize`, as on a Cortex-M):

```sh
CARGO_TARGET_WASM32_WASIP1_RUNNER="node examples/mcu-footprint/wasi-run.mjs" \
    cargo test -p styx-mcu-footprint --release --target wasm32-wasip1 --test heap -- --nocapture
```

On the 64-bit host the "other" column is 3-50% larger (pointers, `usize`s and `Vec` headers
twice the size).

| Configuration | Frame buffers QQVGA (160x120) | QVGA (320x240) | Other heap QQVGA | QVGA |
|---|---|---|---|---|
| A | 115.2 KB | 460.8 KB | 1.2 KB | 1.2 KB |
| B | 115.2 KB | 460.8 KB | 12.6 KB | 12.6 KB |
| C | 115.2 KB | 460.8 KB | 25.3 KB | 25.3 KB |
| D (streaming) | 144.0 KB | 576.0 KB | 69.8 KB | 86.2 KB |
| D with the bracket | 144.0 KB | 576.0 KB | 273.1 KB | 727.0 KB |

The frame buffers are the measured set-up's: three RAW10 buffers in 16-bit samples, plus
NV12 for D. Before (64-bit host, as the first measurement ran): B 21.2, C 35.8 KB, D with the
bracket 392.3 / 849.0 KB; after on the same host: B 15.6, C 29.4, D 76.6 / 92.7, D with the
bracket 281.4 / 735.4 KB. What changed:

* `styx-sensor` feature `short-history`: the control schedule keeps 16 frames of history
  instead of 64 (the scheduler's rings are 3.8 instead of 9.4 KB).
* D's statistics: an 8x6 zone grid and a 64-bin histogram (Linux uses 16x12 and 256), ALSC on
  the same grid: the statistics, ALSC's matrices and the lens shading tables shrink to a
  quarter. Set with `SoftLoop::set_base_params` and the ALSC tuning's `grid`.
* `SoftLoop::set_copy_input(false)`: the software ISP's 16 KB staging copy of input rows is
  for uncached DMA memory on Linux; a Cortex-M7 invalidates the D-cache over a buffer when its
  lease begins and reads it in place.
* What is left in D: the sensor state and its decoded description (most of B's 12.6 KB),
  statistics and their
  conversion, ALSC's tables and adaptive solve, the tone curve tables (old and new while
  settings change), the lens shading gain rows, `Params` and ISP settings cloned per frame.
  The bracket holds each shot's raw frame (a copy) until the last shot lands, then
  reprocesses each at full quality: three raw frames, their settings and one still output on
  top of streaming.

Allocator overhead and fragmentation come on top: size the heap at about 1.5 times the
"other" column plus whatever frame buffers it holds. Not on the heap: the metrics `Counters`
(6.4 KB of counters and latency rings, on D's stack here; a firmware keeps them in a
`static`), and stack use in general, which is not measured.

## Frame buffers

The frame buffers dominate RAM from QVGA up. Per frame:

| Format | QQVGA 160x120 | QVGA 320x240 | VGA 640x480 |
|---|---|---|---|
| RAW8 / 8-bit luma | 19.2 KB | 76.8 KB | 307.2 KB |
| RAW10 in 16-bit samples | 38.4 KB | 153.6 KB | 614.4 KB |
| NV12 / I420 | 28.8 KB | 115.2 KB | 460.8 KB |
| RGB24 | 57.6 KB | 230.4 KB | 921.6 KB |
| D as measured (3 x RAW16 + NV12) | 144.0 KB | 576.0 KB | 2304.0 KB |
| D lean (2 x RAW8 + NV12) | 67.2 KB | 268.8 KB | 1075.2 KB |

A DCMI in 8-bit mode (or a sensor set to RAW8) halves the raw buffers; two receiver buffers
are the minimum for continuous capture while one frame is processed. The buffers can be
static (`MemoryRegion::from_static_mut` over a `static` in DMA-capable SRAM) instead of heap.

## What fits where

Flash, static RAM, heap ("other", 32-bit) and frame buffers together; a stack of 8-16 KB on
top. Performance (frames per second) is not measured yet: the images are linked and sized,
not yet run on a core.

| Part | Flash / RAM | Fits comfortably | Tight | Does not fit |
|---|---|---|---|---|
| STM32H743 / H753 (Cortex-M7 480 MHz, DP FPU) | 2 MB / 1 MB (512 KB AXI SRAM, 288 KB SRAM1-3, 64 KB SRAM4, 128 KB DTCM) | A-D at QQVGA and QVGA (D at QVGA: 205 KB of flash, 576 KB of frames over the banks, 86 KB of heap); the bracket at QQVGA | B and C at VGA (two RAW8 buffers: 614 KB) | D at VGA (1.1 MB of frames even lean), the bracket at QVGA (1.3 MB): external SDRAM |
| STM32H750 (same core, 128 KB of flash) | 128 KB / 1 MB | A, B | C (127.6 KB) | D, unless the application runs from QSPI flash |
| STM32F429 / F439 (Cortex-M4F 180 MHz) | 2 MB / 256 KB (192 KB SRAM, 64 KB CCM not reachable by DMA) | A-C at QQVGA, B and C at QVGA with two RAW8 buffers | D at QQVGA lean (67 KB of frames, about 105 KB of heap with allocator slack, the stack in CCM) | D at QVGA, any bracket |
| STM32F407 / F417 (Cortex-M4F 168 MHz) | 1 MB / 192 KB (128 KB SRAM, 64 KB CCM) | A-C at QQVGA | B and C at QVGA with one RAW8 buffer | D (its heap and frames do not fit 128 KB of SRAM next to each other) |
| RP2040 (2 x Cortex-M0+ 133 MHz, no FPU) | 2 MB QSPI / 264 KB | A-C at QQVGA and QVGA (RAW8 through PIO); D at QQVGA lean (227 KB of flash, 67 KB of frames, about 105 KB of heap) | D at QQVGA with three RAW16 buffers | D at QVGA, any bracket |
| ESP32-S3 (2 x Xtensa LX7 240 MHz, SP FPU) with 8 MB PSRAM | 8-16 MB / 512 KB + 8 MB | everything up to VGA, frame buffers in PSRAM | | not built here (Xtensa needs the esp-rs toolchain); the crates are target-neutral `no_std` |

Flash is not the limit (D is 205-227 KB, 10-23% of a 1-2 MB part; only a 128 KB part like
the H750 runs short): RAM is, through the frame buffers from QVGA up, D's 70-86 KB of heap on
an F4, and a bracket's held raw frames everywhere but an H7 at QQVGA or PSRAM.

## Recommended set-up

```toml
# Cargo.toml of the firmware (configuration D; C drops softisp and pipeline's ISP use)
[dependencies]
styx-core = { package = "styx-core-rs", version = "2", default-features = false }
styx-hal = "2"
styx-sensor = { version = "2", default-features = false, features = ["postcard", "short-history"] }
styx-runtime = { version = "2", default-features = false }
styx-algo = { version = "2", default-features = false, features = ["alsc"] }
styx-pipeline = { version = "2", default-features = false }
styx-softisp = { version = "2", default-features = false }

[build-dependencies]
styx-sensor = { version = "2", features = ["build"] }

[profile.release]
opt-level = "z"
lto = "fat"
codegen-units = 1
panic = "abort"
```

* A Cortex-M0+ (or RISC-V without `a`) adds `styx-core`'s `critical-section` feature and a
  critical-section implementation (`cortex-m`'s `critical-section-single-core`, or the HAL's
  multicore-safe one on an RP2040).
* A single-core Cortex-M4F/M7 builds with
  `RUSTFLAGS="--cfg portable_atomic_unsafe_assume_single_core"` (2.1 KB less static RAM).
  Not on a multicore part.
* Descriptions: `styx_sensor::build::compile` in `build.rs`, `SensorDescription::from_compiled`
  in the firmware (no TOML parser, no second validation).
* Tuning: built in code (`Tuning::default()` plus the sections wanted). The TOML and
  Raspberry Pi JSON parsers are linked only if called; a compiled (postcard) tuning is not
  possible yet (the tuning's serde skips absent sections, which postcard cannot read back).
* Algorithms: only what the camera uses; `flicker` (22 KB) only under mains lighting with
  exposures shorter than a flicker period, `af` only with a focus lens.
* The software ISP: `Arithmetic::Int`; statistics on an 8x6 grid with 64 bins (ALSC's grid the
  same); `set_copy_input(false)` for cached SRAM.
* Stills: single shots, or reprocess each bracket shot as it lands, unless the RAM holds the
  raw frames.
* `opt-level = "s"` costs 7-19% more flash on the M7 (12-16% on the M0+, A aside) for speed
  that is not measured yet; `opt-level = 3`
  on the hot crate only (`[profile.release.package.styx-softisp] opt-level = 3`) is the usual
  middle ground.

## Keeping it small

`scripts/check-nostd.sh` links and lints the four images for both targets and runs
`scripts/mcu-size.sh --check`, which fails when an image's flash grows past its limit in
[`examples/mcu-footprint/size-limits.txt`](../examples/mcu-footprint/size-limits.txt) (the
sizes above plus about 5%), and runs the heap test (its frame-buffer and behaviour checks).
A change that grows an image on purpose raises the limit in the same commit.

Not done (TODO.md): decoding the compiled description is 16 KB of serde code in every image
with a sensor (generated Rust constructors would trade it for data); 3A in `f64` is
soft-float on a Cortex-M4F or M0+ (small in flash, slow, not measured); panic messages and
locations stay unless built with nightly `build-std` and `panic_immediate_abort`; a run on
real hardware for cycle counts.
