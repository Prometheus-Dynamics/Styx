use std::io::Write;
#[cfg(feature = "hooks")]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::core::prelude::FrameLease;
#[cfg(feature = "hooks")]
use crate::recording::{FrameRecorder, RecordingOptions};
#[cfg(feature = "hooks")]
use crate::service::RecordingLifecycleEvent;
use crate::service::{SharedStyxServiceRuntime, SinkKind, SinkLifecycleEvent};
use daedalus::NodeHandle;
use daedalus::runtime::NodeError;
#[cfg(feature = "hooks")]
use daedalus::runtime::plugins::PluginError;
use daedalus::runtime::plugins::PluginResult;

use super::{framelease_node_decl, framelease_payload, register_framelease_type};

pub type FrameSinkCell = Arc<Mutex<Box<dyn FnMut(&FrameLease) + Send>>>;
pub type NetworkStreamWriter = Arc<Mutex<Box<dyn Write + Send>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkNodeConfig {
    pub label: &'static str,
    pub kind: SinkKind,
}

impl SinkNodeConfig {
    pub const fn new(label: &'static str) -> Self {
        Self {
            label,
            kind: SinkKind::Analysis,
        }
    }

    pub const fn kind(mut self, kind: SinkKind) -> Self {
        self.kind = kind;
        self
    }
}

/// Options for byte-stream graph sinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkStreamSinkOptions {
    /// Prefix each frame with a little-endian `u64` payload length.
    pub length_prefix: bool,
    /// Flush the writer after every frame.
    pub flush_each_frame: bool,
}

impl Default for NetworkStreamSinkOptions {
    fn default() -> Self {
        Self {
            length_prefix: true,
            flush_each_frame: true,
        }
    }
}

pub fn register_frame_sink_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    config: SinkNodeConfig,
    sink: FrameSinkCell,
) -> PluginResult<NodeHandle> {
    register_frame_sink_node_with_service(registry, node_id, config, sink, None)
}

pub fn register_frame_sink_node_with_service(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    config: SinkNodeConfig,
    sink: FrameSinkCell,
    service: Option<SharedStyxServiceRuntime>,
) -> PluginResult<NodeHandle> {
    register_frame_tap_sink_node(registry, node_id, config.label, config.kind, sink, service)
}

pub(crate) fn register_preview_sink_node_with_service(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    sink: FrameSinkCell,
    service: Option<SharedStyxServiceRuntime>,
) -> PluginResult<NodeHandle> {
    register_frame_tap_sink_node(
        registry,
        node_id,
        "Styx preview sink",
        SinkKind::Preview,
        sink,
        service,
    )
}

pub(crate) fn register_analysis_sink_node_with_service(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    sink: FrameSinkCell,
    service: Option<SharedStyxServiceRuntime>,
) -> PluginResult<NodeHandle> {
    register_frame_tap_sink_node(
        registry,
        node_id,
        "Styx analysis sink",
        SinkKind::Analysis,
        sink,
        service,
    )
}

#[cfg(feature = "hooks")]
pub(crate) fn register_recorder_sink_node_with_service(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    recorder: Arc<Mutex<FrameRecorder>>,
    service: Option<SharedStyxServiceRuntime>,
) -> PluginResult<NodeHandle> {
    register_framelease_type();
    let node_id = node_id.into();
    registry.register_node_decl(framelease_node_decl(&node_id, "Styx recorder sink"))?;
    let emitter = SinkEventEmitter::new(service, node_id.clone(), SinkKind::Recorder);
    if let Ok(recorder) = recorder.lock() {
        emitter.recording_started(
            recorder.metadata().session_id.clone(),
            recorder.metadata().directory.display().to_string(),
        );
    }
    emitter.started();
    registry
        .handlers
        .try_on_stateful(&node_id, move |_node, _ctx, io| {
            let Some(frame) = io.take_owned::<FrameLease>("frame") else {
                return Ok(());
            };
            let record_result = {
                let mut recorder = recorder.lock().map_err(|_| {
                    emitter.error("recorder sink lock poisoned".into());
                    NodeError::Handler("recorder sink lock poisoned".into())
                })?;
                let sequence = recorder.next_sequence();
                let session_id = recorder.metadata().session_id.clone();
                recorder
                    .record(&frame)
                    .map(|path| (session_id, sequence, path.display().to_string()))
                    .map_err(|err| err.to_string())
            };
            match record_result {
                Ok((session_id, sequence, path)) => {
                    emitter.frame_indexed(session_id, sequence, path)
                }
                Err(err) => {
                    emitter.error(err.clone());
                    return Err(NodeError::Handler(err));
                }
            }
            io.push_payload("frame", framelease_payload(frame));
            Ok(())
        })
        .map_err(|_| "recorder sink handler register failed")?;
    Ok(NodeHandle::new(node_id))
}

