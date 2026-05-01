# styx-codec

Unified codec trait plus a registry for pluggable encoders/decoders. Includes MJPEG decoding and raw color converters; optional features enable FFmpeg and alternate JPEG implementations.

## Documentation
- <https://docs.rs/styx-codec>

## Install
```toml
[dependencies]
styx-codec = "2.0.0"
```

## Codec trait
```rust
use styx_codec::prelude::*;

struct Passthrough {
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

let codec = Passthrough {
    desc: CodecDescriptor {
        kind: CodecKind::Decoder,
        input: FourCc::new(*b"RG24"),
        output: FourCc::new(*b"RG24"),
        name: "passthrough",
        impl_name: "docs",
    },
};
assert_eq!(codec.descriptor().output.to_string(), "RG24");
```

`CodecDescriptor` describes the kind (encoder/decoder), input/output FourCc, algorithm family, and implementation name.

## Registry
`CodecRegistry` installs codecs and returns a `CodecRegistryHandle` for lookups:
```rust
use styx_codec::prelude::*;
use std::sync::Arc;

let registry = CodecRegistry::new();
let handle = registry.handle();
registry.register(FourCc::new(*b"RG24"), Arc::new(PassthroughDecoder::new(FourCc::new(*b"RG24"))));

let codec = handle.lookup(FourCc::new(*b"RG24"))?;
assert_eq!(codec.descriptor().output.to_string(), "RG24");
# Ok::<(), Box<dyn std::error::Error>>(())
```

Selection can be influenced via:
- `set_policy(CodecPolicy)`: ordered impls, priorities, and hardware bias per FourCc.
- `set_impl_priority` / `enable_only` / `disable_impl`: granular control over implementations.
- `lookup_preferred` / `process_preferred`: choose by ordered impl names and hardware bias.

`CodecStats` tracks processed/errors/backpressure counters via the handle.

## Built-in codecs
- Minimal no-feature build: codec traits, registry, `PassthroughDecoder`, and packed-frame helpers.
- `codec-jpeg-decoder`: MJPEG/JPEG decoder backed by `jpeg-decoder`.
- `raw-decoders`: raw color converters: YUYV/NV12/I420 -> RGB, RGBA/BGRA/BGR -> RGB, Bayer, mono, and related CPU conversions.
- Optional FFmpeg (`codec-ffmpeg`): H264/H265/MJPEG encoders/decoders.
- Optional JPEG (`codec-mozjpeg`, `codec-turbojpeg`, `codec-zune`): alternate MJPEG backends.
- Optional `dynamic-image` feature: compatibility helpers for `DynamicImage` conversions.

See `examples/03_codecs/mjpeg_decode.rs` for an end-to-end registry/decode usage example.
