# Portability: Styx without `std`

Styx's platform-neutral crates (the "brains": formats and frame metadata, 3A, the software
ISP, sensor descriptions and drivers, PiSP configuration, DNG) build as `no_std` + `alloc`, so
they run on microcontrollers, RTOSes, WebAssembly and SoCs without Linux. Each has a default
`std` feature; on Linux nothing changes (same code, same speed, same results: see
[Linux is unchanged](#linux-is-unchanged)).

Phase 2 adds the hardware side ([portability-design.md](portability-design.md)). Its first
three steps have landed ([The hardware layer](#the-hardware-layer-styx-hal)): `styx-hal`
(camera hardware traits, no `std`, no allocation, over embedded-hal), the sensor driver over
any embedded-hal I²C or SPI bus, blocking or async, sensor descriptions compiled at build
time, and a frame path that allocates nothing. Still missing: the `no_std` camera runtime
that ties a receiver, the sensor and the ISP together per frame (steps 4-8), and a port to
real MCU hardware.

## What builds without `std`

`--no-default-features` (keep `neon` / `x86` on `styx-core-rs` and `styx-softisp` for SIMD):

| Crate | Without `std` | Needs `std` |
|---|---|---|
| `styx-hal` | everything (no `alloc` either): power sequencing, DMA memory, the receiver, the lens actuator, `BoardPins`, `StaticDma` | `StdDelay`, `Send + Sync` on `SensorStart`, the `mock` platform |
| `styx-core-rs` | formats (`FourCc`, `MediaFormat`, layouts), plane layouts and their math (`PlaneLayout`, `plane_layout_from_dims`, `FrameAllocation`, `FrameValidationError`), plane views, frame metadata (`FrameMeta`, `NativeFrameMeta`, `FrameTiming`, `CaptureInstant`, ...), requirements (`FrameRect`, ...), controls (`ControlId`, `ControlValue`, descriptors), the SIMD row kernels | `FrameLease`, `BufferPool` and memfd / dma-buf backings, queues, transforms, `metrics`, clocks (`TimestampClock::now_ns` is `None` without std), `async` (tokio), `schema` (utoipa) |
| `styx-algo` | every algorithm (AGC with flicker avoidance and deflicker, AWB, ALSC, CCM, contrast, denoise, black level, lux, AF), `Pipeline`, the tuning model, `Tuning::from_toml_str` / `to_toml_string`, Raspberry Pi JSON in and out, the simulator, `WarmStart` | `Tuning::load` (paths), `replay` (JSON Lines over `std::io`) |
| `styx-softisp` | every kernel, `SoftIsp` on the calling thread (all outputs, statistics, both arithmetics) | `SoftIsp::with_threads` (the helper thread pool); run-time CPU feature detection |
| `styx-sensor` | descriptions from strings or compiled (`from_postcard`, feature `postcard`), timing, gain models, the control scheduler, embedded data, lenses and VCM formats, PDAF decoding, kernel-sensor fallback descriptions, `SensorDriver` / `AsyncSensorDriver` over `I2cRegisters` / `SpiRegisters` (any embedded-hal bus) and `SensorPins` | `from_file`, `NoPins`, the `build` helper (a build-dependency) |
| `styx-pisp` | uAPI layouts, front end and back end config builders, back end tiling, statistics decoding | `device` (the kernel nodes) |
| `styx-dng` | the writer and the reader (in memory) | `DngError::Io` |

Extra dependencies without `std`: `libm` (float maths, pure Rust), and `toml`'s own `no_std`
build for TOML. `serde`, `serde_json` (std only, for `replay`), `thiserror` 2 and `smallvec`
are used without their `std` features.

### Targets

CI (`scripts/check-nostd.sh`) builds and lints every crate above without `std` for
`thumbv7em-none-eabihf` (Cortex-M4F/M7, e.g. an STM32H7), `thumbv8m.main-none-eabihf`
(Cortex-M33/M55 with an FPU), `riscv32imac-unknown-none-elf` and `wasm32-unknown-unknown`, and
for the host (`styx-sensor` also with `postcard`). `SensorDriver` and `SoftIsp`'s fp16 tables hold
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

## The hardware layer (`styx-hal`)

`styx-hal` holds the camera-specific hardware traits, `no_std` and without `alloc`. The bus
vocabulary is embedded-hal 1.0 and embedded-hal-async 1.0, used directly: generic I²C, SPI,
GPIO, delay and clock work is the platform layer's (Lemnos, a chip HAL, `styx-kernel` for now),
not Styx's.

| Piece | What |
|---|---|
| `SensorPins` / `AsyncSensorPins` | a sensor's power sequence by role (GPIO lines, the input clock, supplies) with an embedded-hal `DelayNs` (the delay is required: no `std::thread::sleep`); `BoardPins` builds them from embedded-hal `OutputPin`s, a `ClockEnable` and a delay |
| `DmaMemory` / `DmaBuffer` | buffers devices fill: CPU view, cache maintenance (`begin_cpu`/`end_cpu`), device address, export handle; `StaticDma` carves 32-byte-aligned buffers from a linker-placed region |
| `Receiver` | the CSI-2 or parallel receiver: configure, queue, start/stop with a `SensorStart`, frame starts and filled buffers on separate wakers (`poll_sync`, `poll_done`) with non-blocking `try_` twins |
| `LensActuator` / `AsyncLensActuator`, `NoLens` | focus lenses that are not a plain register device |
| `ErrorKind`, `HalError` | what kind of failure an implementation's own error is (`Nack`, `NotFound`, `Disconnected`, ...), plus a platform code |
| `Blocking<T>` | a blocking implementation used through the async traits (its futures are ready at once) |
| `mock` (feature) | an I²C sensor model (blocking and async, can really suspend and stop answering), recording pins and delay, a receiver with injected events and faults, heap DMA memory, a test `block_on` |

Sensor registers are a thin layer over a bus: `styx_sensor::I2cRegisters<I>` over any
embedded-hal `I2c` (one transfer per register write, bursts of consecutive registers when the
description allows them, stack buffers), `SpiRegisters<S>` over any `SpiDevice`; both are
`RegisterBus` over a blocking bus and `AsyncRegisterBus` over an async one. On Linux
`styx-kernel`'s `I2cDevice` implements embedded-hal's `I2c` (and a requested GPIO line its
`OutputPin`), so the native stack runs on the same code (`styx_native::regbus::I2cRegisterBus`
is `I2cRegisters<I2cDevice>`).

### Async sensor driver

`AsyncSensorDriver<B: AsyncRegisterBus, P: AsyncSensorPins>` is the sensor driver with every
call that talks to the sensor `async`: bring-up sequences, mode and control writes, register
reads, frame starts. The driver's logic is written once, as async code; `SensorDriver` (the
blocking one, used on Linux) runs it over `Blocking` adapters, whose futures complete when
first polled, so each blocking call is one poll with no executor and the same register
traffic (a test compares both drivers' I²C transactions over a bus that suspends on every
transfer). The other direction, blocking code over an async-only bus, needs an executor;
Styx ships none: run `AsyncSensorDriver` in the platform's (Embassy, `pollster`, a task).

On Linux the sensor stays on the blocking path. i2c-dev has no asynchronous interface
(`I2C_RDWR` is a synchronous ioctl, the node is not pollable), so the epoll reactor cannot
wait on it; an async bus there would be the blocking one run inline (`Blocking(SensorBus)`,
which works with `AsyncSensorDriver` as is) or a worker thread, which adds two context switches
per transfer to writes that take 0.1-0.5 ms at 400 kHz and already run on the event thread at
the frame start. Neither helps, so `styx-native` keeps `SensorDriver`.

### Compiled descriptions

`toml` parses without `std`, but a firmware need not carry the parser: with feature `build`
(a build-dependency), `styx_sensor::build::compile(&["sensors/ov5640.toml"])` in `build.rs`
validates descriptions on the host (every problem reported with its path; a broken file fails
the build) and writes them as postcard bytes to `OUT_DIR`; the firmware reads them with
`SensorDescription::from_postcard(include_description!("ov5640"))` (feature `postcard`,
`no_std`). The OV9782 description is 1.3 KB compiled against 13 KB of TOML.
`examples/nostd-smoke` does this for the built-in OV9782.

### No allocation per frame

Once the sensor streams, its frame path allocates nothing: the control schedule keeps its
pending requests, issued values and reports in fixed rings sorted by frame (~9 KiB per
scheduler; a control with 16 requests waiting drops the oldest and counts it,
`dropped_requests`), requests return `Landings` and reports `Mismatches` (fixed lists of four),
register batches, V4L2 control batches, gain code searches and embedded data decoding use the
stack, and I²C transfers are encoded in place. `crates/sensor/tests/no_alloc.rs` proves it with
a counting allocator: 300 frames of frame starts, 3A requests, immediate requests, embedded
reports and applied values, over the blocking and async drivers and a kernel-driven sensor,
make zero allocations.

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
- Without `std` only (additive): `NoPins` and `SoftIsp::with_threads` do not exist.

## API changes with `styx-hal` (phase 2, steps 1-3)

- `SensorPins` is `styx_hal::SensorPins` (re-exported by `styx-sensor`): an associated
  `Error: HalError` instead of `BusResult`, and embedded-hal's `DelayNs` as a supertrait in
  place of `delay(Duration)` (required with and without `std`; `NoPins` sleeps).
- `BusErrorKind` gains `Nack`; `BusError` carries a platform code (`code()`) and converts from
  `HalError`s, embedded-hal I²C and SPI errors (`from_hal`, `from_i2c`, `from_spi`).
- `ControlScheduler::request`/`request_now` return `Landings`, `report` returns `Mismatches`;
  `SensorDriver::request`/`request_now`/`request_codes` and `report` likewise
  (`FixedVec`: a slice by `Deref`, compares with `Vec`s; `to_vec()`). `kernel_controls` returns
  `KernelControls`. `styx-native`'s `ControlHandle` keeps returning `Vec`s.
- `SensorDescription` has a `bus: Option<BusSection>` field (the `[bus]` section).
- `styx_native::regbus::I2cRegisterBus` is `styx_sensor::I2cRegisters` (constructor takes the
  7-bit address: `new(i2c, address, address_bits)`); `I2cIo` and `encode_write` are gone
  (`I2cDevice` is an embedded-hal `I2c`).
- New: `AsyncRegisterBus`, `AsyncSensorDriver`, `I2cRegisters`, `SpiRegisters`,
  `SensorDriver::read_register`, `SensorDescription::{from_postcard, to_postcard}`,
  `include_description!`, `styx_sensor::build`.
