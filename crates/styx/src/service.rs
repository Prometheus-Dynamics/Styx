use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::metrics::{HealthReport, PipelineStageError};
use crate::watch::{DeviceWatcher, InventoryEvent, WatchError, WatchRefreshReport, WatchRuntime};

/// Shared service runtime handle used by graph and pipeline components that need to publish events.
pub type SharedStyxServiceRuntime = Arc<Mutex<StyxServiceRuntime>>;

/// Event emitted by the facade service runtime.
#[derive(Debug, Clone)]
pub enum StyxServiceEvent {
    /// Device inventory event produced by a watch refresh.
    Device(InventoryEvent),
    /// Pipeline or service health snapshot.
    Health(Box<HealthReport>),
    /// Pipeline worker lifecycle change.
    Pipeline(PipelineWorkerEvent),
    /// Sink lifecycle change.
    Sink(SinkLifecycleEvent),
    /// Recording lifecycle change.
    Recording(RecordingLifecycleEvent),
    /// Graph control response routed through the service event stream.
    #[cfg(feature = "daedalus-plugin")]
    Control(crate::graph::StyxControlResult),
}

impl StyxServiceEvent {
    /// Stable category identifier for filtering service events without matching display strings.
    pub fn kind(&self) -> StyxServiceEventKind {
        match self {
            Self::Device(_) => StyxServiceEventKind::Device,
            Self::Health(_) => StyxServiceEventKind::Health,
            Self::Pipeline(_) => StyxServiceEventKind::Pipeline,
            Self::Sink(_) => StyxServiceEventKind::Sink,
            Self::Recording(_) => StyxServiceEventKind::Recording,
            #[cfg(feature = "daedalus-plugin")]
            Self::Control(_) => StyxServiceEventKind::Control,
        }
    }
}

/// Stable service event category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StyxServiceEventKind {
    Device,
    Health,
    Pipeline,
    Sink,
    Recording,
    #[cfg(feature = "daedalus-plugin")]
    Control,
}

impl StyxServiceEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Health => "health",
            Self::Pipeline => "pipeline",
            Self::Sink => "sink",
            Self::Recording => "recording",
            #[cfg(feature = "daedalus-plugin")]
            Self::Control => "control",
        }
    }
}

impl std::fmt::Display for StyxServiceEventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle event for a worker driving a media pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineWorkerEvent {
    /// Worker has stopped and will not process more frames.
    Stopped { reason: PipelineWorkerStopReason },
}

impl PipelineWorkerEvent {
    /// Stable worker event category.
    pub fn kind(&self) -> PipelineWorkerEventKind {
        match self {
            Self::Stopped { .. } => PipelineWorkerEventKind::Stopped,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PipelineWorkerEventKind {
    Stopped,
}

impl PipelineWorkerEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
        }
    }
}

impl std::fmt::Display for PipelineWorkerEventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Terminal reason for a media pipeline worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineWorkerStopReason {
    /// Capture closed normally, usually because the handle was stopped or the source ended.
    CaptureClosed,
    /// A pipeline stage returned an error and stopped the worker.
    StageFailed(PipelineStageError),
}

/// Lifecycle event for a media sink attached to a Styx session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkLifecycleEvent {
    /// Sink has started.
    Started { sink_id: String, kind: SinkKind },
    /// Sink has stopped.
    Stopped { sink_id: String, kind: SinkKind },
    /// Sink reported an error.
    Error {
        sink_id: String,
        kind: SinkKind,
        message: String,
    },
}

impl SinkLifecycleEvent {
    /// Stable sink lifecycle category.
    pub fn kind(&self) -> SinkLifecycleEventKind {
        match self {
            Self::Started { .. } => SinkLifecycleEventKind::Started,
            Self::Stopped { .. } => SinkLifecycleEventKind::Stopped,
            Self::Error { .. } => SinkLifecycleEventKind::Error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SinkLifecycleEventKind {
    Started,
    Stopped,
    Error,
}

impl SinkLifecycleEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for SinkLifecycleEventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Sink category used for service diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkKind {
    /// Preview/display sink.
    Preview,
    /// Frame recorder sink.
    Recorder,
    /// Image sequence writer.
    FileSequence,
    /// Network stream writer.
    NetworkStream,
    /// Analysis/tap sink.
    Analysis,
}

impl std::fmt::Display for SinkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Preview => "preview",
            Self::Recorder => "recorder",
            Self::FileSequence => "file-sequence",
            Self::NetworkStream => "network-stream",
            Self::Analysis => "analysis",
        })
    }
}

