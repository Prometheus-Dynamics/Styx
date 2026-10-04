# Portability: Styx without `std`

Styx's platform-neutral crates (the "brains": formats and frame metadata, 3A, the software
ISP, sensor descriptions and drivers, PiSP configuration, DNG) build as `no_std` + `alloc`, so
they run on microcontrollers, RTOSes, WebAssembly and SoCs without Linux. Each has a default
`std` feature; on Linux nothing changes (same code, same speed, same results: see
[Linux is unchanged](#linux-is-unchanged)).

Phase 2 adds the hardware side and the camera runtime
([portability-design.md](portability-design.md)). Steps 1-3 ([The hardware
layer](#the-hardware-layer-styx-hal)): `styx-hal` (camera hardware traits, no `std`, no
allocation, over embedded-hal), the sensor driver over any embedded-hal I²C or SPI bus,
blocking or async, sensor descriptions compiled at build time, and a sensor frame path that
allocates nothing. Steps 4-6 ([The camera runtime](#the-camera-runtime-styx-runtime)):
`styx-runtime`, the `no_std` runtime that ties a receiver, the sensor and the frames together
(Linux is its reference platform, at no cost), and the processing loop written once over ISP
traits in a `no_std` pipeline core. Still missing: stills and metrics counters in the runtime,
a bare-metal proof run of the whole loop (steps 7-8), and a port to real MCU hardware.

## What builds without `std`

`--no-default-features` (keep `neon` / `x86` on `styx-core-rs` and `styx-softisp` for SIMD):

| Crate | Without `std` | Needs `std` |
|---|---|---|
| `styx-runtime` | everything: the sensor side (`SensorState`, `Controls`, lens, health), `Camera<P>`, `FrameStream`, buffer leases, the sensor service; shared state is `Rc<RefCell>` | `Arc<Mutex>` shared state and `Send + Sync` handles (feature `std`, the default) |
| `styx-pipeline` | the core: `Controller` (the 3A loop runner), `IspSettings` for the PiSP and the software ISP, statistics, the back end config builder, the processing loop (`process`: `Algorithms`, `FrameIsp`, `InlineIsp`, `SensorControls`), `SoftLoop`, stills (`dng_metadata_at`) | algorithm replay recording, raw recordings, tunings and warm starts on disk, measurements, timings, the device paths (`device`), the GPU ISP |
| `styx-hal` | everything (no `alloc` either): power sequencing, DMA memory, the receiver, the lens actuator, `BoardPins`, `StaticDma` | `Send + Sync` on `SensorStart`, the `mock` platform |
| `styx-core-rs` | formats (`FourCc`, `MediaFormat`, layouts), plane layouts and their math (`PlaneLayout`, `plane_layout_from_dims`, `FrameAllocation`, `FrameValidationError`), plane views, frame metadata (`FrameMeta`, `NativeFrameMeta`, `FrameTiming`, `CaptureInstant`, ...), requirements (`FrameRect`, ...), controls (`ControlId`, `ControlValue`, descriptors), the SIMD row kernels | `FrameLease`, `BufferPool` and memfd / dma-buf backings, queues, transforms, `metrics`, clocks (`TimestampClock::now_ns` is `None` without std), `async` (tokio), `schema` (utoipa) |
| `styx-algo` | every algorithm (AGC with flicker avoidance and deflicker, AWB, ALSC, CCM, contrast, denoise, black level, lux, AF), `Pipeline`, the tuning model, `Tuning::from_toml_str` / `to_toml_string`, Raspberry Pi JSON in and out, the simulator, `WarmStart` | `Tuning::load` (paths), `replay` (JSON Lines over `std::io`) |
| `styx-softisp` | every kernel, `SoftIsp` on the calling thread (all outputs, statistics, both arithmetics) | `SoftIsp::with_threads` (the helper thread pool); run-time CPU feature detection |
| `styx-sensor` | descriptions from strings or compiled (`from_postcard`, feature `postcard`), timing, gain models, the control scheduler, embedded data, lenses (description and schedule; the VCM drivers are `lemnos-drivers-vcm`), PDAF decoding, kernel-sensor fallback descriptions, `SensorDriver` / `AsyncSensorDriver` over Lemnos's `I2cRegisters` / `SpiRegisters` (any embedded-hal bus) and `SensorPins` | `from_file`, `NoPins`, the `build` helper (a build-dependency) |
| `styx-pisp` | uAPI layouts, front end and back end config builders, back end tiling, statistics decoding | `device` (the kernel nodes) |
| `styx-dng` | the writer and the reader (in memory) | `DngError::Io` |

Extra dependencies without `std`: `libm` (float maths, pure Rust), `toml`'s own `no_std`
build for TOML, and Lemnos's `no_std` crates (`lemnos-hal`, `lemnos-drivers-vcm`: no `alloc`). `serde`, `serde_json` (std only, for `replay`), `thiserror` 2 and `smallvec`
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
vocabulary is embedded-hal 1.0 and embedded-hal-async 1.0, used directly, and Lemnos 2.0
(`lemnos-hal`: register maps over I²C/SPI, regulators, clock outputs, portable error kinds):
generic I²C, SPI, GPIO, delay, supply and clock work is Lemnos's (`lemnos-linux` on Linux, a
chip HAL on microcontrollers), not Styx's.

| Piece | What |
|---|---|
| `SensorPins` / `AsyncSensorPins` | a sensor's power sequence by role (GPIO lines, the input clock, supplies) with an embedded-hal `DelayNs` (the delay is required: no `std::thread::sleep`); `BoardPins` builds them from embedded-hal `OutputPin`s, a Lemnos `ClockOutput` (`FixedClock`, a PWM or PLL output) and a delay |
| `DmaMemory` / `DmaBuffer` | buffers devices fill: CPU view, cache maintenance (`begin_cpu`/`end_cpu`), device address, export handle; `StaticDma` carves 32-byte-aligned buffers from a linker-placed region |
| `Receiver` | the CSI-2 or parallel receiver: configure, queue, start/stop with a `SensorStart`, frame starts and filled buffers on separate wakers (`poll_sync`, `poll_done`) with non-blocking `try_` twins |
| `LensActuator` / `AsyncLensActuator`, `NoLens` | focus lenses that are not a plain register device |
| `ErrorKind`, `HalError` | what kind of failure an implementation's own error is (`Nack`, `NotFound`, `Disconnected`, ...), plus a platform code; Lemnos's `ErrorKind` converts into it (and is a `HalError`) |
| `Blocking<T>` | a blocking implementation used through the async traits (its futures are ready at once) |
| `mock` (feature) | an I²C sensor model (blocking and async, can really suspend and stop answering), recording pins and delay, a receiver with injected events and faults, heap DMA memory, a test `block_on` |

Sensor registers are Lemnos's register maps: `I2cRegisters<I>` over any embedded-hal `I2c`
(one transfer per register write, bursts of consecutive registers when the description allows
them, stack buffers), `SpiRegisters<S>` over any `SpiDevice` (`lemnos_hal::register`,
re-exported by `styx-sensor`). `styx-sensor` makes them the driver's `RegisterBus` (blocking)
and `AsyncRegisterBus` (async); `Registers<R>` does the same for any other Lemnos register map,
and `SensorDescription::i2c_registers` builds one from a description. The driver's trait adds
only what is camera-specific: a kernel driver's V4L2 controls (`set_controls`) and errors as
`BusError`. On Linux, `lemnos_linux::hal::I2cBus` is the embedded-hal `I2c` over i2c-dev (each
address claimed with `I2C_SLAVE`, never forced; no allocation per transfer) and `GpioLine` an
`OutputPin` over GPIO uAPI v2, so the native stack runs on the same code
(`styx_native::regbus::I2cRegisterBus` is `I2cRegisters<I2cBus>`).

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

## The camera runtime (`styx-runtime`)

`styx-runtime` is the logic that runs a camera, written once against `styx-hal` and the
sensor driver, `no_std` + `alloc` (feature `std` on Linux). It never spawns, sleeps or blocks:
the platform runs its parts on threads (Linux), tasks (Embassy) or one loop.

| Piece | What |
|---|---|
| `SensorState<B, P>` | the sensor side: bring-up sequencing (power, chip id, stream-off before init for a sensor a dead owner left streaming, init, mode; refused when the description would leave standby early), the receiver's start and stop (`serve_start` with the start-format check, `serve_stop`), frame starts driving the frame-exact schedule, typed requests with immediate writes inside the current frame (`request_at_now`, allocation-free `request_at_now_landings`), the values that produced each frame, embedded data reports, the lens and PDAF data, shut-down. Time from a platform `Clock` (`fn() -> Duration`; without one every request waits for a frame start) |
| `Controls<B, P>` | a cloneable handle on it: requests, ranges, the lens |
| `LensControl`, `PdafFrames` | the focus lens moved frame-exactly over any boxed `styx_hal::LensActuator` |
| `Health`, `Fault` | the fault that ends a stream (by `ErrorKind`) and its counters |
| `Platform`, `Camera<P>` | a receiver and a sensor side (`SensorSide`); start (configure, queue every buffer, start the receiver) and stop (retire the pool, stop, release, standby) in the order every path ends clean; `attach` gives a receiver whose own machinery serves the sensor (Linux) the stream's sensor side and health |
| `Pool`, `Lease`, `Frame` | the receiver's buffers lent out as frames, given back on drop while the stream runs; a frame keeps its buffer handle after a stop, so it outlives its stream |
| `FrameStream<P>` | filled buffers to frames through `Receiver::poll_done` (the caller's waker registered before looking: any executor, or a no-op waker in a superloop), frame starts inferred from frames when the receiver's events go missing, sequence gaps, the corrupt-frame limit, faults ending the stream after one error item |
| `serve_sync` | the sensor service: pending frame starts (`Receiver::try_sync`, or any `SyncSource`) to the control schedule, embedded data to the reports |

`styx-hal` changed for it: `FrameBuffer` is the read side of `DmaBuffer` (also implemented by
`&T`), and `Receiver::buffer` returns a handle (`Option<Self::Buffer>`) that keeps the buffer's
memory, so frames outlive a receiver's release.

### Linux as the reference platform

`styx-native` is the `Linux` platform: `SensorControl` is `SensorState` on `CLOCK_MONOTONIC`
(`sensor_control`), the bridge's requests are served through it (`BridgeServe`), and
`V4l2Receiver` implements `Receiver` over the capture node with everything `rp1-cfe` needs
inside it, unchanged: the event thread with its own `poll(2)` and the quiesce around
`STREAMON`/`STREAMOFF`, the bridge's start and stop served from that thread, the embedded data
node started first and stopped last, the failed start the bridge reports in its state, buffers
from MMAP + EXPBUF or a dma-heap. `NativeCamera`'s raw route is `Camera<Linux>`; `NativeFrame`
and `FrameStream` wrap the runtime's, with the same public API. The sensor side and the devices
stay behind `dyn` on Linux (so the host fault tests run the same receiver over fake devices);
measured, that costs nothing (below).

### The processing loop

`styx_pipeline::process` (in the `no_std` core) is the per-frame loop written once:
`Algorithms` decides which frames run the algorithms (every frame until settled, then about
15 Hz), gives each frame the newest settings retargeted to the exposure it got, runs the
algorithms on its statistics and hands their requests to `SensorControls`; `process` puts a
frame through a `FrameIsp` (the PiSP back end, the software ISP, the GPU ISP) with the
algorithms overlapped with the job when an `InlineIsp` (the PiSP front end) delivered the
statistics, or after it when the frame ISP made them. `PispPipeline` and `SoftLoop` (and with it
`SoftPipeline` and the replay tools) run on it. A software-loop replay of a CM5 recording gives
the same algorithm recording, image and per-frame values as before the move.

### No allocation per frame in the runtime

`crates/runtime/tests/no_alloc.rs`: 300 frames on the mock platform (frame starts through the
sensor service, frames through the stream with their values, the bytes read, a 3A-style
request per frame, the buffer given back) make zero allocations. On the CM5 the allocations per
frame of the whole process are the same as before the runtime (counted with an `LD_PRELOAD`
counter, below).

### Linux is unchanged by the runtime

CM5 (OV9782 1280x800, native mode, HeliOS tuning), `dev` fcc4343 (before) against
`native/runtime` (after), built alike and run alternately; syscalls and allocations per frame
from an `LD_PRELOAD` counter of libc calls (150- and 450-frame runs, the difference over 300
frames; `strace` is not on the device), CPU from the thread's scheduler time:

| | before | after |
|---|---|---|
| PiSP, 30 fps: `ioctl` / `poll` / `write` per frame | 28.0-28.1 / 6.0 / 0.01 | 28.0 / 6.0 / 0.01 |
| PiSP: `malloc` + `realloc` per frame (whole process, mostly the algorithms) | 45.5-45.6 | 45.5 |
| raw (Styx API, 30/60/120 fps segments): `ioctl` / `poll` / `read` / `write` per frame | 75.8-77.1 / 59.4-63.3 / 19.8-21.0 / 5.8-7.0 | 75.9-77.1 / 59.7-63.2 / 19.9-21.0 / 5.9-7.0 |
| software ISP, 30 fps: `ioctl` / `poll` / `malloc` per frame | 11.2-11.4 / 10.6-11.2 / 57.1-57.6 | 11.3-11.4 / 10.9-11.4 / 57.3 |
| PiSP CPU per frame (pipeline thread), 30 fps, median of 14 runs (spread) | 0.284 ms (0.267-0.306) | 0.290 ms (0.277-0.337) |
| same, 8 alternating runs | 0.286 ms | 0.286 ms |
| PiSP CPU per frame, 120 fps | 0.172-0.180 ms | 0.173-0.177 ms |
| PiSP latency (frame start → outputs), 30 / 120 fps, median | 9.80-9.86 / 9.87-9.89 ms | 9.80-9.88 / 9.87 ms |
| software ISP, 30 fps, 1 thread: CPU per frame (pipeline + event thread) | 2.92-2.98 ms | 2.95-2.97 ms |
| Styx API NV12 30 fps (`native_isp_bench single`): latency median, peak RSS | 9.42-9.45 ms, 19.3-19.6 MiB | 9.44-9.47 ms, 19.2-19.6 MiB |
| open → first frame: PiSP tool / Styx API raw | 29.8-30.3 / 36.0-38.3 ms | 29.6-30.1 / 35.9-36.2 ms |
| AE locked after (cold start, 30 fps) | 6 frames | 6 frames |
| raw landing (`camera_controls`): exposure/gain after the request | 2 frames | 2 frames |

The software loop replays a CM5 recording identically before and after (algorithm recording,
image and per-frame values), and allocates the same 50 times per frame on the host.

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
  `SensorPins` return a `BusError` (a `BusErrorKind`, which is Lemnos's `ErrorKind`, a
  message and a platform code): with `std`, `std::io::Error` converts to it and back without
  loss (errno, message and wrapped errors survive), so implementations over Linux devices use
  `?` as before. Lemnos register errors convert with `BusError::from_register` (Lemnos's
  classification; the errno when the bus error is an `io::Error`, as
  `styx_native::sensor_bus::i2c_error` makes it for i2c-dev).
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

## API changes with Lemnos 2.0

Styx's generic hardware code moved to Lemnos (`docs/portability-design.md`, "Moved to
Lemnos"); Styx depends on `lemnos-hal`, `lemnos-linux` and `lemnos-drivers-vcm`.

- `styx_sensor::{I2cRegisters, SpiRegisters}` are Lemnos's (`lemnos_hal::register`):
  `I2cRegisters::new(i2c, address, AddressWidth::Bits16)` (infallible; the width is an
  `AddressWidth`), `with_bursts(max_bytes)` (was `with_bursts(bool)`; Styx's sensors use
  `styx_sensor::MAX_BURST`, 32), `i2c()`/`i2c_mut()`/`release()` (were
  `inner()`/`inner_mut()`/`into_inner()`); `SpiRegisters::new(spi, width)` with
  `with_flags(read, write)` (was a `read_flag` argument). `read_bytewise` is a free function.
  `SensorDescription::i2c_registers(i2c)` builds the sensor's map from its description.
- `RegWrite` is `lemnos_hal::RegWrite` (same fields; `RegWrite::new(address, bytes, value)`,
  `byte`, `word`); it has no `Display`: `styx_sensor::show_write(&w)`. Compiled descriptions
  are unchanged on the wire.
- `BusErrorKind` is `lemnos_hal::ErrorKind`: `TimedOut` is `Timeout`, `Other` is `Failed`, and
  `Busy`, `PermissionDenied`, `Unavailable`, `Overrun` exist. `BusError::from_register`,
  `with_code` are new; `From<io::Error>` classifies by errno (`EREMOTEIO` is `Nack`).
- Lens: `VcmChip` / `VcmFormat` / `VcmI2c` stay as the description's types; the command
  formats and `encode` are `lemnos-drivers-vcm`'s (`VcmI2c::with_format`,
  `VcmFormat::with_lemnos`, `VcmI2c::max_position`); a custom format has at most
  `MAX_VCM_WRITES` power-up and power-down writes. `styx_native::lens::I2cVcm` drives
  `lemnos_drivers_vcm::Vcm` over `I2cBus`.
- `styx_hal`: `ClockEnable` is gone (`BoardPins` takes a `lemnos_hal::ClockOutput`; `NoClock`
  implements it); `StdDelay` is gone (`lemnos_linux::hal::StdDelay`); `ErrorKind` gains
  `PermissionDenied` and converts from and into `lemnos_hal::ErrorKind`; `styx_hal::lemnos_hal`
  is re-exported.
- `styx-kernel`: `bus::{i2c, gpio, eh}` and `uevent` are gone (`lemnos_linux::hal::{I2cBus,
  GpioChip, GpioLine}`, `lemnos_linux::uevent`); `bus` is the sensor bridge only.

## API changes with the runtime (phase 2, steps 4-6)

The `styx` API is unchanged; so are `NativeCamera`, `NativeFrame`, `FrameStream`,
`CameraControls`, `PispPipeline`, `SoftPipeline` and `SoftLoop` (new methods only).

- `styx_native::SensorControl<B, P>` is `styx_runtime::SensorState<B, P>`: build it with
  `styx_native::control::sensor_control(driver)`; its methods return `styx_runtime::Error`
  (`NativeError: From` it, losslessly); `serve`/`serve_detailed` come from the `BridgeServe`
  trait; `ExpectedStart` is `StartFormat` (same fields). `FrameControls`, `BringUpTimes`,
  `standby_problems`, `DEFAULT_WRITE_MARGIN` are the runtime's, re-exported.
- `styx_native::LensActuator` is `styx_hal::LensActuator` (`Error = io::Error` for the Linux
  actuators); `LensControl::new` takes any actuator (`boxed` a `Box<dyn LensDrive>`),
  `request_at` takes the time; `open_actuator` returns `Box<dyn LensDrive>`.
- `styx_hal`: `FrameBuffer` (split out of `DmaBuffer`, whose users import both),
  `Receiver::buffer(&self, index) -> Option<Self::Buffer>`.
- `styx_pipeline`: feature `std` (default; `device` and `gpu` imply it); `dng_metadata_at`
  (capture time as a duration) next to `dng_metadata`; new `process` module, `SoftLoop::{
  start_with, process_frame_with, algorithms}`; `ControlHandle::request_at_now_landings`.
