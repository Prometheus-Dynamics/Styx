use std::sync::Arc;

use crate::core::prelude::{FrameLease, FrameResidency};
use daedalus::registry::capability::{NodeDecl, PortDecl};

/// Stable Daedalus transport type key for Styx frames.
pub const FRAMELEASE_TYPE_KEY: &str = "styx:framelease";

/// Register the stable Daedalus type identity for `FrameLease`.
///
/// Call this before registering Styx media nodes or building graphs that use
/// `FrameLease` ports. It installs a Daedalus type-expression override so node
/// macros and host payloads agree on `styx:framelease` instead of falling back
/// to a Rust type-name-derived key.
pub fn register_framelease_type() {
    daedalus::data::typing::register_type::<FrameLease>(daedalus::data::model::TypeExpr::opaque(
        FRAMELEASE_TYPE_KEY,
    ));
}

/// Return the canonical Daedalus transport key used for `FrameLease` payloads.
pub fn framelease_type_key() -> daedalus::transport::TypeKey {
    daedalus::transport::TypeKey::new(FRAMELEASE_TYPE_KEY)
}

/// Map Styx frame residency to Daedalus's transport residency vocabulary.
pub fn framelease_daedalus_residency(frame: &FrameLease) -> daedalus::transport::Residency {
    match frame.residency() {
        FrameResidency::HostOwned | FrameResidency::CompressedPacket => {
            daedalus::transport::Residency::Cpu
        }
        FrameResidency::HostExternal | FrameResidency::Dmabuf => {
            daedalus::transport::Residency::External
        }
        FrameResidency::GpuTexture => daedalus::transport::Residency::Gpu,
    }
}

/// Wrap a `FrameLease` as a Daedalus payload without copying frame planes.
pub fn framelease_payload(frame: FrameLease) -> daedalus::transport::Payload {
    register_framelease_type();
    let residency = framelease_daedalus_residency(&frame);
    let bytes = Some(frame.payload_bytes() as u64);
    daedalus::transport::Payload::shared_with(
        framelease_type_key(),
        Arc::new(frame),
        residency,
        None,
        bytes,
    )
}

pub(super) fn framelease_node_decl(id: &str, label: &'static str) -> NodeDecl {
    let type_key = framelease_type_key();
    let schema = daedalus::data::model::TypeExpr::opaque(FRAMELEASE_TYPE_KEY);
    NodeDecl::new(id)
        .label(label)
        .input(PortDecl::new("frame", type_key.clone()).schema(schema.clone()))
        .output(PortDecl::new("frame", type_key).schema(schema))
}

pub(super) fn framelease_source_node_decl(id: &str, label: &'static str) -> NodeDecl {
    let type_key = framelease_type_key();
    let schema = daedalus::data::model::TypeExpr::opaque(FRAMELEASE_TYPE_KEY);
    NodeDecl::new(id)
        .label(label)
        .output(PortDecl::new("frame", type_key).schema(schema))
}
