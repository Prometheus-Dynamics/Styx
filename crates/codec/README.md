# styx-codec

Unified codec trait plus a registry for pluggable encoders/decoders. Includes MJPEG decoding and raw color converters; optional features enable FFmpeg and alternate JPEG implementations.

## Documentation
- <https://docs.rs/styx-codec>

## Install
```toml
[dependencies]
styx-codec = "0.1.0"
```

## Codec trait
```rust,ignore
use styx_codec::prelude::*;

pub struct Passthrough {
    desc: CodecDescriptor,
}

impl Codec for Passthrough {
    fn descriptor(&self) -> &CodecDescriptor { &self.desc }
    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        if input.meta().format.code != self.desc.input {
            return Err(CodecError::FormatMismatch { expected: self.desc.input, actual: input.meta().format.code });
        }
        Ok(input)
    }
}
```

`CodecDescriptor` describes the kind (encoder/decoder), input/output FourCc, algorithm family, and implementation name.

## Registry
`CodecRegistry` installs codecs and returns a `CodecRegistryHandle` for lookups:
```rust,ignore
use styx_codec::prelude::*;
use std::sync::Arc;

let registry = CodecRegistry::new();
let handle = registry.handle();
registry.register(FourCc::new(*b"MJPG"), Arc::new(MjpegDecoder::new(FourCc::new(*b"RG24"))));

let frame = /* FrameLease carrying MJPG data */;
let decoded = handle.process(FourCc::new(*b"MJPG"), frame)?;
```

Selection can be influenced via:
- `set_policy(CodecPolicy)`: ordered impls, priorities, and hardware bias per FourCc.
- `set_impl_priority` / `enable_only` / `disable_impl`: granular control over implementations.
- `lookup_preferred` / `process_preferred`: choose by ordered impl names and hardware bias.

`CodecStats` tracks processed/errors/backpressure counters via the handle.

## Built-in codecs
- MJPEG decoder (default feature set).
- Raw color converters: YUYV/NV12/I420 → RGB, RGBA/BGRA/BGR → RGB, passthrough.
- Optional FFmpeg (`codec-ffmpeg`): H264/H265/MJPEG encoders/decoders.
- Optional JPEG (`codec-mozjpeg`, `codec-turbojpeg`, `codec-zune`): alternate MJPEG backends.
- Optional `image` feature: `ImageAnyDecoder` and helpers for `DynamicImage` conversions.

See `crates/styx/examples/mjpeg_decode.rs` for an end-to-end registry/decode usage example.
