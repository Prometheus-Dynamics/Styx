use std::io::Write;
#[cfg(feature = "hooks")]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::capabilities::StyxCapabilityInventory;
use crate::capture_api::CaptureHandle;
use crate::core::prelude::FrameLease as BorrowedSinkFrameLease;
#[cfg(feature = "hooks")]
use crate::core::prelude::{FrameLease, FrameTransform};
#[cfg(feature = "hooks")]
use crate::recording::{FrameRecorder, RecordingOptions};
use crate::service::{SharedStyxServiceRuntime, SinkKind};
use daedalus::NodeHandle;
use daedalus::runtime::plugins::{Plugin, PluginInstallContext, PluginResult};
use styx_codec::prelude::{Codec, CodecRegistryHandle};

use super::codec_nodes::{
    StyxCodecNodeDescriptor, StyxCodecNodeOptions, register_concrete_codec_node,
};
#[cfg(feature = "hooks")]
use super::runtime_nodes::{register_frame_hook_node, register_transform_node};
use super::sinks::{
    FrameSinkCell, NetworkStreamSinkOptions, NetworkStreamWriter,
    register_analysis_sink_node_with_service, register_network_stream_sink_node_with_service,
    register_preview_sink_node_with_service,
};
#[cfg(feature = "hooks")]
use super::sinks::{
    register_file_sequence_sink_node_with_service, register_recorder_sink_node_with_service,
};
use super::sources::register_shared_capture_source_node_with_options;
use super::{register_framelease_type, register_styx_capabilities};

/// Concrete Styx media plugin installer.
///
/// Codecs are registered as exact graph nodes. A graph should contain
/// `styx.codec.decoder.mjpg.turbojpeg`, not a generic runtime "decode" node.
#[derive(Default)]
pub struct StyxMediaPlugin {
    source_nodes: Vec<StyxSourceNodeRegistration>,
    codec_registrations: Vec<StyxCodecRegistration>,
    #[cfg(feature = "hooks")]
    runtime_nodes: Vec<StyxRuntimeNodeRegistration>,
    sink_nodes: Vec<StyxSinkNodeRegistration>,
    service_runtime: Option<SharedStyxServiceRuntime>,
}