impl std::str::FromStr for SinkKind {
    type Err = SinkKindParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "preview" | "display" => Ok(Self::Preview),
            "recorder" | "recording" => Ok(Self::Recorder),
            "file-sequence" | "file_sequence" | "sequence" => Ok(Self::FileSequence),
            "network-stream" | "network_stream" | "netstream" => Ok(Self::NetworkStream),
            "analysis" | "tap" => Ok(Self::Analysis),
            _ => Err(SinkKindParseError {
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkKindParseError {
    value: String,
}

impl std::fmt::Display for SinkKindParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown sink kind '{}'", self.value)
    }
}

impl std::error::Error for SinkKindParseError {}

/// Recording lifecycle event emitted by a pipeline recorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingLifecycleEvent {
    /// Recording session has started.
    Started {
        session_id: String,
        directory: String,
    },
    /// A frame was written and indexed.
    FrameIndexed {
        session_id: String,
        sequence: u64,
        path: String,
    },
    /// Recording session has stopped.
    Stopped { session_id: String, frames: usize },
}

impl RecordingLifecycleEvent {
    /// Stable recording lifecycle category.
    pub fn kind(&self) -> RecordingLifecycleEventKind {
        match self {
            Self::Started { .. } => RecordingLifecycleEventKind::Started,
            Self::FrameIndexed { .. } => RecordingLifecycleEventKind::FrameIndexed,
            Self::Stopped { .. } => RecordingLifecycleEventKind::Stopped,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RecordingLifecycleEventKind {
    Started,
    FrameIndexed,
    Stopped,
}

impl RecordingLifecycleEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::FrameIndexed => "frame-indexed",
            Self::Stopped => "stopped",
        }
    }
}

impl std::fmt::Display for RecordingLifecycleEventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Cursor into the retained service event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceEventCursor {
    next_index: usize,
}

impl ServiceEventCursor {
    /// Create a cursor that reads retained events from the beginning.
    pub fn from_start() -> Self {
        Self { next_index: 0 }
    }

    /// Return the absolute event index that the cursor will read next.
    pub fn next_index(self) -> usize {
        self.next_index
    }
}

/// Borrowed poll result for retained service events.
#[derive(Debug, Clone)]
pub struct ServiceEventPoll<'a> {
    events: &'a [TimestampedServiceEvent],
    was_truncated: bool,
}

impl<'a> ServiceEventPoll<'a> {
    /// Events visible since the caller's previous cursor position.
    pub fn events(&self) -> &'a [TimestampedServiceEvent] {
        self.events
    }

    /// Whether the cursor fell behind the retention window.
    pub fn was_truncated(&self) -> bool {
        self.was_truncated
    }
}

/// Service event with sequence and wall-clock timestamp metadata.
#[derive(Debug, Clone)]
pub struct TimestampedServiceEvent {
    /// Monotonic sequence assigned by the runtime.
    pub sequence: u64,
    /// Unix timestamp in milliseconds when the event was recorded.
    pub unix_ms: u128,
    /// Event payload.
    pub event: StyxServiceEvent,
}

/// Configuration for retained service events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyxServiceConfig {
    /// Maximum number of events retained for polling subscribers.
    pub max_retained_events: usize,
}

impl Default for StyxServiceConfig {
    fn default() -> Self {
        Self {
            max_retained_events: 512,
        }
    }
}

/// Runtime facade that joins inventory watch state with retained service events.
pub struct StyxServiceRuntime {
    watch: WatchRuntime,
    config: StyxServiceConfig,
    base_index: usize,
    next_sequence: u64,
    events: Vec<TimestampedServiceEvent>,
    notifier: Arc<ServiceEventNotifier>,
}

