use std::sync::Arc;

use crate::capture_api::{
    CameraRequest, CaptureError, CaptureHandle, CaptureRequest, CaptureStartPolicy,
};
use daedalus::NodeHandle;
use daedalus::runtime::plugins::{PluginError, PluginResult};

use super::frame::framelease_source_node_decl;
use super::{StyxCaptureSourceOptions, framelease_payload, register_framelease_type};

/// Register a Daedalus source node backed by an already-started Styx capture handle.
pub fn register_capture_source_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    capture: CaptureHandle,
) -> PluginResult<NodeHandle> {
    register_capture_source_node_with_options(
        registry,
        node_id,
        capture,
        StyxCaptureSourceOptions::default(),
    )
}

/// Register a Daedalus source node backed by an already-started Styx capture handle.
///
/// The node emits `FrameLease` payloads on its `frame` output and does not copy
/// frame planes. `frames_per_tick` controls batching per scheduler tick. Source
/// handlers poll capture queues without blocking the graph scheduler; callers
/// that need waiting should drive capture outside the scheduler or tick again.
pub fn register_capture_source_node_with_options(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    capture: CaptureHandle,
    options: StyxCaptureSourceOptions,
) -> PluginResult<NodeHandle> {
    register_shared_capture_source_node_with_options(registry, node_id, Arc::new(capture), options)
}

pub(crate) fn register_shared_capture_source_node_with_options(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    capture: Arc<CaptureHandle>,
    options: StyxCaptureSourceOptions,
) -> PluginResult<NodeHandle> {
    register_framelease_type();
    let node_id = node_id.into();
    registry.register_node_decl(framelease_source_node_decl(&node_id, "Styx capture source"))?;
    registry
        .handlers
        .try_on_stateful(&node_id, move |_node, _ctx, io| {
            if !options.wait.is_zero() {
                tracing::trace!(
                    wait_ms = options.wait.as_millis() as u64,
                    "capture source wait is handled outside the graph scheduler"
                );
            }
            let frames_per_tick = options.frames_per_tick.max(1);
            for _ in 0..frames_per_tick {
                match capture.recv() {
                    styx_core::prelude::RecvOutcome::Data(frame) => {
                        io.push_payload("frame", framelease_payload(frame));
                    }
                    styx_core::prelude::RecvOutcome::Empty => break,
                    styx_core::prelude::RecvOutcome::Closed => break,
                }
            }
            Ok(())
        })
        .map_err(|_| "capture source handler register failed")?;
    Ok(NodeHandle::new(node_id))
}

/// Start a Styx capture request and register it as a Daedalus source node.
pub fn register_capture_request_source_with_policy(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    request: CaptureRequest<'_>,
    policy: CaptureStartPolicy,
    options: StyxCaptureSourceOptions,
) -> Result<NodeHandle, CaptureError> {
    let capture = request.start_with_policy(policy)?;
    register_capture_source_node_with_options(registry, node_id, capture, options)
        .map_err(graph_register_error)
}

/// Start up to `count` cameras matching a camera request and register one source node per camera.
pub fn register_camera_sources_limit(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id_prefix: impl AsRef<str>,
    request: CameraRequest,
    count: usize,
) -> Result<Vec<NodeHandle>, CaptureError> {
    register_camera_sources_with_policy(
        registry,
        node_id_prefix,
        request,
        Some(count),
        CaptureStartPolicy::default(),
        StyxCaptureSourceOptions::default(),
    )
}

/// Start every camera matching a camera request and register one source node per camera.
pub fn register_camera_sources_all(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id_prefix: impl AsRef<str>,
    request: CameraRequest,
) -> Result<Vec<NodeHandle>, CaptureError> {
    register_camera_sources_with_policy(
        registry,
        node_id_prefix,
        request,
        None,
        CaptureStartPolicy::default(),
        StyxCaptureSourceOptions::default(),
    )
}

/// Start matching cameras and register one Daedalus source node per camera.
///
/// Pass `Some(count)` to cap the number of cameras, or `None` to register all
/// matching cameras. Node ids are generated as `{node_id_prefix}.{index}`.
pub fn register_camera_sources_with_policy(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id_prefix: impl AsRef<str>,
    request: CameraRequest,
    count: Option<usize>,
    policy: CaptureStartPolicy,
    options: StyxCaptureSourceOptions,
) -> Result<Vec<NodeHandle>, CaptureError> {
    let captures = match count {
        Some(count) => request.start_many_with_policy(count, policy)?,
        None => request.start_all_with_policy(policy)?,
    };
    let mut nodes = Vec::with_capacity(captures.len());
    for (index, capture) in captures.into_iter().enumerate() {
        let node_id = format!("{}.{}", node_id_prefix.as_ref(), index);
        let node = register_capture_source_node_with_options(registry, node_id, capture, options)
            .map_err(graph_register_error)?;
        nodes.push(node);
    }
    Ok(nodes)
}

fn graph_register_error(err: PluginError) -> CaptureError {
    CaptureError::Backend(format!("graph install failed: {err}"))
}
