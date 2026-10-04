# Portable camera runtime: `styx-hal` and a `no_std` runtime

Status: reviewed; steps 1-3 landed on branch `native/hal` (see [Status](#status) below),
steps 4-8 wait for the HeliOS image proof. Decisions from the review: async I²C now (not
later), `styx-native` keeps its name, ports live in this repository under `ports/`. Since the
review, **Lemnos** (Prometheus Dynamics' hardware discovery, driver and bus workspace) is the
lower layer for generic driver and communication work: Styx uses the embedded-hal 1.0 /
embedded-hal-async 1.0 traits as its bus vocabulary, depends on Lemnos 2.0 for everything
generic (register maps, i2c-dev, GPIO, uevents, VCM drivers, regulators and clocks; see
[Moved to Lemnos](#moved-to-lemnos)) and keeps only camera-specific traits in `styx-hal`
(sections 2.2, 2.3 and 2.7 below are superseded where they say otherwise).

## Status

| # | step | state |
|---|---|---|
| 1 | `styx-hal` | **landed** (`2fe7432`): `SensorPins`/`AsyncSensorPins` (power sequencing by role over embedded-hal `DelayNs`), `BoardPins` (embedded-hal `OutputPin`s + `ClockEnable` + delay), `DmaMemory`/`DmaBuffer`/`StaticDma`, `Receiver` and its types (2.5), `LensActuator`/`AsyncLensActuator`/`NoLens`, `ErrorKind`/`HalError`, `Blocking`, `Instant`; feature `mock` (I²C sensor model blocking and async, pins, delay, receiver, memory, `block_on`). No `alloc`. `styx-kernel`: `I2cDevice` is an embedded-hal `I2c`, `GpioPin` an `OutputPin`. CI: `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`, `riscv32imac-unknown-none-elf`, `wasm32-unknown-unknown` |
| 2 | `styx-sensor` on `styx-hal` | **landed** (`413408a`): `I2cRegisters`/`SpiRegisters` over embedded-hal buses (blocking `RegisterBus`, async `AsyncRegisterBus`, shared encoding), `AsyncSensorDriver` (driver logic written once as async code; the blocking `SensorDriver` runs it over `Blocking` adapters in one poll), `delay` required (`DelayNs`), the `[bus]` section, compiled descriptions (`build` helper in `build.rs`, postcard, `from_postcard`, `include_description!`); `styx-native`'s bus is `I2cRegisters<I2cDevice>`. `RegisterBus` stays in `styx-sensor` (a sensor register layer, not a bus abstraction); `KernelControl` keeps its name |
| 3 | allocation-free sensor frame path | **landed** (`06b2d47`): scheduler rings (`FrameMap`), `Landings`/`Mismatches`/`KernelControls`, stack batches, gain codes and embedded decoding without allocation; `tests/no_alloc.rs` (counting allocator, 300 frames, blocking + async + kernel-driven: zero) |
| Lemnos | generic hardware on Lemnos 2.0 | **landed** (branch `native/lemnos-switch`): register maps, `RegWrite` and error kinds from `lemnos-hal`; i2c-dev and uevents from `lemnos-linux`; VCM lenses from `lemnos-drivers-vcm`; Styx's copies deleted ([Moved to Lemnos](#moved-to-lemnos)) |
| 4 | `styx-runtime`, sensor side | **landed** (branch `native/runtime`): new crate `styx-runtime` (`no_std` + `alloc`, feature `std`): `SensorState` (was `styx_native::SensorControl`: bring-up sequencing, `serve_start`/`serve_stop` with the start-format check, frame starts, immediate writes, applied values, embedded reports, shut-down) on a platform `Clock` (`fn() -> Duration`), `Controls` (was `ControlHandle`), `LensControl` over a boxed `styx_hal::LensActuator`, `PdafFrames`, `Health`/`Fault` (kinds instead of errnos), `Error` (keeps `SensorError`/`BusError`, so `NativeError` loses nothing), `Shared`/`Counter`. `styx-native`: `SensorControl` = `SensorState` (built with `sensor_control`, on `CLOCK_MONOTONIC`), bridge requests through `BridgeServe`, `ControlHandle` wraps `Controls` with `NativeError`s; lens hardware (`KernelLens`, `I2cVcm`) implements `styx_hal::LensActuator` |
| 5 | `styx-runtime`, frames | **landed** (branch `native/runtime`): `Platform` (receiver + sensor side, `attach` hook for receiver-driven machinery), `Camera<P>` (start/stop order, undo on failure, `shut_down`), `Pool`/`Lease`/`Frame` (buffer leases given back on drop, outliving their stream), `FrameStream<P>` (waker-registered `poll_frame`, frame-start fallback, gaps, corrupt limit, faults), `SensorSide` + `serve_sync` (the sensor service over any `SyncSource`), `RunError`. `styx-hal`: `FrameBuffer` (the read side of `DmaBuffer`; `&T` implements it) and `Receiver::buffer` returns a handle (`Option<Self::Buffer>`) so frames outlive a restart. `styx-native`: `V4l2Receiver` implements `Receiver` (event thread, quiesce, bridge serving, embedded node, `StartFailed` all inside it), `Linux` is the platform, `NativeFrame`/`FrameStream` wrap the runtime's (public API unchanged). Mock-platform tests and a counting-allocator test (300 frames, zero allocations) in `crates/runtime/tests` |
| 6-8 | runtime | see below |

CM5 (OV9782, native mode, baseline a178a44 against step 3, alternating runs): open → first
frame median 34.25 ms (34.1-34.8, 12 runs) against 34.4 ms (34.0-35.1); `camera_controls` raw
exposure/gain land 2 frames after the request and 60 fps lands on the predicted frame (read
back from embedded data) in both; `native-pipeline regcheck` reads back the same 91 of 93
registers in both (`0x0101` and `0x1000` read 0 in the baseline too).

### Async I²C

The driver's register paths have an async variant (`AsyncSensorDriver` over
`AsyncRegisterBus`: `I2cRegisters` on an embedded-hal-async `I2c`, e.g. Embassy's). The logic
is shared, not copied: it is written once as async code, and the blocking driver runs it over
`styx_hal::Blocking` adapters whose futures are ready at once (one poll, no executor). Blocking
code over an async-only bus needs an executor, which Styx does not hide: firmware runs the
async driver in its own (Embassy task, `embassy_futures::block_on`). On Linux the sensor keeps
the blocking path: i2c-dev has no asynchronous interface (`I2C_RDWR` is a synchronous ioctl
and the node is not pollable, so the epoll reactor has nothing to wait on); async there would
be the blocking call inline (`Blocking(SensorBus)` works today) or a worker thread, adding two
context switches to 0.1-0.5 ms transfers that already run on the event thread at the frame
start.

### Moved to Lemnos

Generic pieces built in Styx that Lemnos 2.0 took over (Lemnos `docs/foundation.md`), now
deleted here. Styx depends on a pinned commit of Lemnos `dev` by git until 2.0 is on crates.io (a local
checkout can be patched in through an untracked `.cargo/config.toml`, see the root `Cargo.toml`).

| piece | was | now | state |
|---|---|---|---|
| register maps over I²C/SPI: address widths, big-endian values, bursts, one message per write (`I2cRegisters`, `SpiRegisters`, `RegWrite`, `MAX_BURST`, encoders) | `styx-sensor` `registers.rs`, `desc::RegWrite` | `lemnos_hal::register` (blocking + async), re-exported; Styx's `RegisterBus` is implemented for them (`registers.rs`, 145 lines, no encoding) | done |
| bus error kinds (`BusErrorKind`) | `styx-sensor` `bus_error.rs` | `lemnos_hal::ErrorKind` (re-exported as `BusErrorKind`); `BusError::from_register` keeps the errno | done |
| embedded-hal `I2c` over i2c-dev (`I2cDevice`, `IoError`), the `I2C_RDWR` message building | `styx-kernel` `bus::i2c`, `bus::eh`, `bus::ioctl` | `lemnos_linux::hal::I2cBus` (address claimed with `I2C_SLAVE`, never forced; messages on the stack, a Lemnos fix for Styx's no-allocation frame path) | done |
| embedded-hal `OutputPin` over GPIO character devices (`GpioChip`, `GpioLines`, `GpioPin`) | `styx-kernel` `bus::gpio`, `bus::eh` | `lemnos_linux::hal::{GpioChip, GpioLines, GpioLine}` (uAPI v2; the native camera has no GPIO roles, the bridge powers it) | done |
| VCM command formats and the I²C VCM driver (`VcmFormat::encode`, chip tables, `I2cVcm` transfers) | `styx-sensor` `lens.rs`, `styx-native` `lens.rs` | `lemnos-drivers-vcm` (DW9714, DW9807, DW9817, AK7375, custom formats); Styx keeps the description, the frame-exact `LensSchedule` and `LensMotion` | done |
| hotplug (`uevent`) | `styx-kernel` `uevent` | `lemnos_linux::uevent` (UVC hotplug, the uevent fuzz target) | done |
| delays (`StdDelay`) | `styx-hal` `time` | `lemnos_linux::hal::StdDelay` | done |
| the camera input clock enable (`ClockEnable`) | `styx-hal` `power` | `lemnos_hal::ClockOutput` (`FixedClock`; `BoardPins` takes one) | done |
| error kinds of buses, pins, regulators and clocks | `styx-hal` `ErrorKind::from_{i2c,spi,digital,io}` | classified by `lemnos_hal::ErrorKind`, converted into the camera `ErrorKind` | done |
| the monotonic clock (`styx_kernel::monotonic_now`, `Instant`) | `styx-hal` `time`, `styx-kernel` `clock` | stays: camera timing (Lemnos's clock trait is a clock signal, not time) | kept |
| sysfs discovery | `styx-native` discovery | stays: it finds sensor bridges and media graphs, not I²C devices | kept |
| the I²C mock bus model (`mock::MockI2c`) | `styx-hal` `mock` | stays for now: Styx's tests need its shared clones, async suspension (`with_pending_polls`) and a target that stops answering (`set_dead`), which `lemnos_hal::mock::MockI2c` does not have | kept |

What stays in Styx: the camera-specific traits (`Receiver`, DMA memory, ISP traits, lens
actuators that are not plain I²C devices, power sequencing by role), the driver's
`RegisterBus` (Lemnos register maps plus a kernel driver's V4L2 controls), descriptions, the
driver and the runtime.

## The decision in one page

Styx's camera runtime is mostly platform-neutral logic that happens to be written against
Linux types. The plan is to separate the two with a small hardware abstraction layer and move
the logic into a runtime that builds without `std`:

```
 apps, HeliOS, PipeWire/GStreamer          (Linux)          your firmware            (MCU)
 styx  facade, planner, service, IPC       std, Linux       -
 ─────────────────────────────────────────────────────────────────────────────────────────
 styx-native   Linux platform: V4L2/media/bridge receiver,  styx-port-<chip>: DCMI/LCD_CAM
               i2c-dev bus, dma-heaps, discovery, provider  receiver, embedded-hal I²C/GPIO
 styx-pipeline[device]  PiSP, software, GPU ISP backends    (software ISP, or none)
 ─────────────────────────────────────────────────────────────────────────────────────────
 styx-runtime  no_std + alloc: sensor bring-up sequencing, frame-exact control schedule,
               buffer pool and leases, frame stream, sensor service, 3A loop orchestration,
               stills and brackets, metrics counters; ISP traits
 styx-hal      no_std, no alloc: Clock, Delay, RegisterBus, SensorPins, DmaMemory/DmaBuffer,
               Receiver, LensActuator; error kinds; embedded-hal 1.0 adapters (feature)
 brains        styx-sensor, styx-algo, styx-pipeline core, styx-softisp, styx-pisp (data),
               styx-dng, styx-core formats                        no_std + alloc (native/nostd)
```

Recommended design, decided:

1. **Two new crates.** `styx-hal` holds hardware traits only (no allocation, depends on
   `core`). `styx-runtime` holds the camera runtime (`no_std` + `alloc`, `std` feature).
   ISP traits live in `styx-runtime`, not in the HAL, because they speak the algorithms'
   types (`IspSettings`, `Statistics`).
2. **Generics on the frame path, one `Platform` trait bundling the associated types.**
   `Camera<P: Platform>` is monomorphised once per binary; on Linux `Camera<Linux>` is the
   only instantiation. The two `dyn` objects on today's frame path
   (`Arc<dyn CaptureDevice>`, `Arc<dyn SensorSide>`) become static calls. `dyn` stays where it
   costs nothing: provider registries, backend dispatch, the sensor start callback (twice per
   session).
3. **Runtime-agnostic async through `core::task`.** The receiver has two poll methods with
   separate wakers, `poll_sync` (frame starts, embedded data) and `poll_done` (filled
   buffers), plus non-blocking `try_` twins. Linux keeps its event thread and `poll(2)` exactly
   as today; Embassy wakes from interrupt handlers; superloops call the `try_` methods.
4. **The runtime never spawns, sleeps or blocks on its own.** It exposes three parts (sensor
   service, frame stream, control handle) that the platform runs on threads (Linux), Embassy
   tasks, or one loop.
5. **Linux is the reference platform at zero cost.** `styx-native` keeps its name and public
   API (`NativeCamera` becomes `styx_runtime::Camera<Linux>` behind the same methods), keeps
   the `rp1-cfe` workarounds inside its receiver, and is verified by the same CM5 gates the
   native stack already uses (same syscalls per frame, CPU and latency within noise).
6. **The minimal port is one trait.** I²C, GPIO and delays come from any `embedded-hal` 1.0
   HAL through adapters; sensors are description files; a new target writes a `Receiver`
   (500 to 800 lines for a DCMI-class peripheral) and a `DmaMemory` (about 100).

Core restructuring is eight steps, about 7-9 weeks of focused work, each step landing
separately with no regression on the CM5 (section 7). Two ports follow as proofs: an
STM32H7 + OV5640 (DCMI) and a Rockchip/i.MX8MP `rkisp1` Linux board.

---

## 1. Inventory: where the runtime touches the OS today

Legend: **N** platform-neutral logic (written against Linux types only by accident: `Instant`,
`io::Error`, `std::thread::sleep`, `Arc<Mutex>`), **L** genuinely Linux (ioctls, file
descriptors, sysfs, `/proc`, threads as a deployment choice), **N/L** a module that mixes both.
Line counts are today's (`wc -l`, tests excluded where noted).

### `styx-sensor` (6.5k lines): N, already almost portable

| module | class | OS contact |
|---|---|---|
| `desc/*`, `timing`, `gain`, `mbus`, `embedded`, `fallback`, `kernel_data`, `lens` (models, VCM formats, `LensSchedule`, PDAF decode) | N | TOML parsing with `toml` (needs `std` today; see 5.4), `std::fs::read_to_string` in the description loader |
| `bus.rs` (`RegisterBus`, `SensorPins`, `MockBus`) | N | `std::io::Result` in every signature; `SensorPins::delay` defaults to `std::thread::sleep` |
| `driver.rs` (`SensorDriver<B, P>`: power, chip id, init, mode, flips, test pattern, start/stop, requests) | N | generic over bus and pins already; `Vec` per write batch |
| `schedule.rs` (`ControlScheduler`) | N | `BTreeMap` per control and per frame: allocates on the frame path |

### `styx-native` (9.8k lines incl. 1.9k of tests and fakes): N/L

| module | lines | class | what is neutral / what is Linux |
|---|---|---|---|
| `control.rs` | 713 | N | `SensorControl` (bring-up sequencing, stream-off before init, frame starts, `request_at_now` with the write margin, embedded reports, shut-down), `ControlHandle`. Linux leaks: `StreamRequest`/`ExpectedStart` (the bridge protocol), `Instant` for bring-up times, `std::sync::Mutex` |
| `lens.rs` | 535 | N/L | `LensControl`, `PdafFrames`, `LensActuator` trait: N. `KernelLens` (subdev `FOCUS_ABSOLUTE`), `I2cVcm` (i2c-dev), `find_kernel_lens` (media topology): L |
| `stream.rs` | 392 | N/L | `StreamShared` logic: frame-sync fallback after 3 missing events, sequence gap counting, corrupt-frame limit, "register before looking" waker protocol: N. `CaptureDevice` dequeue, `BufferFlags`, errno: L |
| `buffers.rs` | 360 | N/L | lease bookkeeping (outstanding count, `live` flag, frames outliving their stream, give-back on drop): N. MMAP/`EXPBUF`/dma-heap import, `DMA_BUF_IOCTL_SYNC`: L |
| `health.rs` | 113 | N | fault recording; errno → `Disconnected` classification is L |
| `modes.rs`, `formats.rs` | 346 | N | mode listing, `select_mode`, bus code ↔ pixel format tables |
| `session.rs` | 420 | N/L | start/stop ordering (event thread before `STREAMON`, quiesce around it, undo on failure): sequencing N, devices L |
| `camera.rs` | 714 | N/L | `configure` sequence (power → chip id → init → mode → timing → receiver → initial frame duration): N. Media links, subdev pad formats, `VIDIOC_S_FMT`, bridge timing controls: L |
| `device.rs` | 133 | L (seam) | `BridgeDevice`, `CaptureDevice`: already the trait seam the HAL `Receiver` generalises |
| `events.rs` | 405 | L | the event thread: own `poll(2)`, wake pipe, quiesce gate (the `rp1-cfe` lock), bridge request serving |
| `external.rs`, `embedded.rs` | 419 | L | the PiSP front end owning the capture nodes; the embedded-data node |
| `discover.rs`, `kernel.rs`, `topology.rs`, `graph.rs`, `provider.rs`, `library.rs` | 2.2k | L | media topology, subdevs, search paths on disk, the `styx-graph` provider, hotplug |
| `regbus.rs`, `sensor_bus.rs` | 720 | N/L | I²C message encoding and bursts (`encode_write`): N. `I2cDevice`, `SubdevBus` (V4L2 controls), `BridgePins`: L |

### `styx-pipeline` (5.9k lines): mostly N

| module | class | OS contact |
|---|---|---|
| `controller.rs` (3A loop runner), `isp.rs` (`IspSettings` → PiSP FE/BE, software ISP), `stats.rs`, `sensor.rs`, `pisp_be.rs` (`BeConfigBuilder`), `soft.rs` (`SoftLoop`), `engine.rs`, `still.rs` | N | `Instant` for timings, `SystemTime` for DNG dates, `std::io::Write` for replay recording |
| `tuning.rs`, `warm.rs`, `rawrec.rs`, `replay.rs`, `measure.rs` | N/L | search paths and state files on disk; recordings as files |
| `device/*` (`PispPipeline`, `SoftPipeline`, `StillBackEnd`, `/proc` thread usage) | L | the loops on a `NativeCamera`, PiSP nodes, dma-bufs |

### Other crates

| crate | class | notes |
|---|---|---|
| `styx-kernel` (10.2k) | L | every ioctl; stays as is |
| `styx-graph` (4.1k) | N/L | graph model, paths, caps: N (could be `no_std` + `alloc`, not needed); `rt` (epoll reactor, timerfd, eventfd): L; `Provider`: std |
| `styx-pisp` (7.4k) | N/L | uAPI layouts, FE/BE config, tiling, stats: N (nostd agent); `device`: L |
| `styx-softisp` (8.3k) | N | `pool.rs` helper threads: std; NEON/x86 SIMD; scalar path everywhere else |
| `styx-gpuisp` (2.4k) | L/desktop | Vulkan through `ash`; std only, stays so |
| `styx-uvc` (4.1k) | N/L | descriptors, payload assembly, PTS/SCR clock model: N; usbfs, sysfs, uevent hotplug: L |
| `styx-core` (11.2k) | N/L | formats/types: N (nostd agent); `FrameLease`, pool, queues (`parking_lot`, `crossbeam`, optional `tokio`), `shared_fd`, `dmabuf_sync`: std/L |
| `styx` (50k) | std/L | `capture_api` (worker threads, `mpsc`, supervisors), planner, metrics (`/proc` CPU), service, IPC, recording; stays std |

**Summary.** Of the code that runs a camera (sensor, native, pipeline: ~22k lines), about
13-14k lines are neutral logic and ~8k are genuinely Linux. The neutral part is what a port
would otherwise have to rewrite, and it is the part that took the measurements (frame-exact
landing, 29 ms to first frame, AE at frame 6) to get right.

---

## 2. `styx-hal`: the traits

`no_std`, no `alloc`, depends on `core` only. Optional feature `embedded-hal` adds adapters
(2.7). Every trait has an associated error implementing `HalError`, so implementations keep
their own error types (errno on Linux, a vendor status on MCUs) and the runtime still knows
what kind of failure it was.

### 2.1 Time, errors

> **As built:** `Instant`, `ErrorKind` and `HalError` as below (`HalError` also requires
> `Display`); no `Clock` or `Delay` trait: delays are embedded-hal's `DelayNs`, and clocks
> belong to the platform layer (Lemnos).

```rust
#![no_std]
pub use core::time::Duration;

/// A point on the platform's monotonic clock in nanoseconds: `CLOCK_MONOTONIC` on Linux (the
/// clock of V4L2 buffer and event timestamps, as `styx_kernel::monotonic_now` today), the
/// tick counter scaled to ns on MCUs. Receiver timestamps use the same clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(pub u64);

impl Instant {
    pub fn saturating_duration_since(self, earlier: Instant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
    pub fn checked_add(self, d: Duration) -> Option<Instant> { /* ... */ }
}

pub trait Clock {
    fn now(&self) -> Instant;
    /// Granularity of `now`. Immediate control writes ("write in the current frame while 4 ms
    /// of it are left") are disabled when this is coarser than 100 µs.
    fn resolution(&self) -> Duration { Duration::from_nanos(1) }
}

/// Blocking waits for bring-up (power sequencing, settle times). Never used on the frame path.
pub trait Delay {
    fn delay(&mut self, d: Duration);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    Disconnected,  // device gone (ENODEV/ENXIO, a USB unplug, a bus that stopped answering)
    Busy,          // owned by someone else
    Timeout,
    Nack,          // I²C address or data not acknowledged
    NotFound,      // a role/resource the board does not have (optional steps are skipped)
    Unsupported,
    InvalidConfig,
    NoMemory,
    Overrun,       // the receiver had no buffer / FIFO overflow (frame lost, stream continues)
    Corrupt,       // sync or CRC error on a frame (frame flagged, stream continues)
    Io,
    Other,
}

pub trait HalError: core::fmt::Debug + From<ErrorKind> {
    fn kind(&self) -> ErrorKind;
    /// A platform code for logs: errno on Linux, a vendor status on MCUs.
    fn code(&self) -> Option<i32> { None }
}
```

### 2.2 Register bus (I²C, SPI): the existing trait, `io` removed

> **As built:** `RegisterBus` stayed in `styx-sensor` with `BusError`, as a thin sensor
> register layer over embedded-hal (`I2cRegisters<I: I2c>`, `SpiRegisters<S: SpiDevice>`,
> blocking and async). `KernelControl` kept its name. There is no Styx bus trait in
> `styx-hal`.

`RegisterBus` moves from `styx-sensor` to `styx-hal` unchanged except for the error type and
the name of the control-level method; `styx-sensor` re-exports it so drivers do not change.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegWrite { pub address: u16, pub bytes: u8, pub value: u32 }   // moved from styx-sensor

/// Sensor controls for sensors whose driver is not register-level: a Linux kernel driver
/// (V4L2 controls) today; a module with its own firmware tomorrow. Was `KernelControl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensorControlId { Exposure, AnalogueGain, DigitalGain, Vblank, Hblank, HFlip, VFlip, TestPattern }

pub trait RegisterBus {
    type Error: HalError;
    /// Read `bytes` (1..=4) starting at `address`, most significant byte first.
    fn read(&mut self, address: u16, bytes: u8) -> Result<u32, Self::Error>;
    fn write(&mut self, address: u16, bytes: u8, value: u32) -> Result<(), Self::Error>;
    /// Several registers in order; implementations may burst consecutive ones (one
    /// auto-incrementing transfer, as `I2cRegisterBus::with_bursts` does on the CM5).
    fn write_sequence(&mut self, writes: &[RegWrite]) -> Result<(), Self::Error> {
        writes.iter().try_for_each(|w| self.write(w.address, w.bytes, w.value))
    }
    /// Control-level access (see `SensorControlId`); register buses do not have it.
    fn set_controls(&mut self, _: &[(SensorControlId, i64)]) -> Result<(), Self::Error> {
        Err(ErrorKind::Unsupported.into())
    }
}
```

The address width (8 or 16 bit) and the 7-bit device address stay in the implementation, as
in `I2cRegisterBus::new(io, address_bits)` today. SPI sensors implement the same trait.

### 2.3 Pins and power

> **As built:** `SensorPins` has an associated `Error: HalError` and embedded-hal's `DelayNs` as
> a supertrait (the delay); `AsyncSensorPins` is its async twin. There is no `Clock` or `Delay`
> trait in `styx-hal` (embedded-hal `DelayNs`; the clock is the platform's).

```rust
/// GPIO lines, clocks and supplies of one sensor, by role name from the description's power
/// sequence ("reset", "powerdown", "xclk", "avdd", "dovdd", ...). Roles the board does not
/// have return `ErrorKind::NotFound` (optional steps are skipped, required ones fail).
pub trait SensorPins {
    type Error: HalError;
    fn set_gpio(&mut self, role: &str, value: bool) -> Result<(), Self::Error>; // logical level
    fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> Result<(), Self::Error>;
    fn set_supply(&mut self, role: &str, on: bool) -> Result<(), Self::Error>;
    fn delay(&mut self, d: Duration);   // now required: there is no std::thread::sleep
}
```

This is today's trait with `io` replaced. Linux keeps `BridgePins` (the bridge's power switch)
and `NoPins` (kernel-driven sensors). MCU boards use `BoardPins` from the adapters (2.7).

### 2.4 DMA memory

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access { Read, Write, ReadWrite }

/// Where a buffer must come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Any,          // whatever the device can reach
    Contiguous,   // physically contiguous (CMA on Linux; any DMA-capable RAM on MCUs)
    Fast,         // on-chip SRAM: statistics, small frames
    Large,        // external SDRAM/PSRAM: full frames on MCUs
}

/// One buffer a device writes or reads.
pub trait DmaBuffer {
    /// A handle another process or device can import (Linux: the dma-buf descriptor).
    type Export<'a>: Copy where Self: 'a;
    fn len(&self) -> usize;
    /// The CPU view. Contents are defined only between `begin_cpu` and `end_cpu` (or always,
    /// on coherent memory).
    fn bytes(&self) -> &[u8];
    fn bytes_mut(&mut self) -> &mut [u8];
    /// Cache maintenance before the CPU touches the buffer: `DMA_BUF_IOCTL_SYNC` start on Linux
    /// (cached dma-heap buffers), D-cache invalidate by address on a Cortex-M7, nothing on
    /// coherent memory.
    fn begin_cpu(&self, access: Access);
    /// After the CPU is done: clean what it wrote. Skipped for reads where the platform allows
    /// (Linux: no `SYNC_END` after reads, 22 µs per 1.3 MB frame saved on the CM5).
    fn end_cpu(&self, access: Access);
    /// The address a DMA engine is programmed with (MCUs; `None` behind an IOMMU or a driver).
    fn device_address(&self) -> Option<u64> { None }
    fn export(&self) -> Option<Self::Export<'_>> { None }
}

pub trait DmaMemory {
    type Buffer: DmaBuffer;
    type Error: HalError;
    fn allocate(&mut self, len: usize, align: usize, region: Region)
        -> Result<Self::Buffer, Self::Error>;
}
```

Linux implementations: `HeapBuffer` (dma-heap `linux,cma` / `system`, mapped, `Export = BorrowedFd`)
and the receiver's own MMAP buffers (mapped and `EXPBUF`ed). MCU implementations:
`StaticDma` carving 32-byte-aligned buffers out of a `&'static mut [u8]` placed in AXI SRAM,
SDRAM or PSRAM by the linker script (`Export = ()`, `device_address = Some(ptr as u64)`).

### 2.5 Receiver

The receiver is whatever turns the sensor's bus into buffers in memory: a CSI-2 receiver and
its DMA (rp1-cfe, the rkisp1 input, an ESP32-P4 CSI), a parallel port (STM32 DCMI, ESP32
LCD_CAM), an ISP front end that owns the capture nodes (PiSP FE). Its methods take `&self`
and it is `Sync` because interrupt handlers and the Linux event thread share it with the
frame path, exactly like `CaptureDevice` today (V4L2 objects are naturally `&self`).

```rust
/// How the sensor's bus is wired. Linux takes it from the device tree; MCUs from the sensor
/// description's new `[bus]` section (6.1).
#[derive(Clone, Copy, Debug)]
pub enum Bus {
    Csi2 { lanes: u8, link_frequency: u64, continuous_clock: bool, virtual_channel: u8 },
    Parallel { width: u8, pclk_rising: bool, hsync_active_high: bool, vsync_active_high: bool,
               embedded_sync: bool },
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct EmbeddedConfig { pub data_type: u8, pub lines: u32, pub line_bytes: u32 }

#[derive(Clone, Debug)]
pub struct ReceiverConfig {
    pub bus: Bus,
    pub bus_code: u32,           // MEDIA_BUS_FMT_* numbering, the sensor description's codes
    pub fourcc: [u8; 4],         // memory format (packed raw, YUYV, JPEG, ...)
    pub width: u32,
    pub height: u32,
    pub stride: Option<u32>,
    pub buffers: u32,
    pub memory: BufferSource,    // receiver-owned or allocated from the platform's DmaMemory
    pub embedded: Option<EmbeddedConfig>,
    pub frame_starts: bool,      // deliver FrameStart events
}

#[derive(Clone, Copy, Debug)]
pub enum BufferSource { Own, Allocate(Region) }

#[derive(Clone, Copy, Debug)]
pub enum StartOrder {
    /// Arm the receiver, then start the sensor (CSI-2 receivers that must see LP-11 first;
    /// DCMI waits for VSYNC either way).
    ReceiverFirst,
    SensorFirst,
    /// The receiver's own start asks for the sensor start and waits for it (the Styx sensor
    /// bridge: `STREAMON` → bridge request → acknowledgement).
    ReceiverDriven,
}

#[derive(Clone, Copy, Debug)]
pub struct ReceiverCaps {
    pub frame_start_events: bool,    // without them frame starts are inferred from dequeues
    pub embedded_data: bool,
    pub timestamp: TimestampPoint,   // FrameStart | FrameEnd | Dequeue
    pub start_order: StartOrder,
    pub max_buffers: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum TimestampPoint { FrameStart, FrameEnd, Dequeue }

#[derive(Clone, Copy, Debug)]
pub struct Configured { pub stride: u32, pub buffer_len: usize, pub buffers: u32 }

/// Frame starts and embedded data: what the sensor service needs, on its own waker.
#[derive(Clone, Copy, Debug)]
pub enum SyncEvent {
    FrameStart { sequence: u64, at: Instant },
    Embedded { sequence: u64, slot: u32, bytes: usize },
    /// A non-fatal problem (an overrun, a sync error): counted, the stream goes on.
    Glitch(ErrorKind),
}

/// A filled buffer: what the frame stream needs, on its own waker.
#[derive(Clone, Copy, Debug)]
pub struct FrameDone {
    pub index: u32,
    pub sequence: u64,
    pub timestamp: Instant,
    pub bytes_used: usize,       // variable for JPEG
    pub corrupt: bool,
    pub stats_slot: Option<u32>, // statistics captured with the frame by an inline ISP
}

/// The runtime's sensor side, as a receiver with `StartOrder::ReceiverDriven` calls it.
/// `MaybeSendSync` is `Send + Sync` with styx-hal's `std` feature (the Linux event thread
/// serves the bridge with it) and empty without (an MCU camera lives in one task, its handle
/// is an `Rc`).
pub trait SensorStart: MaybeSendSync + 'static {
    fn start(&self) -> Result<(), ErrorKind>;   // write stream-on, start the control schedule
    fn stop(&self) -> Result<(), ErrorKind>;
}

pub trait Receiver: Sync {
    type Error: HalError;
    type Buffer: DmaBuffer;

    fn caps(&self) -> ReceiverCaps;
    fn configure(&self, cfg: &ReceiverConfig) -> Result<Configured, Self::Error>;
    fn buffer(&self, index: u32) -> &Self::Buffer;
    fn embedded(&self, slot: u32) -> Option<&[u8]> { let _ = slot; None }
    fn statistics(&self, slot: u32) -> Option<&[u8]> { let _ = slot; None }

    /// Give buffer `index` to the hardware. Callable from any thread or task: a frame
    /// dropped anywhere goes straight back (V4L2 `QBUF` from the dropping thread, as today;
    /// a bit in the free mask the DMA interrupt picks the next target from on an MCU).
    fn queue(&self, index: u32) -> Result<(), Self::Error>;

    /// Start capturing. `sensor` is called here (ReceiverFirst/SensorFirst) or by the
    /// receiver's own machinery (ReceiverDriven: the Linux event thread serving the bridge).
    fn start<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), Self::Error>;
    fn stop<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), Self::Error>;
    /// Free the buffers (`REQBUFS 0`); frames still held keep their memory until dropped.
    fn release(&self) -> Result<(), Self::Error>;

    fn poll_sync(&self, cx: &mut Context<'_>) -> Poll<Result<SyncEvent, Self::Error>>;
    fn poll_done(&self, cx: &mut Context<'_>) -> Poll<Result<FrameDone, Self::Error>>;
    /// Never wait: superloops, and the Linux event thread after its own `poll(2)`.
    fn try_sync(&self) -> Result<Option<SyncEvent>, Self::Error>;
    fn try_done(&self) -> Result<Option<FrameDone>, Self::Error>;
}
```

Why two event channels and not one: the frame-start work (writing the controls due this
frame) must happen at the frame start even while the consumer is busy processing the
previous frame (the software ISP holds it for 3 ms). One queue would make the busy consumer
the one to drain it. Today's code has the same split (event thread: `DQEVENT`; consumer:
`DQBUF`), and the PiSP path's "driven" mode, where the pipeline thread serves frame starts
itself because it wakes anyway, is "one task polls both" (3.2).

Errors: a fatal error (`Disconnected`, `Io` from the queue) ends the stream after one error
item, as `FrameStream` does today; `Glitch` and `FrameDone::corrupt` are counted and the
stream goes on until `max_error_frames` corrupt frames in a row.

### 2.6 Lens

`LensActuator` moves from `styx-native` unchanged except for the error:

```rust
pub trait LensActuator {
    type Error: HalError;
    fn power(&mut self, on: bool) -> Result<(), Self::Error>;
    fn move_to(&mut self, position: i32) -> Result<(), Self::Error>;   // driver units
}
pub struct NoLens;   // impl with Unsupported; the runtime treats Unsupported as "no lens"
```

A VCM on the sensor's I²C bus is `I2cVcm<B: RegisterBus>`-like and portable (the command
formats are `styx-sensor` data); Linux adds `KernelLens` (`FOCUS_ABSOLUTE`). The move-time
model, the frame-exact `LensSchedule` and PDAF decoding are already in `styx-sensor` (N).

### 2.7 `embedded-hal` adapters (least setup on MCUs)

Behind feature `embedded-hal` (1.0 traits, `no_std`):

```rust
/// Any embedded-hal I²C master as a sensor register bus.
pub struct I2cBus<I> { i2c: I, address: u8, address_bits: u8, bursts: bool }
impl<I: embedded_hal::i2c::I2c> RegisterBus for I2cBus<I> { /* write_read / write, bursts */ }

/// Reset, power-down and an optional enable for the clock and supplies from embedded-hal pins.
pub struct BoardPins<R, P, C, D> { pub reset: Option<R>, pub powerdown: Option<P>,
                                   pub xclk: Option<C>, pub delay: D, /* supplies */ }
impl<R: OutputPin, P: OutputPin, C: ClockEnable, D: DelayNs> SensorPins for BoardPins<R, P, C, D> { /* by role */ }

/// The camera's input clock where the MCU makes it (STM32 MCO, ESP32 LEDC): two methods.
pub trait ClockEnable { fn enable(&mut self, hz: u32) -> Result<u32, ErrorKind>; fn disable(&mut self); }
```

`embedded-hal-async` versions come later if wanted (10, open question 1): the
synchronous bus is enough at frame rates (a group-held exposure/gain/frame-length write is
about 0.5 ms at 400 kHz) and keeps the trait object-free.

> **As built:** the review chose async I²C now. embedded-hal and embedded-hal-async are
> always-on dependencies (the vocabulary, not adapters): `BoardPins` implements both pin
> traits, `I2cRegisters` both bus traits, and `AsyncSensorDriver` runs the driver over the
> async ones (Status, "Async I²C").

---

## 3. Event, wake and threading model

### 3.1 Wakers are the only contract

Everything waits through `core::task::Waker`. The HAL never says how a waker is woken; each
platform has one natural answer:

```
                    Linux (today, unchanged)           Embassy (MCU)                Superloop / RTOS
frame start    event thread: poll(2) on the capture   VSYNC interrupt → pushes     main loop calls
               node (POLLPRI) → DQEVENT → service()   SyncEvent, AtomicWaker::wake  try_sync() each turn,
               (quiesced around STREAMON/OFF)          → sensor task runs           WFI when idle
frame done     event thread sees POLLIN → wakes the   DMA/frame-end interrupt →    try_done()
               stream's waker → consumer DQBUF         FrameDone, wake
isp job        PiSP BE: poll on output nodes;         ISP done interrupt           poll the job
               soft ISP: synchronous                   (DCMIPP, ESP32-P4)
timeouts       styx-graph rt timers (timerfd)          embassy-time                 tick counter
```

On Linux the existing pieces stay: `styx-graph::rt` (the epoll reactor) for async users of
`FrameStream`, the event thread's own `poll(2)` for the capture node (because of the
`rp1-cfe` node lock, `events.rs`), `next_blocking` for blocking users. The runtime adds no
reactor; it only calls `poll_sync`/`poll_done`/`try_*`.

### 3.2 The three parts of a running camera

```
           ┌─────────────────────── Camera<P>::start() ───────────────────────┐
           │                                                                  │
   SensorService<P>                 FrameStream<P>                  Controls<P> (Clone)
   frame starts → schedule writes   filled buffers → Frame leases   request_at / request_at_now,
   embedded data → reports          frame-sync fallback, drops,     set_exposure, lens, test
   lens moves, PDAF                 corrupt limit, metrics          pattern, flips
   service() | poll(cx)             poll_next(cx) | try_next()       (locks the sensor state)
           │                                 │                               │
           └────────── Shared<SensorState<P>> (the sensor driver, scheduler, lens) ──┘
```

`Shared<T>` is `Arc<std::sync::Mutex<T>>` with feature `std` and `Rc<RefCell<T>>` without it:
on an MCU the three parts run in one task (interrupt handlers touch only the receiver's
atomics and wakers), so no lock and no critical section is ever held across an I²C transfer.
This is a cargo feature, not a generic: one binary has one answer, and the Linux build is
byte-for-byte today's locking.

How each platform runs them:

| work | Linux raw/soft path | Linux PiSP path ("driven") | Embassy | superloop |
|---|---|---|---|---|
| sensor service | the event thread: `poll(2)` then `service()` (serves the bridge inside the receiver) | the pipeline thread calls `service()` whenever it wakes and adds the frame-start fd to its wait | a task `select`ing `sensor.serve()` with the frame future, or its own task | `service()` each turn |
| frames | consumer thread (`next_blocking`, `block_on`, tokio) | pipeline thread | camera task | `try_next()` |
| ISP | software ISP: worker thread + helper pool (`soft_threads`); PiSP BE: same thread, job overlapped with the algorithms | same | same task (or a second task for a hardware ISP's completion) | inline |
| 3A algorithms | worker thread, after the ISP job is queued | pipeline thread, while the BE works (15 Hz settled) | camera task, between frames | inline |
| stills | still thread (`StillJob`) | still thread, BE node group 1 | low-priority task, or skipped | skipped |

Nothing in the runtime spawns: `std::thread` stays in `styx-native`, `styx-pipeline[device]`
and `styx`'s workers, which remain the deployment choice for Linux.

---

## 4. The portable runtime (`styx-runtime`)

### 4.1 What moves in

| from | what | into `styx-runtime` |
|---|---|---|
| `styx-native` `control.rs` | `SensorControl`, `ControlHandle`, `BringUpTimes`, `standby_problems`, write margin | `sensor::{SensorState, Controls}` |
| `styx-native` `camera.rs` (sequencing half) | open → power → chip id → stream-off → init → mode → timing → receiver → start values | `camera::Camera::{open, configure}` |
| `styx-native` `session.rs` (ordering) | start/stop order, undo on failure, restart with held frames | `camera::Camera::{start, stop}` |
| `styx-native` `buffers.rs` (bookkeeping) | lease counting, `live` flag, give-back on drop, frames outliving the stream | `buffers::{Pool, Frame}` |
| `styx-native` `stream.rs` (logic) | sync fallback, gap and corrupt counting, waker protocol | `stream::FrameStream` |
| `styx-native` `health.rs`, `modes.rs`, `formats.rs`, `lens.rs` (`LensControl`, `PdafFrames`) | | `health`, `modes`, `lens` |
| `styx-pipeline` `device/{pisp,soft}.rs` (orchestration) | settled-rate decisions, retarget, request application, raw copy for stills, warm starts | `pipeline::ProcessedCamera` |
| `styx` `native_isp/still_runner.rs` (decisions) | which frame a still/bracket shot is, landed/verified checks | `still::StillRunner` |
| `styx` `metrics` (the counters the native workers bump) | frames, drops by cause, corrupt, ISP skipped, restarts, 3A state | `metrics::Counters` |

What does not move: the brains (they are already libraries the runtime calls), anything
in section 5's "stays Linux" list.

### 4.2 Structure

```rust
/// Everything a camera runs on, as associated types: one generic parameter instead of six.
pub trait Platform: Sized + 'static {
    type Clock: Clock + Clone;
    type Bus: RegisterBus;
    type Pins: SensorPins;
    type Receiver: Receiver;
    type Lens: LensActuator;
    /// Where processed outputs and copies come from (soft ISP outputs, still copies).
    type Memory: DmaMemory;
}

pub struct Parts<P: Platform> {
    pub clock: P::Clock,
    pub bus: P::Bus,
    pub pins: P::Pins,
    pub receiver: Shared<P::Receiver>,   // shared with interrupt handlers / the event thread
    pub lens: Option<P::Lens>,
    pub memory: P::Memory,
}

pub struct Camera<P: Platform> {
    sensor: Shared<SensorState<P>>,      // SensorDriver<P::Bus, P::Pins> + scheduler + lens
    receiver: Shared<P::Receiver>,
    clock: P::Clock,
    pool: Option<Shared<Pool<P>>>,
    config: Option<Configured>,
    counters: Shared<Counters>,
    options: CameraOptions,              // buffers, write margin, max_error_frames, ...
}

impl<P: Platform> Camera<P> {
    pub fn open(parts: Parts<P>, desc: Arc<SensorDescription>, options: CameraOptions) -> Result<Self>;
    pub fn modes(&self) -> impl Iterator<Item = SensorMode> + '_;
    pub fn configure(&mut self, s: &StreamSettings) -> Result<Configured>;
    pub fn start(&mut self) -> Result<Running<P>>;     // (SensorService, FrameStream, Controls)
    pub fn stop(&mut self) -> Result<()>;
    pub fn close(self) -> Result<()>;                  // standby, power down, whatever failed
}

pub struct Frame<P: Platform> {                         // a lease: Drop queues the buffer again
    pub sequence: u64, pub timestamp: Instant, pub bytes_used: usize, pub corrupt: bool,
    pub controls: Option<FrameControls>,               // what produced it (embedded or predicted)
    index: u32, pool: Shared<Pool<P>>,
}
impl<P: Platform> Frame<P> {
    pub fn data(&self) -> &[u8];                         // begin_cpu(Read) on first access
    pub fn buffer(&self) -> &<P::Receiver as Receiver>::Buffer;  // export(), device_address()
}
```

ISP backends are runtime traits (they are typed in the brains' vocabulary):

```rust
/// An ISP in the capture path: statistics come with the frame, settings go into a parameter
/// queue ahead of the frame (PiSP front end, rkisp1, STM32 DCMIPP, ESP32-P4 ISP).
pub trait InlineIsp {
    type Error: HalError;
    fn set_settings(&self, from_frame: u64, s: &IspSettings) -> Result<(), Self::Error>;
    /// Converts the statistics delivered with `frame` (Receiver::statistics) in place.
    fn statistics(&self, frame: &FrameDone, raw: &[u8], out: &mut Statistics) -> bool;
}

/// An ISP that processes a frame from memory into output buffers (PiSP back end, the
/// software ISP, the GPU ISP, a vendor memory-to-memory ISP).
pub trait FrameIsp {
    type Error: HalError;
    type Input<'a>;                 // a receiver buffer: &[u8], a dma-buf, an index
    type Job;
    type Output;                    // a lease on output buffers, returned on drop
    fn configure(&mut self, raw: &RawFormat, outputs: &[OutputSpec]) -> Result<(), Self::Error>;
    fn set_settings(&mut self, s: &IspSettings);              // from the next submit on
    fn submit(&mut self, input: Self::Input<'_>) -> Result<Self::Job, Self::Error>;
    fn poll_job(&mut self, job: &mut Self::Job, cx: &mut Context<'_>)
        -> Poll<Result<Self::Output, Self::Error>>;
    /// Statistics produced while processing (software/GPU ISP); `false` if it makes none.
    fn take_statistics(&mut self, out: &mut Statistics) -> bool;
}

pub struct ProcessedCamera<P: Platform, I: FrameIsp, F: InlineIsp = NoInline> {
    camera: Camera<P>, frames: FrameStream<P>, sensor: SensorService<P>, controls: Controls<P>,
    isp: I, inline: F, controller: Controller, step: Option<Step>, settled_every: u64,
    stills: StillRunner, counters: Shared<Counters>,
}
```

The per-frame loop, written once (today it is written twice, `PispPipeline::next` and
`SoftPipeline::next`, plus the `styx` workers around them):

```rust
pub async fn next(&mut self) -> Result<Processed<P, I>> {
    let raw = self.frames.next().await.ok_or(Error::Stopped)??;
    let values = SensorValues::from_frame(&raw);          // embedded data or the prediction
    let run = self.algorithms_due(&values);               // 15 Hz when AE locked, AWB converged
    if let Some(step) = &mut self.step {
        self.controller.retarget(&mut step.isp, &step.params, &values); // digital gain for F
        self.isp.set_settings(&step.isp);
    }
    let mut job = self.isp.submit(raw.input())?;          // the BE works from here
    self.sensor.service()?;                               // a frame start that came meanwhile
    let mut stats = Statistics::default();
    let inline_stats = run && self.inline.statistics(raw.done(), raw.stats(), &mut stats);
    if inline_stats { self.run_algorithms(&stats, &values)?; }   // overlapped with the job
    let out = poll_fn(|cx| self.isp.poll_job(&mut job, cx)).await?;
    if run && !inline_stats && self.isp.take_statistics(&mut stats) {
        self.run_algorithms(&stats, &values)?;            // software ISP: settings for F + 1
    }
    self.stills.consider(&raw, &values, self.step.as_ref());
    self.counters.frame_processed(&values);
    Ok(Processed { out, values, raw_held: self.stills.take_copy() })
}

fn run_algorithms(&mut self, stats: &Statistics, values: &SensorValues) -> Result<()> {
    let step = self.controller.process_with_lens(stats, values, self.controls.lens_state())?;
    if let Some(r) = &step.sensor { self.controls.request_at_now(r.frame, &r.into())?; }
    if let Some(l) = &step.lens { self.controls.request_lens_at(l.frame, l.position)?; }
    self.inline.set_settings(step.frame + 1, &step.isp)?;
    self.step = Some(step);
    Ok(())
}
```

A blocking `next_blocking(timeout)` wraps it with `styx_graph::rt::block_on` on Linux (as
`FrameStream::next_blocking` today); MCUs await it in a task.

### 4.3 Generics vs `dyn` on the frame path: why Linux pays nothing

Per frame today (raw path, OV9782 1280x800 at 30 fps):

| step | today | after |
|---|---|---|
| frame-start event | event thread `poll(2)`, `DQEVENT`, `Arc<dyn SensorSide>::frame_start` → `Mutex` → scheduler → I²C `ioctl(I2C_RDWR)` | same syscalls; `SensorState<Linux>::frame_start` called statically |
| embedded data | `DQBUF`/`QBUF` on the embedded node, `report_embedded` (dyn) | same syscalls, static call |
| filled buffer | wake, `Arc<dyn CaptureDevice>::dequeue` → `DQBUF`; `applied(seq)` (dyn, mutex) | `Receiver::try_done` (static) → `DQBUF`; `applied` static |
| frame lease | `Arc<Lender>` clone; `data()` → `DMA_BUF_IOCTL_SYNC` once | `Shared<Pool>` clone (the same `Arc`); same sync |
| drop | `give_back` → `QBUF` | `Receiver::queue` → `QBUF` |

`Camera<Linux>` is the only instantiation in a Linux binary, so monomorphisation duplicates
nothing; the dynamic calls become direct (and inlinable). The locks are the same
`std::sync::Mutex`es. The `SensorStart` callback is the only new `dyn`-like boundary and runs
twice per session. `SensorBus` and `CameraPins` stay enums (I²C vs kernel subdev, bridge vs
none), which is static dispatch already.

What "zero cost" is checked with (each restructuring step, section 7):

1. **Syscalls per frame equal**: `strace -c -f` over 300 frames of `native-pipeline pisp` and
   `examples/landing`; the counts of `ioctl`, `poll`, `read`, `write` must match the baseline
   (`~20` V4L2 ioctls and two waits per frame on the PiSP path).
2. **CPU per frame within noise**: `native-pipeline pisp --no-read --profile` 0.26 ms at
   30 fps, 0.22 ms at 120; Styx API NV12 0.26 ms; software ISP RGB24 2.9 ms one thread;
   tolerance +3% (the run-to-run spread).
3. **Latency**: frame start → consumer median 8.35 ms, p95 8.39 ms (PiSP, 30 fps), ±0.05 ms.
4. **Start-up**: open → first frame ≤ 35 ms (29-34 ms today); AE locked by frame 6.
5. **Memory**: peak RSS of the Styx API process ≤ 17.8 MiB + 2%; allocations per frame in
   steady state ≤ today's (counting allocator in the mock-platform test, 4.4).
6. **Code**: `cargo bloat` of `native-pipeline` within +1%; `cargo llvm-lines` shows one copy
   of `Camera<_>`.

Alternative considered: `dyn Receiver` everywhere (object-safe traits, `Box<dyn>`). Simpler
signatures, no `Platform` trait, but the HAL could then not use associated buffer types,
GATs (`Export<'a>`, `Input<'a>`) or generic `start<S>`, and every MCU call would be indirect.
Rejected; the generics are contained in one trait and type aliases hide them
(`type NativeCamera = Camera<Linux>`).

### 4.4 Memory model

Three levels; a target picks one by features, the runtime code is the same:

| model | for | what allocates when |
|---|---|---|
| `std` | Linux | as today |
| `alloc`, no allocation after start | MCUs with a heap (`embedded-alloc`, `esp-alloc` in PSRAM) | descriptions, tuning, scheduler rings, pools and algorithm state at `open`/`configure`; the frame path allocates nothing |
| `alloc` + static buffers | MCUs that want frames outside the heap | as above, frame buffers from `StaticDma` regions placed by the linker; the heap only holds bookkeeping (a few KB to tens of KB, more with 3A) |

Making the frame path allocation-free is a real change, and it helps Linux slightly too:

* `ControlScheduler` keeps `BTreeMap`s per control and per frame (`pending`, `committed`,
  `reported`, 64 frames of history): becomes fixed rings of `HISTORY_FRAMES` entries and a
  small sorted array of pending requests (at most `max_delay + 2` in flight; overflow refuses
  the request with `InvalidConfig` rather than allocating).
* `request`/`request_now` return `Vec<Landing>` and `frame_start` builds a `Vec<RegWrite>`
  batch: become `ArrayVec<_, 4>` (four controls) and a fixed batch buffer in the driver.
* `report` returns `Vec<Mismatch>`: `ArrayVec<_, 4>`.
* `Controller` and `styx-algo` reuse their statistics and parameter buffers per frame (the
  nostd agent's work; checked by the same counting-allocator test).

The test: the existing counting allocator (`crates/styx/src/test_alloc.rs` counts sizes; add
counts) wraps a mock-platform camera running 300 frames with AE/AWB/flicker and a bracket;
the steady state must allocate zero times outside stills. It runs on the host in CI.

Sensor descriptions and tunings: parsed from TOML/JSON at run time on Linux (unchanged).
`toml` needs `std`, so `no_std` targets get them from a build script that parses the files
on the host and emits `postcard` bytes (`include_bytes!`), deserialised at `open` (serde is
`no_std` + `alloc`). The validator (`desc/validate.rs`) runs in the build script, so a broken
description fails the firmware build, not the boot.

### 4.5 Error model

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,          // from styx-hal: Disconnected, Busy, Timeout, ...
    pub during: &'static str,     // "chip id", "VIDIOC_DQBUF", "frame-start write", ...
    pub code: Option<i32>,        // errno / vendor status from HalError::code
}
```

No allocation, `Copy`, enough for every decision the runtime makes (`is_disconnect()` for
the supervisor, `Busy` for exclusive open, `Timeout` for the bridge acknowledgement). A
sensor-level error (`SensorError` from `styx-sensor`: chip id mismatch, invalid mode) maps
to `InvalidConfig`/`NotFound` with its static context. `styx-native` keeps `NativeError`
(with its `String` contexts and `Display`) as its public error, built from `Error` with a
`From` impl, so `styx` and HeliOS see no change. Faults during streaming keep today's
semantics: one error item, then the stream ends; the camera still has to be stopped or
dropped, which puts the sensor in standby and powers it down whatever failed.

---

## 5. Linux as the reference implementation

### 5.1 Mapping

| HAL / runtime piece | Linux implementation | today's code |
|---|---|---|
| `Clock` | `LinuxClock` (`CLOCK_MONOTONIC`) | `styx_kernel::monotonic_now` |
| `Delay` | `std::thread::sleep` | `SensorPins::delay` default |
| `RegisterBus` | `SensorBus::{I2c(I2cRegisterBus<I2cBus>), Kernel(SubdevBus)}` (Lemnos's register map on Lemnos's i2c-dev bus) | `sensor_bus.rs`, `regbus.rs` |
| `SensorPins` | `CameraPins::{Bridge(BridgePins), None(NoPins)}` | `sensor_bus.rs` |
| `DmaMemory`, `DmaBuffer` | `HeapMemory` (dma-heaps), `V4l2Buffer` (MMAP + `EXPBUF`), `Export = BorrowedFd` | `buffers.rs`, `styx_kernel::dma_heap` |
| `Receiver` (raw route) | `V4l2Receiver`: video node, optional embedded node, the bridge (`StartOrder::ReceiverDriven`) or a kernel-driven sensor (`ReceiverFirst`), the event thread with its quiesce gate | `device.rs`, `events.rs`, `session.rs`, `embedded.rs`, `camera.rs` (links, pads, formats) |
| `Receiver` (PiSP) | `PispFeReceiver`: `fe_image0` + `fe_stats` + FE config queue (+ the bridge); `stats_slot` on each `FrameDone` | `external.rs`, `styx_pisp::device::FrontEndDevice` |
| `InlineIsp` | `PispFrontEnd` (`IspSettings::apply_fe`, `stats::from_pisp_raw`) | `pipeline/device/pisp.rs` |
| `FrameIsp` | `PispBackEnd` (`BackEndStream::process_queued`/`wait_job`, `BeConfigBuilder`), `SoftIsp` (`SoftLoop`, helper threads), `GpuIsp` | `pipeline/device/*`, `soft.rs`, `styx-gpuisp` |
| `LensActuator` | `KernelLens`, `I2cVcm` (`lemnos_drivers_vcm::Vcm`) | `lens.rs` |
| `Platform` | `struct Linux;` | (new, 15 lines) |
| `NativeCamera` | `pub struct NativeCamera { camera: Camera<Linux>, info: CameraInfo, ... }` with today's methods | `camera.rs` |
| `PispPipeline`, `SoftPipeline` | `ProcessedCamera<Linux, PispBackEnd, PispFrontEnd>`, `ProcessedCamera<Linux, SoftIsp>` behind today's names | `pipeline/device/*` |

The `rp1-cfe` pitfalls (README) are properties of one Linux receiver and stay inside
`V4l2Receiver`/`PispFeReceiver`: the own-`poll(2)` event thread, the quiesce around
`STREAMON`/`STREAMOFF`, stream-off before init (that one is portable and moves into the
runtime's bring-up sequence: any sensor left streaming by a dead owner needs it), the
embedded node being the last to stop, `report_start_errors`.

### 5.2 What stays Linux-only

* Discovery and description of hardware: media topology, subdev probing of kernel-driven
  sensors (`kernel.rs`), the bridge, `CameraInfo`, the search paths for descriptions and
  tunings, hotplug, the `styx-graph` `Provider`.
* `styx-kernel`, `styx-graph::rt`, usbfs UVC (`styx-uvc` device/stream/hotplug), V4L2
  probing of other cameras, libcamera compat.
* The `styx` facade: `BackendKind` dispatch, capture workers and supervisors, `FrameLease`
  and the shared-capture fan-out, the planner, the camera service and IPC (`FrameSocket`,
  dma-buf passing), recordings (MCAP, `.styxrec`), metrics export (Prometheus, `/proc` CPU),
  PipeWire and GStreamer bridges, GPU ISP.
* Persistence: warm starts on disk (`warm.rs` keeps an in-memory default; the runtime takes a
  `WarmStore` callback), replay recordings to files.

### 5.3 The facade and the planner

`styx`'s native backend keeps calling `NativeCamera` and the pipelines through the same
names, so `capture_api/native_backend.rs`, `native_isp/*`, the planner's native routes and
costs, and the camera service need no change beyond imports. The metrics they record come
from the runtime's `Counters` (read by `CaptureMetrics`), replacing the counters the workers
bump by hand.

The planner stays `std` and in `styx`. It plans across backends (V4L2, UVC, libcamera,
netcam, replay), shared captures, codecs and processes; an MCU has one camera, one consumer
and a fixed configuration, so the runtime's `select_mode` (moved from `camera.rs`) is the
right tool there. The rate logic (`planner/rate.rs`: default 30 fps within a range, exact
rates) is pure and could move into `styx-runtime::modes` later if a second user appears;
not part of this plan.

---

## 6. Porting guide

### 6.1 What a new target implements

| | required | from where |
|---|---|---|
| `Receiver` | yes | write it: the only substantial code |
| `DmaMemory` / `DmaBuffer` | yes (often 50-150 lines) | write it, or reuse `StaticDma` |
| `Clock` | yes (10-20 lines) | the platform timer (`embassy-time`, a SysTick counter, `CLOCK_MONOTONIC`) |
| `RegisterBus` | yes | Lemnos's `I2cRegisters` over the chip HAL's embedded-hal I²C (`SensorDescription::i2c_registers`): zero lines |
| `SensorPins` | yes | `BoardPins` over embedded-hal `OutputPin`s + a Lemnos `ClockOutput`: a constructor call |
| sensor description | per sensor | TOML (identity, power sequence, init/mode registers, controls, delays, a `[bus]` section) |
| `InlineIsp` / `FrameIsp` | only with a hardware ISP | statistics conversion + settings mapping |
| `LensActuator` | only with a lens | `lemnos_drivers_vcm::Vcm` on the sensor bus: zero lines |
| tuning | only for the 3A loop | `styx-tune` or a Raspberry Pi tuning |

New description section, used where no device tree describes the bus:

```toml
[bus]
parallel = { width = 8, pclk_rising = true, hsync_active_high = true, vsync_active_high = false }
# or: csi2 = { lanes = 2, continuous_clock = true }
```

### 6.2 Sketch (a): STM32H7 + OV5640 on DCMI, Embassy

Hardware: STM32H743/H750 (Cortex-M7, 480 MHz, 1 MB SRAM, D-cache), DCMI 8-bit parallel,
OV5640 in DVP mode with XCLK from MCO1, reset and power-down on GPIOs, frames in AXI SRAM
(QVGA/VGA YUV422) or external SDRAM (larger, JPEG).

```rust
// ports/stm32h7-dcmi (sketch, not compiled)
pub struct H7;
impl Platform for H7 {
    type Clock = EmbassyClock;                                   // embassy_time::Instant → ns
    type Bus = I2cBus<embassy_stm32::i2c::I2c<'static, Blocking>>;
    type Pins = BoardPins<Output<'static>, Output<'static>, Mco, Delay>;
    type Receiver = DcmiReceiver;
    type Lens = NoLens;
    type Memory = StaticDma;
}

pub struct DcmiReceiver {
    slots: [StaticBuffer; 3],             // 32-byte aligned, in AXI SRAM (DMA-reachable)
    free: AtomicU8,                       // bitmask of queued slots (Receiver::queue sets a bit)
    active: AtomicU8,                     // the slot the DMA writes now
    sequence: AtomicU32,
    sync: Channel<SyncEvent, 4>,          // ISR → sensor service (heapless spsc + AtomicWaker)
    done: Channel<FrameDone, 4>,          // ISR → frame stream
}

impl Receiver for DcmiReceiver {
    fn caps(&self) -> ReceiverCaps {
        ReceiverCaps { frame_start_events: true, embedded_data: false,
                       timestamp: TimestampPoint::FrameEnd, start_order: StartOrder::ReceiverFirst,
                       max_buffers: 3 }
    }
    fn configure(&self, c: &ReceiverConfig) -> Result<Configured, DcmiError> {
        // CR: EDM (8/10/12/14 bit), PCKPOL/HSPOL/VSPOL from Bus::Parallel, JPEG bit for
        // compressed formats, CM = continuous; DMA stream: peripheral → memory, 32-bit words,
        // length = frame bytes / 4 (≤ 65535 words per stream: larger frames need the MDMA
        // linked list or a split); enable VSYNC, FRAME, OVR, ERR interrupts.
    }
    fn queue(&self, i: u32) -> Result<(), DcmiError> { self.free.fetch_or(1 << i, Release); Ok(()) }
    fn start<S: SensorStart + Clone>(&self, sensor: S) -> Result<(), DcmiError> {
        self.arm_next_slot()?; /* CAPTURE = 1 */ sensor.start().map_err(Into::into)
    }
    fn poll_sync(&self, cx: &mut Context<'_>) -> Poll<Result<SyncEvent, DcmiError>> { self.sync.poll(cx) }
    fn poll_done(&self, cx: &mut Context<'_>) -> Poll<Result<FrameDone, DcmiError>> { self.done.poll(cx) }
    // try_sync / try_done / stop / release / buffer: a few lines each
}

#[interrupt]
fn DCMI() {
    let rx = RECEIVER.get();
    let mis = pac::DCMI.mis().read();
    if mis.vsync_mis() { rx.frame_start(now()) }         // SyncEvent::FrameStart, wake
    if mis.frame_mis() { rx.frame_end(now()) }            // FrameDone{index, bytes_used from NDTR},
                                                          // re-arm DMA on the next free slot, or
                                                          // count an overrun and reuse the slot
    if mis.ovr_mis() || mis.err_mis() { rx.glitch() }
    pac::DCMI.icr().write(|w| w.0 = 0x1f);
}

#[embassy_executor::task]
async fn camera(parts: Parts<H7>) {
    let desc = styx_runtime::description!("ov5640-dvp");    // build-time postcard blob
    let mut cam = Camera::<H7>::open(parts, desc, CameraOptions::default()).unwrap();
    cam.configure(&StreamSettings::new(640, 480).fourcc(FourCc::YUYV).fps(15)).unwrap();
    let Running { mut sensor, mut frames, controls } = cam.start().unwrap();
    controls.set_exposure(Duration::from_millis(10)).ok();   // lands on the predicted frame
    loop {
        match select(sensor.serve(), frames.next()).await {
            Either::First(r) => r.unwrap(),                    // frame start: due writes on I²C
            Either::Second(Some(Ok(frame))) => consume(&frame), // drop → back to the DMA ring
            Either::Second(_) => break,
        }
    }
}
```

What the OV5640 brings: it has its own ISP (AEC/AGC, AWB, demosaic, YUV/RGB565/JPEG
outputs), so the usual configuration is the sensor's own 3A with Styx doing bring-up,
modes, exact frame rates (VTS), frame-exact manual exposure and gain when asked
(`0x3500-0x3502`, `0x350a-0x350b`, group hold `0x3212`), frame starts, buffers and the
stream. Styx's own 3A needs raw output (OV5640 has RAW8/10) and then the software ISP at
small sizes (8.4). Without embedded data, frames report the predicted values
(`FrameControls::verified = false`), as on kernel-driven sensors without a metadata pad.

Variant: **ESP32-S3** (LCD_CAM camera interface, GDMA descriptor lists into octal PSRAM,
esp-hal's camera driver underneath): same structure, `Receiver` over the descriptor ring,
`DmaBuffer::begin_cpu` invalidates the PSRAM cache lines. OV2640 (JPEG up to UXGA) is the
common module; JPEG frames have variable `bytes_used` and a buffer sized for the worst case.
**ESP32-P4** and **STM32N6** add a MIPI CSI-2 receiver and a small ISP with statistics; they
fit `Bus::Csi2` + `InlineIsp`, which is where a real 3A loop on an MCU becomes reasonable.

Estimated size of the port:

| piece | lines |
|---|---|
| `Platform` impl, `EmbassyClock`, glue | 60 |
| `StaticDma` (shared, written once in `styx-hal`) | 0 (120 once) |
| `DcmiReceiver` + ISR + DMA programming | 450-650 |
| board setup (pins, MCO, I²C, `Parts`) | 60-100 |
| OV5640 DVP description (TOML, mostly register tables) | 300-400 data |
| **code total** | **~600-800** |

### 6.3 Sketch (b): a non-Pi Linux SoC with its own ISP (`rkisp1`)

Target: the upstream `rkisp1` driver, which covers Rockchip RK3399 and PX30 and, in recent
kernels, the NXP i.MX8M Plus ISP. An inline ISP: sensor → MIPI receiver → ISP → main path
and self path video nodes (NV12/YUYV/raw with resizers), with a `rkisp1_params` meta output
node (`V4L2_META_FMT_RK_ISP1_PARAMS`) and a `rkisp1_stats` meta capture node
(`V4L2_META_FMT_RK_ISP1_STAT_3A`), and `FRAME_SYNC` events on the ISP subdevice.

Everything on the Linux side is reused: `styx-kernel`, discovery through the media graph,
kernel-driven sensors (`imx219`, `ov5647`, ... with their `*.kernel.toml`) or the bridge
(it is generic; it needs an overlay for that board's receiver endpoint), the control
schedule, buffers, the provider, the planner, the service. New:

```
RkIsp1Receiver        V4l2Receiver configured on rkisp1_mainpath (+ selfpath): links, ISP pad
                      formats (raw in, YUV out), resizer sizes; stats node buffers delivered
                      as FrameDone::stats_slot; FRAME_SYNC from the isp subdev
RkIsp1 (InlineIsp)    params: #[repr(C)] uAPI (rkisp1-config.h): BLS, AWB gains, CTK (CCM +
                      offsets), goc (gamma out), LSC (17x17 sectors), DPCC, DPF (denoise), flt
                      (sharpen), ie/cproc; IspSettings → params, queued per frame ahead;
                      stats: AE means (5x5 or 9x9 per version), AWB means (one window), 16- or
                      32-bin histogram, AF sums → styx_algo::Statistics
graph                 rkisp1 topology → DeviceGraph nodes for the planner (main/self paths
                      as two outputs of one IspStage, like be_output0/1)
```

```rust
pub struct RkIsp1 { params: V4l2MetaOut, stats: Arc<V4l2MetaCap>, version: Rkisp1Version }

impl InlineIsp for RkIsp1 {
    type Error = KernelError;
    fn set_settings(&self, from_frame: u64, s: &IspSettings) -> Result<(), KernelError> {
        let mut p = rkisp1_params_cfg::default();
        p.set_bls(s.black_level);                 // per channel, sensor bit depth → 12 bit
        p.set_awb_gains(s.channel_gains());       // × digital gain, as apply_be does
        p.set_ctk(&s.ccm, s.offsets);             // 3x3 + offsets, fixed point 3.7 (v10)
        p.set_goc(gamma_points(s.gamma.as_ref())); // 17 or 34 segments by version
        if let Some(ls) = &s.lens_shading { p.set_lsc(&resample_17x17(ls)); }
        self.params.queue_for(from_frame, &p)     // one params buffer per frame, 2 ahead
    }
    fn statistics(&self, f: &FrameDone, raw: &[u8], out: &mut Statistics) -> bool {
        stats::from_rkisp1(rkisp1_stat_buffer::from_bytes(raw), self.version, out)
    }
}
// Platform: the Linux one, with Receiver = RkIsp1Receiver; the loop is
// ProcessedCamera<Linux, NoFrameIsp, RkIsp1>: no memory-to-memory pass at all.
```

Estimated size:

| piece | lines |
|---|---|
| `rkisp1` uAPI structs (repr(C), size asserts) | 450-550 |
| `IspSettings` → params (BLS, gains, CTK, gamma, LSC resampling, denoise/sharpen strengths) | 350-450 |
| statistics → `Statistics` (AE grid, single AWB window, histogram, AF) | 200-300 |
| receiver/topology configuration, graph, planner costs | 250-350 |
| **total** | **~1.3k-1.7k**, plus a tuning per sensor (`styx-tune`) |

For comparison `styx-pisp` is 7.4k lines, because the PiSP back end needs tiling, two
passes and a far larger configuration; `rkisp1` needs none of that.

What degrades: `rkisp1`'s AWB statistic is one mean over a window (the PiSP has 32x32
zones), so the Bayesian AWB runs on a 1-zone grid (it works, grey-world-like, less robust in
mixed light) unless the main path's statistics are complemented by the software ISP's zone
statistics on a downscaled self-path frame (an option, not in the estimate). The AE grid is
5x5 or 9x9 instead of 32x32, which costs metering precision but not convergence.

---

## 7. Migration plan

Rules for every step: one branch, merged into `native-stack` only when its checks pass;
public API of `styx-native`, `styx-pipeline` and `styx` unchanged (type aliases and
re-exports where things moved); no behaviour change except where the step says so.

**Gate A (host, every step):** `cargo fmt`, `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo test --workspace`, `scripts/check-file-sizes.sh`,
`scripts/check-feature-combinations.sh`, the replay determinism tests (`styx-algo` replays
bit for bit, the virtual-sensor closed loop), fuzz smoke (`scripts/fuzz.sh` short).

**Gate B (CM5, under `scripts/with-device-lock.sh`, steps that touch the frame path):**

| check | tool | pass |
|---|---|---|
| bring-up registers | `native-pipeline regcheck` | all 93 registers read back identical |
| start-up | `native-pipeline pisp` (startup table) | open → first frame ≤ 35 ms; AE lock ≤ frame 6 cold, ≤ 2 warm |
| landing | `examples/landing` (30/60/120 fps) | exposure/gain land on the predicted frame, verified by embedded data |
| PiSP CPU and latency | `native-pipeline pisp --no-read --profile`, 300 frames, 30 and 120 fps | ≤ 0.27 / 0.23 ms per frame; latency median ≤ 8.40 ms |
| Styx API | `examples` NV12 1280x800 and NV12 + RG24 two consumers | ≤ 0.27 / 0.33 ms per frame |
| software ISP | `native-pipeline soft`, RGB24, 1 thread | ≤ 3.0 ms per frame |
| syscalls | `strace -c -f`, 300 frames, PiSP and raw | counts equal to the baseline run |
| back end configs | `native-pipeline be-replay` on a recording | byte-identical configs |
| robustness | `examples/native_harden` (killed owner, restart with held frames, reconnect), 15 min soak | first frame after kill ≤ 40 ms; no fd, dma-buf or RSS growth |
| stills | `examples/01_capture/still_capture` bracket | 3 shots on consecutive frames, `landed` |
| metrics | `metrics_top --overhead` | ≤ 300 ns per frame |
| comparison | `tools/compare/device-run.sh` | no row worse than the last report beyond its spread |

Baseline numbers are taken once on 840927c (plus the nostd merge) and kept in
`target/hal-baseline/` for the whole migration.

| # | step | effort | verification |
|---|---|---|---|
| 0 | **Prerequisite**: `native/nostd` merged (brains `no_std` + `alloc`). Record the Gate B baseline. | (other agent); 0.5 d baseline | Gate A, Gate B baseline |
| 1 | **`styx-hal` crate**: time, errors, `RegisterBus`/`SensorPins` (moved, `io` → `HalError`), `DmaMemory`/`DmaBuffer`, `Receiver`, `LensActuator`, `StaticDma`, embedded-hal adapters, a `mock` module (bus, pins, receiver with injectable faults, modelled on `fake.rs`/`fake_bridge.rs`). `styx-sensor` re-exports the bus traits. CI builds it for `thumbv7em-none-eabihf` and `riscv32imc-unknown-none-elf`. | 3-4 d | Gate A; `cargo build -p styx-hal --target thumbv7em-none-eabihf` |
| 2 | **`styx-sensor` on `styx-hal`**: errors through `HalError`, `delay` required, `KernelControl` → `SensorControlId`, the `[bus]` description section, build-script description compiler (`postcard`) for `no_std`. `styx-native`'s `SensorBus`/`CameraPins` implement the new traits. | 3 d | Gate A; Gate B rows regcheck, start-up, landing |
| 3 | **Allocation-free sensor frame path**: scheduler rings, `ArrayVec` landings and batches, counting-allocator test (zero allocations per frame in the scheduler and driver). | 3 d | Gate A (all `schedule_tests` unchanged); Gate B landing, PiSP CPU, syscalls |
| 4 | **`styx-runtime`, sensor side**: `SensorState` (from `SensorControl`), `Controls` (from `ControlHandle`), bring-up sequencing, lens control and PDAF, health, modes, `Shared<T>`, `Error`. `styx-native` uses them; `NativeCamera` API unchanged. | 5-7 d | Gate A (`control_tests`, `fault_tests` on the mock platform); full Gate B |
| 5 | **`styx-runtime`, frames**: `Receiver` implementations `V4l2Receiver` and `PispFeReceiver` (event thread, quiesce, bridge serving, embedded node moved inside verbatim), `Pool`/`Frame` leases, `FrameStream`, `SensorService`, `Camera<P>`; `NativeCamera` becomes a wrapper of `Camera<Linux>`; `start_external_driven` becomes "driven" polling. The riskiest step (the `rp1-cfe` locking). | 8-10 d | Gate A (async tests, fault tests: killed bridge, stop timeouts, disconnect, frames outliving the stream); full Gate B twice (two days apart) |
| 6 | **Processing loop into the runtime**: `styx-pipeline` core (`Controller`, `IspSettings`, `stats`, `pisp_be`, `SoftLoop`, `still`) `no_std` + `alloc`; `InlineIsp`/`FrameIsp`; `ProcessedCamera<P, I, F>`; `PispPipeline`/`SoftPipeline` become it with the PiSP and soft backends; GPU ISP as a `FrameIsp`. | 8-10 d | Gate A (replays bit for bit, virtual sensor loop); full Gate B, with BE configs byte-identical and IQ (`native-pipeline quality`) unchanged |
| 7 | **Stills and metrics counters**: `StillRunner` decisions and `Counters` into the runtime (`portable-atomic` for 64-bit counters on 32-bit MCUs); `styx` reads them. | 4 d | Gate A (metrics tests, still tests); Gate B stills, metrics, comparison |
| 8 | **`no_std` proof**: `styx-runtime` with `--no-default-features` builds for `thumbv7em-none-eabihf`; a host test runs `ProcessedCamera<MockPlatform, SoftIsp>` over a replayed raw recording (AE converges as in the replay tests) with `std` off in the runtime; CI job. | 2-3 d | Gate A; the new CI job |
| | **Core restructuring, steps 1-8** | **36-44 d (7-9 weeks)** | |
| 9 | **MCU port** (sketch a) as `ports/stm32h7-dcmi` (out of the workspace's default members): QVGA/VGA YUYV and JPEG, manual exposure landing (checked by mean brightness steps per frame), 1 h run. | 10-15 d, hardware-bound | on the board; host build in CI |
| 10 | **`rkisp1` port** (sketch b) on an RK3399 or i.MX8MP board with an IMX219 or OV5647. | 15-20 d | on the board: AE/AWB converge, landing, CPU per frame, vs libcamera's `rkisp1` |

Steps 1-3 are useful on their own (cleaner sensor crate, no allocations at frame rate) and
can stop there if the review prefers. Steps 4-5 are where the code actually moves.

### Risks

| risk | mitigation |
|---|---|
| A regression in the bridge/`rp1-cfe` start/stop handshake (stop timeouts, a start oops) | step 5 moves `events.rs` and the quiesce gate verbatim into `V4l2Receiver`; `fault_tests` run on the mock before the CM5; `native_harden` and the soak twice |
| Hidden per-frame cost from generic code (extra clones, locks taken twice) | syscall counts and CPU per frame in every Gate B; `cargo llvm-lines` |
| The HAL fits Linux and DCMI but not the next receiver | review the traits against three more receivers on paper before step 5 (ESP32 LCD_CAM, ESP32-P4/STM32N6 CSI, rkisp1); `styx-hal` stays 0.x |
| HeliOS breaks | public API kept; HeliOS's `styx-native-trial` branch built against each step |
| `no_std` creep fails late (a dependency pulls `std`) | step 1 adds the `thumbv7em` CI build; each later step keeps it green |
| 64-bit atomics on 32-bit MCUs (Cortex-M, ESP32-C3) | `portable-atomic` with the `critical-section` fallback for counters |
| Description/tuning parsing on MCUs | build-time compilation (step 2) instead of `toml` on target |
| 800-line file limit | the moved modules are already below it; splits planned with the moves |

---

## 8. Constraints and honest limits

### 8.1 Memory

* **No MMU, no virtual memory**: buffers are plain RAM, there is no `mmap`, no overcommit and
  no fallback when allocation fails. Every buffer is allocated at `configure`; `start` fails
  cleanly with `NoMemory` if they do not fit.
* **Frame sizes**: 640x480 YUYV is 600 KB; three buffers do not fit in an STM32H7's AXI SRAM
  (512 KB) with anything else, so VGA needs SDRAM/PSRAM or two buffers. QVGA (150 KB) fits.
  Raw 1280x800 10-bit packed is 1.25 MB; full-resolution raw from a 5 MP sensor (6 MB) is out
  of reach on any common MCU. JPEG from the sensor (OV2640/OV5640) is the way to large images.
* **Bandwidth**: PSRAM on ESP32-S3 is tens of MB/s effective; VGA YUYV at 30 fps is 18 MB/s
  of writes alone. Expect 15 fps VGA, 30 fps QVGA as realistic targets before measurement.
* **DMA reachability and alignment**: STM32H7 DTCM is not reachable by the DMA that serves
  DCMI; buffers go in AXI SRAM or SDRAM and must be cache-line aligned (32 bytes) with
  explicit invalidation. `Region` and `DmaBuffer::begin_cpu` exist for this.
* **Statistics and algorithms**: the full 3A state (ALSC tables, AWB priors, AF, flicker
  model) is tens of KB to a few hundred KB of heap; fine on H7-class parts, tight on
  ESP32-S3 SRAM (use PSRAM), too much for small Cortex-M4 parts (run without Styx 3A).

### 8.2 No dma-buf, no other processes

On MCUs `DmaBuffer::export` is `None`: frames are slices, zero-copy only within the firmware.
There is no camera service, no shared captures across processes, no PipeWire/GStreamer.
Consumers that want a frame later than the next DMA cycle hold it (the receiver then drops
frames: counted as overruns, as the PiSP path counts `OutputsHeld`) or copy it.

### 8.3 What degrades gracefully where

| feature | Linux CM5 | Linux SoC with its own ISP | MCU, sensor with an on-chip ISP (OV2640/OV5640) | MCU with CSI + ISP (ESP32-P4, STM32N6) |
|---|---|---|---|---|
| sensor bring-up from descriptions | yes | yes (bridge or kernel driver) | yes | yes |
| exact frame rates (VTS) | yes | yes | yes (where the description has the timing) | yes |
| frame-exact controls | yes, verified by embedded data | yes, predicted unless the driver has a metadata pad | yes, predicted (no embedded data on DVP) | predicted; embedded data if the CSI receiver captures it |
| immediate writes in the current frame | yes (FRAME_SYNC timestamps) | yes | yes if the clock resolves ≤ 100 µs | yes |
| frame starts | `FRAME_SYNC` events | events | VSYNC interrupt | interrupt |
| without frame starts | dequeue fallback (existing) | same | same | same |
| 3A in Rust | PiSP statistics | `rkisp1` statistics (coarser AWB) | normally the sensor's own; Styx 3A only on raw output + software ISP at small sizes | hardware statistics |
| ISP | PiSP / software / GPU | hardware | sensor's / software (QVGA-class) | hardware |
| stills, brackets | yes (BE group 1) | yes (software reprocess or main path) | brackets by exposure landing; DNG only for raw output, written as a stream to storage | yes |
| autofocus | simulation, IMX708 pending | yes with a VCM | VCM on I²C if present (contrast AF from soft stats) | yes |
| metrics | full, Prometheus | full | counters only (no `/proc`; CPU via the DWT cycle counter optional) | counters |
| hotplug, multiple processes, planner | yes | yes | no (fixed hardware, one firmware) | no |
| warm starts | memory + disk | memory + disk | memory, or a `WarmStore` in flash | same |

### 8.4 The software ISP on an MCU

`styx-softisp` builds `no_std` (nostd agent) and its scalar integer path is bit-exact with the
SIMD ones, so it runs on a Cortex-M7. Its cost there is unmeasured. Scaling from the A76
(2.9 ms/MP per thread with NEON fp16) by clock, issue width and the lack of SIMD suggests on
the order of 100 ms per megapixel on a 480 MHz M7, i.e. QVGA (0.08 MP) in roughly 10 ms:
usable at 15-30 fps for QVGA, not for VGA at 30. That figure is an estimate to measure in
step 9, not a promise. Helium (M55/M85) and the ESP32-P4's ISP change the picture.

### 8.5 Clocks and timing

MCU tick clocks are often 32 kHz or 1 MHz. Frame-exact scheduling needs only frame starts
(sequence numbers), so it is unaffected; "write in the current frame while 4 ms are left"
needs a frame start timestamp and a clock fine enough to compare with the margin, so it is
switched off below 100 µs resolution (requests then land one frame later, as without
`FRAME_SYNC` timestamps on Linux). Timestamps on `FrameDone` are taken in the interrupt at
frame end (DCMI) or frame start (CSI receivers that stamp it); `ReceiverCaps::timestamp`
says which, and latency metrics use it.

### 8.6 What this design does not cover

* USB cameras on MCU hosts: `styx-uvc`'s protocol core (descriptors, payload assembly, the
  PTS/SCR clock model) is neutral, but a `UsbHost` HAL trait (control, isochronous, bulk
  transfers) is a separate design.
* Async I²C (`embedded-hal-async`): not needed at today's write volumes; an async
  `RegisterBus` twin can be added without changing the runtime's structure (the sensor
  service is already a future).
* Encoding (JPEG/H.264) on MCUs: out of scope; sensors with JPEG output cover the common case.
* Non-Linux OSes with a kernel (Zephyr, QNX, an RTOS with a camera driver): they fit the same
  traits (Zephyr's video API as a `Receiver`), not sketched here.

---

## 9. Alternatives considered

| alternative | why not |
|---|---|
| Port `styx-kernel` to other OSes (a V4L2-like shim on MCUs) | V4L2's semantics (file descriptors, ioctls, poll) are what MCUs do not have; the shim would be larger than the receiver it hides |
| Fully sans-IO runtime (events in, actions out, the platform does all I/O) | maximal purity, but every platform would re-implement the I²C write timing, power sequencing and buffer queueing that the runtime should own; the HAL traits give the same testability through the mock platform |
| `async fn` in the HAL traits (Embassy-style) | needs an executor even for Linux's blocking users and the event thread; `poll_*` + `try_*` serve both, and `async fn` wrappers are a few lines on top |
| `dyn` HAL objects | see 4.3: loses associated types and GATs, adds indirect calls on MCUs |
| One crate (`styx-runtime` containing the HAL) | the HAL is what porters implement; keeping it small, allocation-free and stable separately is what makes "least setup" true |
| Move the planner into the portable runtime | it plans across backends, codecs and processes; MCUs need `select_mode` only |

## 10. Open questions for the review

Answered by the review:

1. Async I²C now or later (8.6)? **Now** (landed with step 2).
2. Keep `styx-native` as the Linux crate's name or rename to `styx-linux`? **Keep.**
3. Ports in-tree (`ports/`) or separate repositories? **In-tree, under `ports/`.**
4. Should steps 1-3 land before the HeliOS ship? **Steps 1-3 now; 4-6 after the HeliOS image
   proof.**
