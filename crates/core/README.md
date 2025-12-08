# styx-core

Core primitives shared across the stack: buffer pools, frame leases/layouts, bounded queues, FourCc/format types, controls, and lightweight metrics.

## Documentation
- <https://docs.rs/styx-core-rs>

## Install
```toml
[dependencies]
styx-core-rs = "0.1.0"
```

## Modules
- `buffer`: `BufferPool`, `BufferLease`, `FrameLease`, plane views, and helpers like `plane_layout_from_dims`/`plane_layout_with_stride`.
- `queue`: bounded/newest/unbounded queues with backpressure-aware `SendOutcome`/`RecvOutcome`.
- `format`: `FourCc`, `Resolution`, `MediaFormat`, `Interval`/`IntervalStepwise`, and `ColorSpace`.
- `controls`: `ControlId`, `ControlMeta`, `ControlValue`, and validation logic.
- `metrics`: simple hit/miss/allocation counters for pools.

## Zero-copy buffers and frames
Frames are built from pooled buffers to avoid churn:
```rust,ignore
use styx_core::prelude::*;

let pool = BufferPool::with_limits(4, 1 << 20, 8);
let res = Resolution::new(640, 480).unwrap();
let layout = plane_layout_from_dims(res.width, res.height, 3);
let meta = FrameMeta::new(MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb), 0);
let frame = FrameLease::single_plane(meta, pool.lease(), layout.len, layout.stride);
```

`FrameLease` exposes immutable/mutable plane views and returns buffers to the pool on drop. Use `planes_mut` for in-place writes; multi-plane layouts are supported via `FrameLease::multi_plane`.

## Bounded queues
`queue::bounded` provides a non-blocking bounded channel:
```rust,ignore
use styx_core::prelude::*;

let (tx, rx) = bounded::<FrameLease>(4);
match tx.send(frame) {
    SendOutcome::Sent => {},
    SendOutcome::Closed => {},
    SendOutcome::Full(f) => { /* backpressure */ }
}

match rx.recv() {
    RecvOutcome::Data(frame) => { /* consume */ }
    RecvOutcome::Empty => { /* try again */ }
    RecvOutcome::Closed => {}
}
```

## Formats and controls
`MediaFormat` bundles FourCc, resolution, and color space; `Interval` describes frame pacing; `ControlMeta` + `ControlValue` carry backend controls with validation helpers.