impl StyxMediaPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_service_runtime(mut self, service: SharedStyxServiceRuntime) -> Self {
        self.service_runtime = Some(service);
        self
    }

    pub fn set_service_runtime(&mut self, service: SharedStyxServiceRuntime) -> &mut Self {
        self.service_runtime = Some(service);
        self
    }

    pub fn source_descriptors(&self) -> Vec<StyxSourceDescriptor> {
        self.source_nodes
            .iter()
            .map(StyxSourceNodeRegistration::descriptor)
            .collect()
    }

    pub fn sink_descriptors(&self) -> Vec<StyxSinkDescriptor> {
        self.sink_nodes
            .iter()
            .map(StyxSinkNodeRegistration::descriptor)
            .collect()
    }

    pub fn codec_descriptors(&self) -> Vec<StyxCodecNodeDescriptor> {
        self.codec_registrations
            .iter()
            .map(StyxCodecRegistration::descriptor)
            .collect()
    }

    pub fn add_capture_source(
        &mut self,
        node_id: impl Into<String>,
        capture: CaptureHandle,
    ) -> NodeHandle {
        self.add_capture_source_with_options(
            node_id,
            capture,
            super::StyxCaptureSourceOptions::default(),
        )
    }

    pub fn add_capture_source_with_options(
        &mut self,
        node_id: impl Into<String>,
        capture: CaptureHandle,
        options: super::StyxCaptureSourceOptions,
    ) -> NodeHandle {
        let node_id = node_id.into();
        self.source_nodes
            .push(StyxSourceNodeRegistration::CaptureHandle {
                node_id: node_id.clone(),
                capture: Arc::new(capture),
                options,
            });
        NodeHandle::new(node_id)
    }

    pub fn add_preview_sink<F>(&mut self, node_id: impl Into<String>, sink: F) -> NodeHandle
    where
        F: FnMut(&BorrowedSinkFrameLease) + Send + 'static,
    {
        let node_id = node_id.into();
        self.sink_nodes.push(StyxSinkNodeRegistration::Preview {
            node_id: node_id.clone(),
            sink: Arc::new(Mutex::new(Box::new(sink))),
        });
        NodeHandle::new(node_id)
    }

    pub fn add_analysis_sink<F>(&mut self, node_id: impl Into<String>, sink: F) -> NodeHandle
    where
        F: FnMut(&BorrowedSinkFrameLease) + Send + 'static,
    {
        let node_id = node_id.into();
        self.sink_nodes.push(StyxSinkNodeRegistration::Analysis {
            node_id: node_id.clone(),
            sink: Arc::new(Mutex::new(Box::new(sink))),
        });
        NodeHandle::new(node_id)
    }

    #[cfg(feature = "hooks")]
    pub fn add_recorder_sink(
        &mut self,
        node_id: impl Into<String>,
        recorder: FrameRecorder,
    ) -> NodeHandle {
        let node_id = node_id.into();
        self.sink_nodes.push(StyxSinkNodeRegistration::Recorder {
            node_id: node_id.clone(),
            recorder: Arc::new(Mutex::new(recorder)),
        });
        NodeHandle::new(node_id)
    }

    #[cfg(feature = "hooks")]
    pub fn add_file_sequence_sink(
        &mut self,
        node_id: impl Into<String>,
        dir: impl Into<PathBuf>,
        options: RecordingOptions,
    ) -> NodeHandle {
        let node_id = node_id.into();
        self.sink_nodes
            .push(StyxSinkNodeRegistration::FileSequence {
                node_id: node_id.clone(),
                dir: dir.into(),
                options,
            });
        NodeHandle::new(node_id)
    }

    pub fn add_network_stream_sink<W>(
        &mut self,
        node_id: impl Into<String>,
        writer: W,
        options: NetworkStreamSinkOptions,
    ) -> NodeHandle
    where
        W: Write + Send + 'static,
    {
        let node_id = node_id.into();
        self.sink_nodes
            .push(StyxSinkNodeRegistration::NetworkStream {
                node_id: node_id.clone(),
                writer: Arc::new(Mutex::new(Box::new(writer))),
                options,
            });
        NodeHandle::new(node_id)
    }

    pub fn with_codec(mut self, codec: Arc<dyn Codec>, options: StyxCodecNodeOptions) -> Self {
        self.add_codec(codec, options);
        self
    }

    pub fn add_codec(&mut self, codec: Arc<dyn Codec>, options: StyxCodecNodeOptions) -> &mut Self {
        self.codec_registrations
            .push(StyxCodecRegistration { codec, options });
        self
    }

    pub fn add_codec_registry(
        &mut self,
        codecs: &CodecRegistryHandle,
        options: StyxCodecNodeOptions,
    ) -> Result<&mut Self, &'static str> {
        for (_fourcc, descriptors) in codecs.list_registered() {
            for descriptor in descriptors {
                let codec = codecs
                    .lookup_named_kind(descriptor.input, descriptor.kind, descriptor.impl_name)
                    .map_err(|_| "codec lookup failed while adding concrete codec node")?;
                self.add_codec(codec, options);
            }
        }
        Ok(self)
    }

    pub fn with_codec_registry(
        mut self,
        codecs: &CodecRegistryHandle,
        options: StyxCodecNodeOptions,
    ) -> Result<Self, &'static str> {
        self.add_codec_registry(codecs, options)?;
        Ok(self)
    }

    pub fn register_capabilities(
        &self,
        registry: &mut daedalus::runtime::plugins::PluginRegistry,
        inventory: &StyxCapabilityInventory,
    ) -> PluginResult<NodeHandle> {
        register_styx_capabilities(registry, inventory)
    }

    #[cfg(feature = "hooks")]
    pub(crate) fn add_transform(
        &mut self,
        node_id: impl Into<String>,
        transform: FrameTransform,
    ) -> NodeHandle {
        let node_id = node_id.into();
        self.runtime_nodes
            .push(StyxRuntimeNodeRegistration::Transform {
                node_id: node_id.clone(),
                transform,
            });
        NodeHandle::new(node_id)
    }

    #[cfg(feature = "hooks")]
    pub(crate) fn add_frame_hook<F>(&mut self, node_id: impl Into<String>, hook: F) -> NodeHandle
    where
        F: FnMut(FrameLease) -> FrameLease + Send + 'static,
    {
        let node_id = node_id.into();
        self.runtime_nodes
            .push(StyxRuntimeNodeRegistration::FrameHook {
                node_id: node_id.clone(),
                hook: Arc::new(Mutex::new(Box::new(hook))),
            });
        NodeHandle::new(node_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyxSinkDescriptor {
    pub node_id: String,
    pub kind: SinkKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyxSourceKind {
    CaptureHandle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyxSourceDescriptor {
    pub node_id: String,
    pub kind: StyxSourceKind,
    pub options: super::StyxCaptureSourceOptions,
}

impl std::fmt::Debug for StyxMediaPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("StyxMediaPlugin");
        debug.field("source_nodes", &self.source_nodes);
        debug.field("codec_registrations", &self.codec_registrations);
        #[cfg(feature = "hooks")]
        debug.field("runtime_nodes", &self.runtime_nodes);
        debug
            .field("sink_nodes", &self.sink_nodes)
            .field("service_runtime", &self.service_runtime.is_some())
            .finish()
    }
}

impl Plugin for StyxMediaPlugin {
    fn id(&self) -> &'static str {
        "styx.media"
    }

    fn install(&self, ctx: &mut PluginInstallContext<'_>) -> PluginResult<()> {
        register_framelease_type();
        for registration in &self.source_nodes {
            let handle = registration.register(ctx)?;
            ctx.manifest_mut()
                .provided_nodes
                .push(daedalus::registry::ids::NodeId::new(handle.id()));
        }
        for registration in &self.codec_registrations {
            let handle = register_concrete_codec_node(
                ctx,
                registration.codec.clone(),
                registration.options,
            )?;
            ctx.manifest_mut()
                .provided_nodes
                .push(daedalus::registry::ids::NodeId::new(handle.id()));
        }
        #[cfg(feature = "hooks")]
        for registration in &self.runtime_nodes {
            let handle = registration.register(ctx)?;
            ctx.manifest_mut()
                .provided_nodes
                .push(daedalus::registry::ids::NodeId::new(handle.id()));
        }
        for registration in &self.sink_nodes {
            let handle = registration.register(ctx, self.service_runtime.clone())?;
            ctx.manifest_mut()
                .provided_nodes
                .push(daedalus::registry::ids::NodeId::new(handle.id()));
        }
        Ok(())
    }
}

#[derive(Clone)]
enum StyxSourceNodeRegistration {
    CaptureHandle {
        node_id: String,
        capture: Arc<CaptureHandle>,
        options: super::StyxCaptureSourceOptions,
    },
}

impl StyxSourceNodeRegistration {
    fn descriptor(&self) -> StyxSourceDescriptor {
        match self {
            Self::CaptureHandle {
                node_id, options, ..
            } => StyxSourceDescriptor {
                node_id: node_id.clone(),
                kind: StyxSourceKind::CaptureHandle,
                options: *options,
            },
        }
    }

    fn register(
        &self,
        registry: &mut daedalus::runtime::plugins::PluginRegistry,
    ) -> PluginResult<NodeHandle> {
        match self {
            Self::CaptureHandle {
                node_id,
                capture,
                options,
            } => register_shared_capture_source_node_with_options(
                registry,
                node_id.clone(),
                Arc::clone(capture),
                *options,
            ),
        }
    }
}

impl std::fmt::Debug for StyxSourceNodeRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CaptureHandle {
                node_id, options, ..
            } => f
                .debug_struct("CaptureHandle")
                .field("node_id", node_id)
                .field("options", options)
                .finish(),
        }
    }
}

