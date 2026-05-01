#[cfg(any(test, feature = "hooks"))]
use crate::core::prelude::{FrameLease, FrameTransform, transform_packed_frame};
#[cfg(any(test, feature = "hooks"))]
use daedalus::NodeHandle;
#[cfg(any(test, feature = "hooks"))]
use daedalus::runtime::NodeError;
#[cfg(any(test, feature = "hooks"))]
use daedalus::runtime::plugins::PluginResult;

#[cfg(any(test, feature = "hooks"))]
use super::{framelease_node_decl, framelease_payload, register_framelease_type};

/// Register a stateful frame hook node that transforms a `FrameLease`.
#[cfg(feature = "hooks")]
pub(super) fn register_frame_hook_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    hook: super::plugin::FrameHookCell,
) -> PluginResult<NodeHandle> {
    register_framelease_type();
    let node_id = node_id.into();
    registry.register_node_decl(framelease_node_decl(&node_id, "Styx frame hook"))?;
    registry
        .handlers
        .try_on_stateful(&node_id, move |_node, _ctx, io| {
            let Some(frame) = io.take_owned::<FrameLease>("frame") else {
                return Ok(());
            };
            let mut hook = hook
                .lock()
                .map_err(|_| NodeError::Handler("frame hook lock poisoned".into()))?;
            let out = hook(frame);
            io.push_payload("frame", framelease_payload(out));
            Ok(())
        })
        .map_err(|_| "frame hook node handler register failed")?;
    Ok(NodeHandle::new(node_id))
}

/// Register a Daedalus node that applies an existing Styx packed-frame transform.
#[cfg(any(test, feature = "hooks"))]
pub(super) fn register_transform_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    transform: FrameTransform,
) -> PluginResult<NodeHandle> {
    register_framelease_type();
    let node_id = node_id.into();
    registry.register_node_decl(framelease_node_decl(&node_id, "Styx transform"))?;
    registry
        .handlers
        .try_on(&node_id, move |_node, _ctx, io| {
            let Some(frame) = io.take_owned::<FrameLease>("frame") else {
                return Ok(());
            };
            let out = if transform.is_identity() {
                frame
            } else {
                transform_packed_frame(&frame, transform)
                    .map_err(|err| NodeError::Handler(err.to_string()))?
            };
            io.push_payload("frame", framelease_payload(out));
            Ok(())
        })
        .map_err(|_| "transform node handler register failed")?;
    Ok(NodeHandle::new(node_id))
}
