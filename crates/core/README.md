# styx-core

Core primitives shared across the stack: buffer pools, frame leases/layouts, bounded queues, FourCc/format types, controls, and lightweight metrics.

## Documentation
- <https://docs.rs/styx-core-rs>

## Install
```toml
[dependencies]
styx-core-rs = "2.0.0"
```

## Modules
- `buffer`: `BufferPool`, `BufferLease`, `FrameLease`, plane views, frames over caller-provided memory (`MemoryRegion`), and helpers like `plane_layout_from_dims`/`plane_layout_with_stride`.
- `queue`: bounded, default-capacity bounded, and newest-value queues with backpressure-aware `SendOutcome`/`RecvOutcome`; non-blocking, waker-based async (`poll_recv`, `recv_async`, `send_async`, any executor) and, with `std`, blocking with timeouts.
- `format`: `FourCc`, `Resolution`, `MediaFormat`, `Interval`/`IntervalStepwise`, and `ColorSpace`.
- `controls`: `ControlId`, `ControlMeta`, `ControlValue`, and validation logic.
- `metrics`: hit/miss/allocation counters for pools and queues (`Metrics`), and a camera's counters (`Counters`: frames, drops, latency rings, 3A, stills; `styx_runtime::metrics` re-exports them).
- `sync`: the `Arc`, atomics and `Counter` the frame path uses on every target.
- `transform`: packed-frame rotations and mirrors.

## `no_std`
Without the default `std` feature (`default-features = false, features = ["neon", "x86"]`) the
crate is `no_std` + `alloc`, frame path included: formats, plane layouts, frame metadata,
controls, the SIMD kernels, `FrameLease` and `BufferPool` over heap memory, frames over static
or DMA memory (`MemoryRegion` with `RegionHooks` for cache maintenance and giving the buffer
back), shared views and companions, the bounded queues (non-blocking and waker-based),
transforms and metrics. Locks are spin locks; feature `critical-section` makes them critical
sections (for pools or queues touched from interrupt handlers) and builds the crate for
targets without compare-and-swap (Cortex-M0, RISC-V without `a`; the queues need it). Clocks
come from the platform (`buffer::set_platform_clock`). `std` adds memfd / dma-buf backings,
their export and import (unix), `SharedBufferPool` (Linux), blocking queue waits with
timeouts, OS clocks, parking_lot locks and run-time SIMD detection. See the repository's
`docs/portability.md`.

```rust
use styx_core::prelude::*;

static PIXELS: [u8; 16] = [7; 16];
let format = MediaFormat::new(FourCc::GREY, Resolution::new(4, 4).unwrap(), ColorSpace::Unknown);
let frame = FrameLease::from_region(
    FrameMeta::new(format, 0),
    smallvec::smallvec![plane_layout_from_dims(format.resolution.width, format.resolution.height, 1)],
    MemoryRegion::from_static(&PIXELS),
);
assert_eq!(frame.planes()[0].data()[0], 7);
```

## Zero-copy buffers and frames
Frames are built from pooled buffers to avoid churn:
```rust
use styx_core::prelude::*;

let pool = BufferPool::with_limits(4, 1 << 20, 8);
let res = Resolution::new(640, 480).unwrap();
let layout = plane_layout_from_dims(res.width, res.height, 3);
let meta = FrameMeta::new(MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb), 0);
let frame = FrameLease::single_plane(meta, pool.lease(), layout.len, layout.stride);
assert_eq!(frame.payload_bytes(), layout.len);
```

`FrameLease` exposes immutable/mutable plane views and returns buffers to the pool on drop. Use `planes_mut` for in-place writes; multi-plane layouts are supported via `FrameLease::multi_plane`.

## Bounded queues
`queue::bounded` provides a non-blocking bounded channel:
```rust
use styx_core::prelude::*;

let (tx, rx) = bounded::<FrameLease>(4);
let pool = BufferPool::with_limits(4, 1 << 20, 8);
let res = Resolution::new(2, 2).unwrap();
let layout = plane_layout_from_dims(res.width, res.height, 3);
let meta = FrameMeta::new(MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb), 0);
let frame = FrameLease::single_plane(meta, pool.lease(), layout.len, layout.stride);

match tx.send(frame) {
    SendOutcome::Ok => {},
    SendOutcome::Closed => {},
    SendOutcome::Full => { /* backpressure */ }
}

match rx.recv() {
    RecvOutcome::Data(frame) => { /* consume */ }
    RecvOutcome::Empty => { /* try again */ }
    RecvOutcome::Closed => {}
}
```

## Formats and controls
`MediaFormat` bundles FourCc, resolution, and color space; `Interval` describes frame pacing; `ControlMeta` + `ControlValue` carry backend controls with validation helpers.
