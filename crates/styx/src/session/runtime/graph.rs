use styx_core::prelude::*;

use crate::service::SharedStyxServiceRuntime;

pub(super) struct GraphProcessError {
    pub(super) stage: crate::metrics::PipelineStage,
    pub(super) component: String,
    pub(super) message: String,
}

pub(crate) struct GraphMediaRuntime {
    graph: daedalus::engine::HostGraph<daedalus::runtime::handler_registry::HandlerRegistry>,
    last_telemetry: Option<daedalus::runtime::ExecutionTelemetry>,
    frame_path_enabled: bool,
    service_runtime: Option<SharedStyxServiceRuntime>,
}

impl GraphMediaRuntime {
    pub(crate) fn new(
        graph: daedalus::engine::HostGraph<daedalus::runtime::handler_registry::HandlerRegistry>,
        frame_path_enabled: bool,
        service_runtime: Option<SharedStyxServiceRuntime>,
    ) -> Self {
        Self {
            graph,
            last_telemetry: None,
            frame_path_enabled,
            service_runtime,
        }
    }

    pub(super) fn last_telemetry(&self) -> Option<&daedalus::runtime::ExecutionTelemetry> {
        self.last_telemetry.as_ref()
    }

    pub(super) fn process(&mut self, frame: FrameLease) -> Result<FrameLease, GraphProcessError> {
        if !self.frame_path_enabled {
            return Ok(frame);
        }
        match self
            .graph
            .push_payload("frame", crate::graph::framelease_payload(frame))
        {
            daedalus::transport::FeedOutcome::Accepted { .. }
            | daedalus::transport::FeedOutcome::Replaced { .. } => {}
            other => {
                return Err(graph_error(format!(
                    "graph input rejected frame: {other:?}"
                )));
            }
        }
        let span = tracing::trace_span!("graph_tick_until_idle", path = "frame");
        let _enter = span.enter();
        let telemetry = self
            .graph
            .tick_until_idle()
            .map_err(graph_engine_error)?
            .ok_or_else(|| graph_error("graph produced no telemetry"))?;
        self.last_telemetry = Some(telemetry);
        let mut output = self
            .graph
            .drain_owned::<FrameLease>("frame")
            .map_err(|err| graph_error(err.to_string()))?;
        output
            .pop()
            .ok_or_else(|| graph_error("graph produced no output frame"))
    }

    pub(super) fn submit_control_event(
        &mut self,
        event: crate::graph::StyxControlEvent,
    ) -> Result<crate::graph::StyxControlResult, String> {
        match self.graph.push_payload(
            "control",
            crate::graph::control_event_payload(event.clone()),
        ) {
            daedalus::transport::FeedOutcome::Accepted { .. }
            | daedalus::transport::FeedOutcome::Replaced { .. } => {}
            other => return Err(format!("graph control input rejected event: {other:?}")),
        }
        let span = tracing::trace_span!("graph_tick_until_idle", path = "control");
        let _enter = span.enter();
        let telemetry = self
            .graph
            .tick_until_idle()
            .map_err(|err| err.to_string())?
            .ok_or_else(|| "graph produced no telemetry for control event".to_string())?;
        self.last_telemetry = Some(telemetry);
        let mut output = self
            .graph
            .drain_owned::<crate::graph::StyxControlResult>("control_result")
            .map_err(|err| err.to_string())?;
        let result = output
            .pop()
            .ok_or_else(|| "graph produced no control result".to_string())?;
        if let Some(service) = &self.service_runtime
            && let Ok(mut service) = service.lock()
        {
            service.record_control_result(result.clone());
        }
        Ok(result)
    }
}

fn graph_error(message: impl Into<String>) -> GraphProcessError {
    GraphProcessError {
        stage: crate::metrics::PipelineStage::Graph,
        component: "graph_pipeline".to_string(),
        message: message.into(),
    }
}

fn graph_engine_error(err: daedalus::engine::EngineError) -> GraphProcessError {
    match err {
        daedalus::engine::EngineError::Runtime(err) => graph_execute_error(err),
        err => graph_error(err.to_string()),
    }
}

