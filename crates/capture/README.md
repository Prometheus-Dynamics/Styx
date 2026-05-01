# styx-capture

Capture descriptors/config validation and the `CaptureSource` trait for zero-copy frame producers. Includes helpers for building frames from buffer pools and a simple virtual backend for tests.

## Documentation
- <https://docs.rs/styx-capture>

## Install
```toml
[dependencies]
styx-capture = "2.0.0"
```

## Key types
- `Mode` / `ModeId`: advertised capture modes (FourCc + resolution + intervals).
- `CaptureDescriptor`: modes + control metadata for a device/source.
- `CaptureConfig`: user-selected mode/interval/controls with validation.
- `CaptureSource`: trait yielding `FrameLease` frames (sync-first). Use
  `try_next_frame` when callers need explicit `Data` / `Empty` / `Closed`
  semantics instead of legacy `Option` closure semantics.

## Building descriptors
`modes_from_formats` converts a list of `MediaFormat` values into modes, or build them manually:
```rust
use styx_capture::prelude::*;

let format = MediaFormat::new(
    FourCc::new(*b"RG24"),
    Resolution::new(640, 480).unwrap(),
    ColorSpace::Srgb,
);
let mode = Mode::new(format);
let descriptor = CaptureDescriptor::new([mode.clone()]);
let cfg = CaptureConfig { mode: mode.id.clone(), interval: None, controls: vec![] };
cfg.validate(&descriptor).unwrap();
```

## Virtual capture
`virtual_backend::VirtualCapture` emits patterned frames from a pool:
```rust
use styx_capture::prelude::*;

let pool = BufferPool::with_limits(4, 1 << 20, 8);
let mode = modes_from_formats([MediaFormat::new(
    FourCc::new(*b"RG24"),
    Resolution::new(320, 180).unwrap(),
    ColorSpace::Srgb,
)])[0].clone();
let cap = VirtualCapture::new(mode.clone(), pool, 3);
let RecvOutcome::Data(frame) = cap.try_next_frame() else {
    panic!("virtual source should produce a frame");
};
assert_eq!(frame.meta().format.code, FourCc::new(*b"RG24"));
```

## Frame helpers
`build_frame_from_pool` constructs a single-plane frame with the correct layout/stride for a given FourCc/resolution, returning a pooled `FrameLease` ready for downstream codecs or pipelines.