impl Default for StyxServiceRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl StyxServiceRuntime {
    /// Create a runtime with default watch and event retention settings.
    pub fn new() -> Self {
        Self::with_watch_runtime(WatchRuntime::new(), StyxServiceConfig::default())
    }

    /// Create a runtime with default watch state and explicit event retention settings.
    pub fn with_config(config: StyxServiceConfig) -> Self {
        Self::with_watch_runtime(WatchRuntime::new(), config)
    }

    /// Create a runtime around an existing watch runtime.
    pub fn with_watch_runtime(watch: WatchRuntime, config: StyxServiceConfig) -> Self {
        Self {
            watch,
            config,
            base_index: 0,
            next_sequence: 0,
            events: Vec::new(),
            notifier: Arc::new(ServiceEventNotifier::default()),
        }
    }

    /// Borrow the active service runtime configuration.
    pub fn config(&self) -> StyxServiceConfig {
        self.config
    }

    /// Borrow the inventory watch runtime.
    pub fn watch(&self) -> &WatchRuntime {
        &self.watch
    }

    /// Mutably borrow the inventory watch runtime.
    pub fn watch_mut(&mut self) -> &mut WatchRuntime {
        &mut self.watch
    }

    /// Subscribe to future events.
    pub fn subscribe(&self) -> ServiceEventCursor {
        ServiceEventCursor {
            next_index: self.event_tail_index(),
        }
    }

    /// Subscribe from the beginning of the retained event window.
    pub fn subscribe_from_start(&self) -> ServiceEventCursor {
        ServiceEventCursor {
            next_index: self.base_index,
        }
    }

    /// Poll retained events and advance the supplied cursor.
    pub fn poll_events<'a>(&'a self, cursor: &mut ServiceEventCursor) -> ServiceEventPoll<'a> {
        let was_truncated = cursor.next_index < self.base_index;
        let start_index = cursor
            .next_index
            .max(self.base_index)
            .min(self.event_tail_index());
        let start_offset = start_index - self.base_index;
        cursor.next_index = self.event_tail_index();
        ServiceEventPoll {
            events: &self.events[start_offset..],
            was_truncated,
        }
    }

    /// Wait until another event is recorded or the timeout expires.
    pub fn wait_for_event(&self, timeout: Option<Duration>) -> bool {
        self.notifier.wait(timeout)
    }

    /// Refresh inventory and record the resulting inventory events.
    pub fn refresh_devices(&mut self) -> WatchRefreshReport {
        let report = self.watch.refresh();
        self.record_inventory_events(&report.diff.events());
        report
    }

    /// Poll a watcher and record any resulting inventory refresh events.
    pub fn poll_watcher_and_refresh(
        &mut self,
        watcher: &mut dyn DeviceWatcher,
    ) -> Result<Option<WatchRefreshReport>, WatchError> {
        let report = self.watch.poll_watcher_and_refresh(watcher)?;
        if let Some(report) = &report {
            self.record_inventory_events(&report.diff.events());
        }
        Ok(report)
    }

    /// Record a health snapshot.
    pub fn record_health(&mut self, report: HealthReport) {
        self.push_event(StyxServiceEvent::Health(Box::new(report)));
    }

    /// Record a pipeline worker lifecycle event.
    pub fn record_pipeline_event(&mut self, event: PipelineWorkerEvent) {
        self.push_event(StyxServiceEvent::Pipeline(event));
    }

    /// Record a sink lifecycle event.
    pub fn record_sink_event(&mut self, event: SinkLifecycleEvent) {
        self.push_event(StyxServiceEvent::Sink(event));
    }

    /// Record a recording lifecycle event.
    pub fn record_recording_event(&mut self, event: RecordingLifecycleEvent) {
        self.push_event(StyxServiceEvent::Recording(event));
    }

    /// Record a graph control result.
    #[cfg(feature = "daedalus-plugin")]
    pub fn record_control_result(&mut self, result: crate::graph::StyxControlResult) {
        self.push_event(StyxServiceEvent::Control(result));
    }

    fn record_inventory_events(&mut self, events: &[InventoryEvent]) {
        for event in events {
            self.push_event(StyxServiceEvent::Device(event.clone()));
        }
    }

    fn push_event(&mut self, event: StyxServiceEvent) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.events.push(TimestampedServiceEvent {
            sequence,
            unix_ms: now_unix_ms(),
            event,
        });
        self.enforce_retention();
        self.notifier.notify();
    }

    fn enforce_retention(&mut self) {
        let overflow = self
            .events
            .len()
            .saturating_sub(self.config.max_retained_events);
        if overflow > 0 {
            self.events.drain(..overflow);
            self.base_index = self.base_index.saturating_add(overflow);
        }
    }

    fn event_tail_index(&self) -> usize {
        self.base_index + self.events.len()
    }
}