#[derive(Clone)]
struct StyxCodecRegistration {
    codec: Arc<dyn Codec>,
    options: StyxCodecNodeOptions,
}

impl StyxCodecRegistration {
    fn descriptor(&self) -> StyxCodecNodeDescriptor {
        StyxCodecNodeDescriptor::from_codec(self.codec.as_ref(), self.options)
    }
}

#[derive(Clone)]
enum StyxSinkNodeRegistration {
    Preview {
        node_id: String,
        sink: FrameSinkCell,
    },
    Analysis {
        node_id: String,
        sink: FrameSinkCell,
    },
    #[cfg(feature = "hooks")]
    Recorder {
        node_id: String,
        recorder: Arc<Mutex<FrameRecorder>>,
    },
    #[cfg(feature = "hooks")]
    FileSequence {
        node_id: String,
        dir: PathBuf,
        options: RecordingOptions,
    },
    NetworkStream {
        node_id: String,
        writer: NetworkStreamWriter,
        options: NetworkStreamSinkOptions,
    },
}

impl StyxSinkNodeRegistration {
    fn descriptor(&self) -> StyxSinkDescriptor {
        match self {
            Self::Preview { node_id, .. } => StyxSinkDescriptor {
                node_id: node_id.clone(),
                kind: SinkKind::Preview,
            },
            Self::Analysis { node_id, .. } => StyxSinkDescriptor {
                node_id: node_id.clone(),
                kind: SinkKind::Analysis,
            },
            #[cfg(feature = "hooks")]
            Self::Recorder { node_id, .. } => StyxSinkDescriptor {
                node_id: node_id.clone(),
                kind: SinkKind::Recorder,
            },
            #[cfg(feature = "hooks")]
            Self::FileSequence { node_id, .. } => StyxSinkDescriptor {
                node_id: node_id.clone(),
                kind: SinkKind::FileSequence,
            },
            Self::NetworkStream { node_id, .. } => StyxSinkDescriptor {
                node_id: node_id.clone(),
                kind: SinkKind::NetworkStream,
            },
        }
    }