#[cfg(feature = "hooks")]
pub fn register_file_sequence_sink_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    dir: impl Into<PathBuf>,
    options: RecordingOptions,
) -> PluginResult<NodeHandle> {
    register_file_sequence_sink_node_with_service(registry, node_id, dir, options, None)
}

#[cfg(feature = "hooks")]
pub fn register_file_sequence_sink_node_with_service(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    dir: impl Into<PathBuf>,
    options: RecordingOptions,
    service: Option<SharedStyxServiceRuntime>,
) -> PluginResult<NodeHandle> {
    let recorder = FrameRecorder::new(dir, options).map_err(|err| PluginError::Install {
        message: err.to_string(),
    })?;
    let node_id = node_id.into();
    register_framelease_type();
    registry.register_node_decl(framelease_node_decl(&node_id, "Styx file sequence sink"))?;
    let recorder = Arc::new(Mutex::new(recorder));
    let emitter = SinkEventEmitter::new(service, node_id.clone(), SinkKind::FileSequence);
    if let Ok(recorder) = recorder.lock() {
        emitter.recording_started(
            recorder.metadata().session_id.clone(),
            recorder.metadata().directory.display().to_string(),
        );
    }
    emitter.started();
    registry
        .handlers
        .try_on_stateful(&node_id, move |_node, _ctx, io| {
            let Some(frame) = io.take_owned::<FrameLease>("frame") else {
                return Ok(());
            };
            let record_result = {
                let mut recorder = recorder.lock().map_err(|_| {
                    emitter.error("file sequence sink lock poisoned".into());
                    NodeError::Handler("file sequence sink lock poisoned".into())
                })?;
                let sequence = recorder.next_sequence();
                let session_id = recorder.metadata().session_id.clone();
                recorder
                    .record(&frame)
                    .map(|path| (session_id, sequence, path.display().to_string()))
                    .map_err(|err| err.to_string())
            };
            match record_result {
                Ok((session_id, sequence, path)) => {
                    emitter.frame_indexed(session_id, sequence, path)
                }
                Err(err) => {
                    emitter.error(err.clone());
                    return Err(NodeError::Handler(err));
                }
            }
            io.push_payload("frame", framelease_payload(frame));
            Ok(())
        })
        .map_err(|_| "file sequence sink handler register failed")?;
    Ok(NodeHandle::new(node_id))
}

pub fn register_network_stream_sink_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    writer: NetworkStreamWriter,
    options: NetworkStreamSinkOptions,
) -> PluginResult<NodeHandle> {
    register_network_stream_sink_node_with_service(registry, node_id, writer, options, None)
}

pub fn register_network_stream_sink_node_with_service(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    writer: NetworkStreamWriter,
    options: NetworkStreamSinkOptions,
    service: Option<SharedStyxServiceRuntime>,
) -> PluginResult<NodeHandle> {
    register_framelease_type();
    let node_id = node_id.into();
    registry.register_node_decl(framelease_node_decl(&node_id, "Styx network stream sink"))?;
    let emitter = SinkEventEmitter::new(service, node_id.clone(), SinkKind::NetworkStream);
    emitter.started();
    registry
        .handlers
        .try_on_stateful(&node_id, move |_node, _ctx, io| {
            let Some(frame) = io.take_owned::<FrameLease>("frame") else {
                return Ok(());
            };
            let planes = frame.planes();
            let bytes = planes.iter().map(|plane| plane.data().len()).sum::<usize>() as u64;
            let mut writer = writer.lock().map_err(|_| {
                emitter.error("network stream sink lock poisoned".into());
                NodeError::Handler("network stream sink lock poisoned".into())
            })?;
            if options.length_prefix
                && let Err(err) = writer.write_all(&bytes.to_le_bytes())
            {
                emitter.error(err.to_string());
                return Err(NodeError::Handler(err.to_string()));
            }
            for plane in planes {
                if let Err(err) = writer.write_all(plane.data()) {
                    emitter.error(err.to_string());
                    return Err(NodeError::Handler(err.to_string()));
                }
            }
            if options.flush_each_frame
                && let Err(err) = writer.flush()
            {
                emitter.error(err.to_string());
                return Err(NodeError::Handler(err.to_string()));
            }
            drop(writer);
            io.push_payload("frame", framelease_payload(frame));
            Ok(())
        })
        .map_err(|_| "network stream sink handler register failed")?;
    Ok(NodeHandle::new(node_id))
}