#[derive(Debug, Default)]
struct ServiceEventNotifier {
    state: Mutex<u64>,
    changed: Condvar,
}

impl ServiceEventNotifier {
    fn notify(&self) {
        let mut version = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *version = version.saturating_add(1);
        self.changed.notify_all();
    }

    fn wait(&self, timeout: Option<Duration>) -> bool {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let version = *guard;
        match timeout {
            None => {
                while *guard <= version {
                    guard = self
                        .changed
                        .wait(guard)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                true
            }
            Some(timeout) => {
                let (next, result) = self
                    .changed
                    .wait_timeout_while(guard, timeout, |current| *current <= version)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard = next;
                !result.timed_out() || *guard > version
            }
        }
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::HealthReport;

    #[test]
    fn service_runtime_retains_and_polls_events() {
        let mut service = StyxServiceRuntime::with_watch_runtime(
            WatchRuntime::new(),
            StyxServiceConfig {
                max_retained_events: 2,
            },
        );
        let mut cursor = service.subscribe_from_start();
        service.record_health(HealthReport::default());
        service.record_sink_event(SinkLifecycleEvent::Started {
            sink_id: "preview".into(),
            kind: SinkKind::Preview,
        });
        service.record_sink_event(SinkLifecycleEvent::Stopped {
            sink_id: "preview".into(),
            kind: SinkKind::Preview,
        });

        let poll = service.poll_events(&mut cursor);
        assert!(poll.was_truncated());
        assert_eq!(poll.events().len(), 2);
    }

    #[test]
    fn service_runtime_accepts_explicit_config() {
        let service = StyxServiceRuntime::with_config(StyxServiceConfig {
            max_retained_events: 3,
        });

        assert_eq!(service.config().max_retained_events, 3);
    }

    #[test]
    fn sink_kind_roundtrips_stable_api_strings() {
        assert_eq!(SinkKind::NetworkStream.to_string(), "network-stream");
        assert_eq!(
            "file_sequence".parse::<SinkKind>(),
            Ok(SinkKind::FileSequence)
        );
        assert!("unknown".parse::<SinkKind>().is_err());
    }

    #[test]
    fn service_events_expose_typed_stable_kinds() {
        let health = StyxServiceEvent::Health(Box::default());
        assert_eq!(health.kind(), StyxServiceEventKind::Health);
        assert_eq!(health.kind().to_string(), "health");

        let sink = SinkLifecycleEvent::Error {
            sink_id: "recording".into(),
            kind: SinkKind::Recorder,
            message: "disk full".into(),
        };
        assert_eq!(sink.kind(), SinkLifecycleEventKind::Error);
        assert_eq!(sink.kind().as_str(), "error");

        let recording = RecordingLifecycleEvent::FrameIndexed {
            session_id: "session".into(),
            sequence: 7,
            path: "frame.jpg".into(),
        };
        assert_eq!(recording.kind(), RecordingLifecycleEventKind::FrameIndexed);
        assert_eq!(recording.kind().to_string(), "frame-indexed");

        let pipeline = PipelineWorkerEvent::Stopped {
            reason: PipelineWorkerStopReason::CaptureClosed,
        };
        assert_eq!(pipeline.kind(), PipelineWorkerEventKind::Stopped);
        assert_eq!(pipeline.kind().as_str(), "stopped");
    }
}