    fn register(
        &self,
        registry: &mut daedalus::runtime::plugins::PluginRegistry,
        service: Option<SharedStyxServiceRuntime>,
    ) -> PluginResult<NodeHandle> {
        match self {
            Self::Preview { node_id, sink } => register_preview_sink_node_with_service(
                registry,
                node_id.clone(),
                Arc::clone(sink),
                service,
            ),
            Self::Analysis { node_id, sink } => register_analysis_sink_node_with_service(
                registry,
                node_id.clone(),
                Arc::clone(sink),
                service,
            ),
            #[cfg(feature = "hooks")]
            Self::Recorder { node_id, recorder } => register_recorder_sink_node_with_service(
                registry,
                node_id.clone(),
                Arc::clone(recorder),
                service,
            ),
            #[cfg(feature = "hooks")]
            Self::FileSequence {
                node_id,
                dir,
                options,
            } => register_file_sequence_sink_node_with_service(
                registry,
                node_id.clone(),
                dir.clone(),
                options.clone(),
                service,
            ),
            Self::NetworkStream {
                node_id,
                writer,
                options,
            } => register_network_stream_sink_node_with_service(
                registry,
                node_id.clone(),
                Arc::clone(writer),
                *options,
                service,
            ),
        }
    }
}

impl std::fmt::Debug for StyxSinkNodeRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preview { node_id, .. } => {
                f.debug_struct("Preview").field("node_id", node_id).finish()
            }
            Self::Analysis { node_id, .. } => f
                .debug_struct("Analysis")
                .field("node_id", node_id)
                .finish(),
            #[cfg(feature = "hooks")]
            Self::Recorder { node_id, .. } => f
                .debug_struct("Recorder")
                .field("node_id", node_id)
                .finish(),
            #[cfg(feature = "hooks")]
            Self::FileSequence { node_id, dir, .. } => f
                .debug_struct("FileSequence")
                .field("node_id", node_id)
                .field("dir", dir)
                .finish(),
            Self::NetworkStream {
                node_id, options, ..
            } => f
                .debug_struct("NetworkStream")
                .field("node_id", node_id)
                .field("options", options)
                .finish(),
        }
    }
}

impl std::fmt::Debug for StyxCodecRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StyxCodecRegistration")
            .field("descriptor", self.codec.descriptor())
            .field("options", &self.options)
            .finish()
    }
}

#[cfg(feature = "hooks")]
pub(super) type FrameHookCell = Arc<Mutex<Box<dyn FnMut(FrameLease) -> FrameLease + Send>>>;

#[cfg(feature = "hooks")]
#[derive(Clone)]
enum StyxRuntimeNodeRegistration {
    Transform {
        node_id: String,
        transform: FrameTransform,
    },
    FrameHook {
        node_id: String,
        hook: FrameHookCell,
    },
}

#[cfg(feature = "hooks")]
impl StyxRuntimeNodeRegistration {
    fn register(
        &self,
        registry: &mut daedalus::runtime::plugins::PluginRegistry,
    ) -> PluginResult<NodeHandle> {
        match self {
            Self::Transform { node_id, transform } => {
                register_transform_node(registry, node_id.clone(), *transform)
            }
            Self::FrameHook { node_id, hook } => {
                register_frame_hook_node(registry, node_id.clone(), Arc::clone(hook))
            }
        }
    }
}

#[cfg(feature = "hooks")]
impl std::fmt::Debug for StyxRuntimeNodeRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transform { node_id, transform } => f
                .debug_struct("Transform")
                .field("node_id", node_id)
                .field("transform", transform)
                .finish(),
            Self::FrameHook { node_id, .. } => f
                .debug_struct("FrameHook")
                .field("node_id", node_id)
                .finish(),
        }
    }
}
