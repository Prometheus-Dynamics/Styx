# Portability: Styx without `std`

Styx's platform-neutral crates (the "brains": formats and frame metadata, 3A, the software
ISP, sensor descriptions and drivers, PiSP configuration, DNG) build as `no_std` + `alloc`, so
they run on microcontrollers, RTOSes, WebAssembly and SoCs without Linux. Each has a default
`std` feature; on Linux nothing changes (same code, same speed, same results: see
[Linux is unchanged](#linux-is-unchanged)).

What is still missing for a camera on such a target is the hardware side: a HAL (I²C, GPIO,
clocks, a CSI/DVP receiver, DMA buffers) and a `no_std` camera runtime that drives a sensor
and the ISP per frame. That is phase 2 (being designed separately); the crates here keep no
std coupling in what it will build on.

## What builds without `std`

`--no-default-features` (keep `neon` / `x86` on `styx-core-rs` and `styx-softisp` for SIMD):

| Crate | Without `std` | Needs `std` |
|---|---|---|
| `styx-core-rs` | formats (`FourCc`, `MediaFormat`, layouts), plane layouts and their math (`PlaneLayout`, `plane_layout_from_dims`, `FrameAllocation`, `FrameValidationError`), plane views, frame metadata (`FrameMeta`, `NativeFrameMeta`, `FrameTiming`, `CaptureInstant`, ...), requirements (`FrameRect`, ...), controls (`ControlId`, `ControlValue`, descriptors), the SIMD row kernels | `FrameLease`, `BufferPool` and memfd / dma-buf backings, queues, transforms, `metrics`, clocks (`TimestampClock::now_ns` is `None` without std), `async` (tokio), `schema` (utoipa) |
| `styx-algo` | every algorithm (AGC with flicker avoidance and deflicker, AWB, ALSC, CCM, contrast, denoise, black level, lux, AF), `Pipeline`, the tuning model, `Tuning::from_toml_str` / `to_toml_string`, Raspberry Pi JSON in and out, the simulator, `WarmStart` | `Tuning::load` (paths), `replay` (JSON Lines over `std::io`) |
| `styx-softisp` | every kernel, `SoftIsp` on the calling thread (all outputs, statistics, both arithmetics) | `SoftIsp::with_threads` (the helper thread pool); run-time CPU feature detection |
| `styx-sensor` | descriptions from strings, timing, gain models, the control scheduler, embedded data, lenses and VCM formats, PDAF decoding, kernel-sensor fallback descriptions, `SensorDriver` over a `RegisterBus` and `SensorPins` | `from_file`, `NoPins`, the sleeping default of `SensorPins::delay` |
| `styx-pisp` | uAPI layouts, front end and back end config builders, back end tiling, statistics decoding | `device` (the kernel nodes) |
| `styx-dng` | the writer and the reader (in memory) | `DngError::Io` |

Extra dependencies without `std`: `libm` (float maths, pure Rust), and `toml`'s own `no_std`
build for TOML. `serde`, `serde_json` (std only, for `replay`), `thiserror` 2 and `smallvec`
are used without their `std` features.

### Targets

CI (`scripts/check-nostd.sh`) builds and lints every crate above without `std` for
`thumbv8m.main-none-eabihf` (Cortex-M33/M55 with an FPU), `riscv32imac-unknown-none-elf` and
`wasm32-unknown-unknown`, and for the host. `SensorDriver` and `SoftIsp`'s fp16 tables hold
`Arc`s, which need pointer-sized atomics (`target_has_atomic = "ptr"`): ARMv6-M
(`thumbv6m`, Cortex-M0) and RISC-V without the `a` extension lack them.

Floats: the crates use `f64` (3A, sensor timing) and `f32` (ISP parameters). A target without
a double-precision FPU emulates `f64` in software; the software ISP's per-pixel work is
integer (or fp16 on AArch64), so it is unaffected.

## Building for a bare-metal target

```toml
[dependencies]
styx-algo = { version = "2", default-features = false }
styx-softisp = { version = "2", default-features = false, features = ["neon", "x86"] }
styx-sensor = { version = "2", default-features = false }
```

```sh
rustup target add thumbv8m.main-none-eabihf
cargo build --release --target thumbv8m.main-none-eabihf
```

The application provides what `no_std` + `alloc` always needs: a `#[global_allocator]` (e.g.
`embedded-alloc`, or the RTOS's heap) and a `#[panic_handler]`. Nothing else: no OS calls, no
threads, no clock.

[`examples/nostd-smoke`](../examples/nostd-smoke/src/lib.rs) is a `#![no_std]` library over
all of them: AE/AWB from a TOML tuning on the simulator, the software ISP over a synthetic
RAW10 frame with statistics, a sensor description's frame timing, a DNG written and read back
in memory, PiSP statistics. CI builds it for the targets above and runs it as a host test with
every dependency built without `std` (`cargo test -p styx-nostd-smoke`), so the `no_std` code
paths (libm, compile-time SIMD selection) are exercised, not only compiled.

## What changes without `std`

- **Floats.** `core` has no `sqrt`, `exp`, `powf`, `round`, ...; each crate has a small
  `math` module that gives `f32`/`f64` those methods through `libm` and is imported only
  without `std`. With `std` the inherent methods are called, as before. libm's `pow`, `exp`,
  `ln`, `sin`/`cos` can differ from the platform's libm in the last bit, so 3A results (and
  replays, which are bit-exact on one platform) can differ in the last bits between a `std`
  and a `no_std` build; the integer ISP path is bit-exact either way.
- **SIMD.** With `std`, x86 leaves (SSE2/SSSE3/AVX2) and the AArch64 fp16 leaves are chosen at
  run time. Without it they follow the target's compile-time features: build with
  `-C target-feature=+avx2` (or `-C target-cpu=...`) / `+fp16` to get them. NEON is part of
  the AArch64 baseline either way. The scalar kernels are the oracle for every leaf.
- **Time.** No `Instant` in these crates' `no_std` parts. `FrameMeta::capture_instant` is a
  `CaptureInstant` (monotonic nanoseconds; with `std` on Linux the `CLOCK_MONOTONIC`
  nanoseconds `Instant` uses, and `Instant` converts into it); without `std` the platform
  passes its own nanoseconds (`CaptureInstant::from_nanos`) and asks
  `FrameMeta::latency_at(now)`. The algorithms take time from the frame metadata they are
  given (exposure, frame duration, sequence), never from a clock.
- **Threads.** None: `SoftIsp` runs on the calling thread; the sensor driver and the control
  scheduler are plain state machines the caller steps per frame.
- **Errors.** Every error type implements `core::error::Error`. `RegisterBus` and
  `SensorPins` return a `BusError` (a `BusErrorKind` and a message): with `std`,
  `std::io::Error` converts to it and back without loss (errno, message and wrapped errors
  survive), so implementations over Linux devices use `?` as before.
- **Collections.** `alloc`'s `Vec`, `String`, `BTreeMap`, `VecDeque`; no `HashMap` was in these
  crates.

## Linux is unchanged

The default features include `std`, and every workspace crate that uses these crates keeps it
(the workspace's `styx-core` dependency has no default features so `styx-softisp` can drop
`std`; its users ask for `std`, `neon` and `x86`, the old defaults). With `std`, the code is
the code before: inherent float methods, run-time SIMD detection, the thread pool, the same
mutexes (`get_mut`, no locking on one thread).

Measured on the dev host (Ryzen 9 5900X, `cargo bench -p styx-softisp`, 52 benchmarks at
1280x800, best of two alternating runs each of the tree before and after the split):
geometric mean −0.2 %, every end-to-end pipeline within ±2 % (noise; the 4-thread runs 3-7 %
faster), e.g.:

| Benchmark | Before | After |
|---|---|---|
| `e2e/nv12_tuned_lsc_stats` | 1.71 ms | 1.74 ms |
| `e2e/rgb24_tuned` | 1.09 ms | 1.11 ms |
| `e2e/rgb24_plain` | 458 µs | 450 µs |
| `e2e/nv12_tuned_lsc_stats_4threads` | 539 µs | 500 µs |
| `stage/tone_lut_x3` | 806 µs | 801 µs |
| `stage/demosaic_mhc` | 401 µs | 405 µs |

The one consistent difference, `half_1280x800/front` (+7 %, the x86 software fp16 oracle,
which x86 never runs in a pipeline), compiles to identical instructions before and after
(`objdump`); only its address, and so its loop alignment, moved.

## API changes that came with the split

- `styx_dng::DngMetadata::capture_time` is `Option<Duration>` since the Unix epoch (was
  `Option<SystemTime>`): `SystemTime::now().duration_since(UNIX_EPOCH).ok()`.
- `styx_sensor::RegisterBus` / `SensorPins` return `BusResult<T>` (`Result<T, BusError>`)
  instead of `io::Result<T>`; `SensorError::{Bus, Pins, Controls}` carry a `BusError`.
- `styx_core::buffer::FrameMeta::capture_instant` is `Option<CaptureInstant>`;
  `with_capture_instant` takes `impl Into<CaptureInstant>` (an `Instant` works).
- Without `std` only (additive): `SensorPins::delay` has no default, `NoPins` and
  `SoftIsp::with_threads` do not exist.