fn graph_execute_error(err: daedalus::runtime::ExecuteError) -> GraphProcessError {
    match err {
        daedalus::runtime::ExecuteError::HandlerFailed { node, error } => {
            let message = error.to_string();
            if node.starts_with("styx.codec.decoder.") {
                return codec_graph_error(crate::metrics::PipelineStage::Decode, node, message);
            }
            if node.starts_with("styx.codec.encoder.") {
                return codec_graph_error(crate::metrics::PipelineStage::Encode, node, message);
            }
            graph_error(format!("handler failed on node {node}: {message}"))
        }
        err => graph_error(err.to_string()),
    }
}

fn codec_graph_error(
    stage: crate::metrics::PipelineStage,
    node: String,
    message: String,
) -> GraphProcessError {
    let stage_name = match stage {
        crate::metrics::PipelineStage::Decode => "decoder",
        crate::metrics::PipelineStage::Encode => "encoder",
        _ => "codec",
    };
    let prefix = format!("{stage_name} ");
    let (component, message) = message
        .strip_prefix(&prefix)
        .and_then(|rest| rest.split_once(" failed: "))
        .map(|(component, reason)| (component.to_string(), reason.to_string()))
        .unwrap_or_else(|| (node, message));

    GraphProcessError {
        stage,
        component,
        message,
    }
}

pub(super) fn summarize_graph_telemetry(
    telemetry: &daedalus::runtime::ExecutionTelemetry,
) -> crate::metrics::GraphTelemetryStats {
    let mut stats = crate::metrics::GraphTelemetryStats {
        nodes_executed: telemetry.nodes_executed as u64,
        graph_duration_ns: telemetry.graph_duration.as_nanos() as u64,
        unattributed_runtime_duration_ns: telemetry.unattributed_runtime_duration.as_nanos() as u64,
        backpressure_events: telemetry.backpressure_events as u64,
        ..Default::default()
    };
    for node in telemetry.node_metrics.values() {
        stats.node_total_duration_ns = stats
            .node_total_duration_ns
            .saturating_add(node.total_duration.as_nanos() as u64);
        stats.node_handler_duration_ns = stats
            .node_handler_duration_ns
            .saturating_add(node.handler_duration.as_nanos() as u64);
        stats.node_cpu_duration_ns = stats
            .node_cpu_duration_ns
            .saturating_add(node.cpu_duration.as_nanos() as u64);
    }
    for edge in telemetry.edge_metrics.values() {
        stats.edge_wait_duration_ns = stats
            .edge_wait_duration_ns
            .saturating_add(edge.total_wait.as_nanos() as u64);
        stats.edge_transport_apply_duration_ns = stats
            .edge_transport_apply_duration_ns
            .saturating_add(edge.transport_apply_duration.as_nanos() as u64);
        stats.edge_adapter_duration_ns = stats
            .edge_adapter_duration_ns
            .saturating_add(edge.adapter_duration.as_nanos() as u64);
        stats.copied_bytes = stats.copied_bytes.saturating_add(edge.copied_bytes);
        stats.transport_bytes = stats.transport_bytes.saturating_add(edge.transport_bytes);
        stats.transport_count = stats.transport_count.saturating_add(edge.transport_count);
        stats.payload_clones = stats
            .payload_clones
            .saturating_add(edge.payload_clone_count);
        stats.unique_handoffs = stats.unique_handoffs.saturating_add(edge.unique_handoffs);
        stats.shared_handoffs = stats.shared_handoffs.saturating_add(edge.shared_handoffs);
        stats.pressure_events = stats
            .pressure_events
            .saturating_add(edge.pressure_events.total);
        stats.current_queue_bytes = stats
            .current_queue_bytes
            .saturating_add(edge.current_queue_bytes);
        stats.peak_queue_bytes = stats.peak_queue_bytes.saturating_add(edge.peak_queue_bytes);
        stats.bounded_queue_capacity = stats
            .bounded_queue_capacity
            .saturating_add(edge.capacity.unwrap_or(0));
        stats.drops = stats.drops.saturating_add(edge.drops);
        stats.latest_replacements = stats
            .latest_replacements
            .saturating_add(edge.pressure_events.latest_replace);
        stats.adapter_count = stats.adapter_count.saturating_add(edge.adapter_count);
        stats.adapter_errors = stats.adapter_errors.saturating_add(edge.adapter_errors);
    }
    stats
}
