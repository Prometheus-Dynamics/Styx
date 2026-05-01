use crate::core::prelude::FrameLease;
use daedalus::NodeHandle;
use daedalus::runtime::NodeError;
use daedalus::runtime::plugins::PluginResult;

use super::{framelease_node_decl, framelease_payload, register_framelease_type};

pub(super) use super::runtime_nodes::register_transform_node;

pub(super) fn register_materialize_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
) -> PluginResult<NodeHandle> {
    register_framelease_type();
    let node_id = node_id.into();
    registry.register_node_decl(framelease_node_decl(&node_id, "Styx materialize"))?;
    registry
        .handlers
        .try_on(&node_id, move |_node, _ctx, io| {
            let frame = io.take_owned::<FrameLease>("frame").ok_or_else(|| {
                NodeError::InvalidInput("missing FrameLease input 'frame'".into())
            })?;
            io.push_payload("frame", framelease_payload(frame.materialize_owned()));
            Ok(())
        })
        .map_err(|_| "materialize node handler register failed")?;
    Ok(NodeHandle::new(node_id))
}

pub(super) fn register_preview_node<F>(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    preview: F,
) -> PluginResult<NodeHandle>
where
    F: FnMut(&FrameLease) + Send + 'static,
{
    register_frame_tap_node(registry, node_id, "Styx preview", preview)
}

pub(super) fn register_analysis_node<F>(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    analysis: F,
) -> PluginResult<NodeHandle>
where
    F: FnMut(&FrameLease) + Send + 'static,
{
    register_frame_tap_node(registry, node_id, "Styx analysis", analysis)
}

fn register_frame_tap_node<F>(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    label: &'static str,
    mut tap: F,
) -> PluginResult<NodeHandle>
where
    F: FnMut(&FrameLease) + Send + 'static,
{
    register_framelease_type();
    let node_id = node_id.into();
    registry.register_node_decl(framelease_node_decl(&node_id, label))?;
    registry
        .handlers
        .try_on_stateful(&node_id, move |_node, _ctx, io| {
            let frame = io.take_owned::<FrameLease>("frame").ok_or_else(|| {
                NodeError::InvalidInput("missing FrameLease input 'frame'".into())
            })?;
            tap(&frame);
            io.push_payload("frame", framelease_payload(frame));
            Ok(())
        })
        .map_err(|_| "frame tap node handler register failed")?;
    Ok(NodeHandle::new(node_id))
}
