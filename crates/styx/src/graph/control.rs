use crate::capture_api::CaptureError;
#[cfg(feature = "graph-pipeline")]
use crate::capture_api::ControlPlane;
use crate::core::prelude::{ControlId, ControlValue};
#[cfg(feature = "graph-pipeline")]
use daedalus::NodeHandle;
#[cfg(feature = "graph-pipeline")]
use daedalus::registry::capability::{NodeDecl, PortDecl};
#[cfg(feature = "graph-pipeline")]
use daedalus::runtime::plugins::PluginResult;

/// Stable Daedalus transport type key for Styx capture control events.
pub const CONTROL_EVENT_TYPE_KEY: &str = "styx:control-event";
/// Stable Daedalus transport type key for Styx capture control results.
pub const CONTROL_RESULT_TYPE_KEY: &str = "styx:control-result";

/// Capture control command that can be routed through a Daedalus graph.
#[derive(Clone, Debug, PartialEq)]
pub enum StyxControlEvent {
    Set { id: ControlId, value: ControlValue },
    Get { id: ControlId },
}

/// Result emitted by the Styx graph control node.
#[derive(Clone, Debug, PartialEq)]
pub struct StyxControlResult {
    pub event: StyxControlEvent,
    pub value: Option<ControlValue>,
    pub error: Option<String>,
}

impl StyxControlResult {
    pub fn success(event: StyxControlEvent, value: Option<ControlValue>) -> Self {
        Self {
            event,
            value,
            error: None,
        }
    }

    pub fn failure(event: StyxControlEvent, error: CaptureError) -> Self {
        Self {
            event,
            value: None,
            error: Some(error.to_string()),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

pub fn register_control_types() {
    daedalus::data::typing::register_type::<StyxControlEvent>(
        daedalus::data::model::TypeExpr::opaque(CONTROL_EVENT_TYPE_KEY),
    );
    daedalus::data::typing::register_type::<StyxControlResult>(
        daedalus::data::model::TypeExpr::opaque(CONTROL_RESULT_TYPE_KEY),
    );
}

pub fn control_event_type_key() -> daedalus::transport::TypeKey {
    daedalus::transport::TypeKey::new(CONTROL_EVENT_TYPE_KEY)
}

pub fn control_result_type_key() -> daedalus::transport::TypeKey {
    daedalus::transport::TypeKey::new(CONTROL_RESULT_TYPE_KEY)
}

pub fn control_event_payload(event: StyxControlEvent) -> daedalus::transport::Payload {
    register_control_types();
    daedalus::transport::Payload::owned(control_event_type_key(), event)
}

#[cfg(feature = "graph-pipeline")]
fn control_result_payload(result: StyxControlResult) -> daedalus::transport::Payload {
    register_control_types();
    daedalus::transport::Payload::owned(control_result_type_key(), result)
}

#[cfg(feature = "graph-pipeline")]
pub(super) fn control_node_decl(id: &str, label: &'static str) -> NodeDecl {
    register_control_types();
    let event_schema = daedalus::data::model::TypeExpr::opaque(CONTROL_EVENT_TYPE_KEY);
    let result_schema = daedalus::data::model::TypeExpr::opaque(CONTROL_RESULT_TYPE_KEY);
    NodeDecl::new(id)
        .label(label)
        .input(PortDecl::new("control", control_event_type_key()).schema(event_schema))
        .output(PortDecl::new("control_result", control_result_type_key()).schema(result_schema))
}

#[cfg(feature = "graph-pipeline")]
pub(crate) fn register_capture_control_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    control: ControlPlane,
) -> PluginResult<NodeHandle> {
    register_control_types();
    let node_id = node_id.into();
    registry.register_node_decl(control_node_decl(&node_id, "Styx capture control"))?;
    registry
        .handlers
        .try_on_stateful(&node_id, move |_node, _ctx, io| {
            let Some(event) = io.take_owned::<StyxControlEvent>("control") else {
                return Ok(());
            };
            let result = match event.clone() {
                StyxControlEvent::Set { id, value } => {
                    match crate::capture_api::apply_control_to_plane(&control, id, value) {
                        Ok(()) => StyxControlResult::success(event, None),
                        Err(err) => StyxControlResult::failure(event, err),
                    }
                }
                StyxControlEvent::Get { id } => {
                    match crate::capture_api::read_control_from_plane(&control, id) {
                        Ok(value) => StyxControlResult::success(event, Some(value)),
                        Err(err) => StyxControlResult::failure(event, err),
                    }
                }
            };
            io.push_payload("control_result", control_result_payload(result));
            Ok(())
        })
        .map_err(|_| "capture control node handler register failed")?;
    Ok(NodeHandle::new(node_id))
}
