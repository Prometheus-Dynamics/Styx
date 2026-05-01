use std::sync::Arc;

use styx_codec::prelude::*;

pub(crate) fn lookup_codec(
    registry: &CodecRegistryHandle,
    fourcc: FourCc,
    impl_name: Option<&str>,
    prefer_hardware: bool,
) -> Result<Arc<dyn Codec>, RegistryError> {
    if let Some(name) = impl_name {
        registry.lookup_named(fourcc, name)
    } else if prefer_hardware {
        registry.lookup_preferred(fourcc, &[], true)
    } else {
        registry.lookup_auto(fourcc)
    }
}