fn register_frame_tap_sink_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    label: &'static str,
    kind: SinkKind,
    sink: FrameSinkCell,
    service: Option<SharedStyxServiceRuntime>,
) -> PluginResult<NodeHandle> {
    register_framelease_type();
    let node_id = node_id.into();
    registry.register_node_decl(framelease_node_decl(&node_id, label))?;
    let emitter = SinkEventEmitter::new(service, node_id.clone(), kind);
    emitter.started();
    registry
        .handlers
        .try_on_stateful(&node_id, move |_node, _ctx, io| {
            let Some(frame) = io.take_owned::<FrameLease>("frame") else {
                return Ok(());
            };
            sink.lock().map_err(|_| {
                emitter.error("frame sink lock poisoned".into());
                NodeError::Handler("frame sink lock poisoned".into())
            })?(&frame);
            io.push_payload("frame", framelease_payload(frame));
            Ok(())
        })
        .map_err(|_| "frame sink handler register failed")?;
    Ok(NodeHandle::new(node_id))
}

#[derive(Clone)]
struct SinkEventEmitter {
    service: Option<SharedStyxServiceRuntime>,
    sink_id: String,
    kind: SinkKind,
    #[cfg(feature = "hooks")]
    recording: Arc<Mutex<Option<RecordingStopState>>>,
}

impl SinkEventEmitter {
    fn new(service: Option<SharedStyxServiceRuntime>, sink_id: String, kind: SinkKind) -> Self {
        Self {
            service,
            sink_id,
            kind,
            #[cfg(feature = "hooks")]
            recording: Arc::new(Mutex::new(None)),
        }
    }

    fn started(&self) {
        self.with_service(|service| {
            service.record_sink_event(SinkLifecycleEvent::Started {
                sink_id: self.sink_id.clone(),
                kind: self.kind,
            });
        });
    }

    fn error(&self, message: String) {
        self.with_service(|service| {
            service.record_sink_event(SinkLifecycleEvent::Error {
                sink_id: self.sink_id.clone(),
                kind: self.kind,
                message,
            });
        });
    }

    #[cfg(feature = "hooks")]
    fn recording_started(&self, session_id: String, directory: String) {
        if let Ok(mut recording) = self.recording.lock() {
            *recording = Some(RecordingStopState {
                session_id: session_id.clone(),
                frames: 0,
            });
        }
        self.with_service(|service| {
            service.record_recording_event(RecordingLifecycleEvent::Started {
                session_id,
                directory,
            });
        });
    }

    #[cfg(feature = "hooks")]
    fn frame_indexed(&self, session_id: String, sequence: u64, path: String) {
        if let Ok(mut recording) = self.recording.lock()
            && let Some(recording) = recording.as_mut()
        {
            recording.frames = recording.frames.saturating_add(1);
        }
        self.with_service(|service| {
            service.record_recording_event(RecordingLifecycleEvent::FrameIndexed {
                session_id,
                sequence,
                path,
            });
        });
    }

    fn stopped(&self) {
        #[cfg(feature = "hooks")]
        let recording = self
            .recording
            .lock()
            .ok()
            .and_then(|mut recording| recording.take());
        self.with_service(|service| {
            #[cfg(feature = "hooks")]
            if let Some(recording) = recording {
                service.record_recording_event(RecordingLifecycleEvent::Stopped {
                    session_id: recording.session_id,
                    frames: recording.frames,
                });
            }
            service.record_sink_event(SinkLifecycleEvent::Stopped {
                sink_id: self.sink_id.clone(),
                kind: self.kind,
            });
        });
    }

    fn with_service(&self, f: impl FnOnce(&mut crate::service::StyxServiceRuntime)) {
        if let Some(service) = &self.service
            && let Ok(mut service) = service.lock()
        {
            f(&mut service);
        }
    }
}

#[cfg(feature = "hooks")]
#[derive(Clone)]
struct RecordingStopState {
    session_id: String,
    frames: usize,
}

impl Drop for SinkEventEmitter {
    fn drop(&mut self) {
        self.stopped();
    }
}
