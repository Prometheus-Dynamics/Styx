use std::sync::Arc;

use styx_codec::prelude::*;

/// A decoder or an encoder for input `fourcc`. The kind matters: a raw format can have both
/// (YUYV → RGB decoders, YUYV → MJPEG/H.264 encoders).
pub(crate) fn lookup_codec(
    registry: &CodecRegistryHandle,
    kind: CodecKind,
    fourcc: FourCc,
    impl_name: Option<&str>,
    prefer_hardware: bool,
) -> Result<Arc<dyn Codec>, RegistryError> {
    if let Some(name) = impl_name {
        registry.lookup_named_kind(fourcc, kind, CodecImplementationId::new(name))
    } else if prefer_hardware {
        registry.lookup_preferred_kind(fourcc, kind, true)
    } else {
        registry.lookup_auto_kind(fourcc, kind)
    }
}
