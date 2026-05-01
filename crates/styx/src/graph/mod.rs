//! Daedalus-backed graph integration for Styx media pipelines.
//!
//! This module is intentionally an adapter layer. Styx owns capture, codec,
//! `FrameLease`, and backing/import/export semantics; Daedalus owns graph
//! scheduling, fanout, edge policy, host bridging, and graph telemetry.

pub use daedalus;

use std::time::Duration;

mod codec_nodes;
mod control;
mod frame;
mod plugin;
mod policy;
mod runtime_nodes;
mod sinks;
mod sources;
#[cfg(test)]
mod test_support;

pub use codec_nodes::{StyxCodecNodeDescriptor, StyxCodecNodeOptions, concrete_codec_node_id};
#[cfg(feature = "graph-pipeline")]
pub(crate) use control::register_capture_control_node;
pub use control::{
    CONTROL_EVENT_TYPE_KEY, CONTROL_RESULT_TYPE_KEY, StyxControlEvent, StyxControlResult,
    control_event_payload, control_event_type_key, control_result_type_key, register_control_types,
};
use frame::framelease_node_decl;
pub use frame::{
    FRAMELEASE_TYPE_KEY, framelease_daedalus_residency, framelease_payload, framelease_type_key,
    register_framelease_type,
};
pub use plugin::{StyxMediaPlugin, StyxSinkDescriptor, StyxSourceDescriptor, StyxSourceKind};
pub use policy::{analysis_policy, preview_policy, recording_policy};
pub use sinks::{
    FrameSinkCell, NetworkStreamSinkOptions, NetworkStreamWriter, register_analysis_sink_node,
    register_network_stream_sink_node, register_preview_sink_node,
};
#[cfg(feature = "hooks")]
pub use sinks::{register_file_sequence_sink_node, register_recorder_sink_node};
pub use sources::{
    register_camera_sources_all, register_camera_sources_limit,
    register_camera_sources_with_policy, register_capture_request_source_with_policy,
    register_capture_source_node, register_capture_source_node_with_options,
};

use crate::capabilities::StyxCapabilityInventory;
use daedalus::NodeHandle;
use daedalus::registry::capability::NodeDecl;

/// Options for graph source nodes backed by Styx capture handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyxCaptureSourceOptions {
    /// Maximum time to wait for one frame during a source node tick.
    pub wait: Duration,
    /// Maximum frames to emit from one source node tick.
    pub frames_per_tick: usize,
}

impl StyxCaptureSourceOptions {
    /// Non-blocking source tick that emits at most one frame.
    pub const fn poll_one() -> Self {
        Self {
            wait: Duration::ZERO,
            frames_per_tick: 1,
        }
    }

    /// Source tick preference for callers that drive waiting outside the graph scheduler.
    ///
    /// Graph source handlers themselves remain nonblocking so one slow capture
    /// source cannot stall unrelated graph nodes.
    pub const fn wait_one(wait: Duration) -> Self {
        Self {
            wait,
            frames_per_tick: 1,
        }
    }
}

impl Default for StyxCaptureSourceOptions {
    fn default() -> Self {
        Self::poll_one()
    }
}

/// Register planner-visible Styx media capability metadata with Daedalus.
///
/// This does not create a scheduler in Styx. It publishes a stable external
/// capability node so Daedalus tooling can see capture, codec, transform, and
/// backing support alongside the executable Styx media nodes.
fn register_styx_capabilities(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    inventory: &StyxCapabilityInventory,
) -> daedalus::runtime::plugins::PluginResult<NodeHandle> {
    let mut decl = NodeDecl::new("styx.capabilities.media")
        .label("Styx media capabilities")
        .external();
    decl.metadata_json.insert(
        "styx.capture_backends".into(),
        inventory.capture_backends.len().to_string(),
    );
    decl.metadata_json
        .insert("styx.codecs".into(), inventory.codecs.len().to_string());
    decl.metadata_json.insert(
        "styx.transforms".into(),
        inventory.transforms.len().to_string(),
    );
    decl.metadata_json
        .insert("styx.backing".into(), inventory.backing.len().to_string());
    let formats = inventory
        .capture_backends
        .iter()
        .flat_map(|cap| cap.formats.iter())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",");
    decl.metadata_json
        .insert("styx.capture_formats".into(), formats);
    registry.register_node_decl(decl)?;
    Ok(NodeHandle::new("styx.capabilities.media"))
}

#[cfg(test)]
mod tests;
