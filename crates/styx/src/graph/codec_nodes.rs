use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::sync::Mutex;

use crate::core::prelude::FrameLease;
#[cfg(target_os = "linux")]
use crate::core::prelude::SharedBufferPool;
#[cfg(target_os = "linux")]
use crate::frame_sizing::{
    SHARED_CODEC_POOL_MIN, SHARED_CODEC_POOL_SPARE, estimated_compressed_packet_pool_bytes,
    estimated_format_bytes,
};
use daedalus::NodeHandle;
use daedalus::runtime::NodeError;
use daedalus::runtime::plugins::PluginResult;
use styx_codec::prelude::{Codec, CodecDescriptor, CodecError, CodecKind};

use super::{framelease_node_decl, framelease_payload, register_framelease_type};

/// Options used when installing codec-backed graph nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyxCodecNodeOptions {
    /// Prefer exportable/shared output buffers when the codec supports them.
    #[cfg(target_os = "linux")]
    pub shared_output: bool,
    /// Allow owned heap-backed codec output if shared/exportable output is unavailable.
    #[cfg(target_os = "linux")]
    pub owned_fallback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyxCodecNodeDescriptor {
    pub node_id: String,
    pub kind: CodecKind,
    pub input: String,
    pub output: String,
    pub name: String,
    pub implementation: String,
    pub options: StyxCodecNodeOptions,
}

impl StyxCodecNodeDescriptor {
    pub fn from_codec(codec: &dyn Codec, options: StyxCodecNodeOptions) -> Self {
        Self::from_descriptor(codec.descriptor(), options)
    }

    pub fn from_descriptor(descriptor: &CodecDescriptor, options: StyxCodecNodeOptions) -> Self {
        Self {
            node_id: concrete_codec_node_id(descriptor),
            kind: descriptor.kind,
            input: descriptor.input.to_string(),
            output: descriptor.output.to_string(),
            name: descriptor.name.to_string(),
            implementation: descriptor.impl_name.to_string(),
            options,
        }
    }
}

impl Default for StyxCodecNodeOptions {
    fn default() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            shared_output: true,
            #[cfg(target_os = "linux")]
            owned_fallback: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodecStage {
    Decode,
    Encode,
}

impl CodecStage {
    fn label(self) -> &'static str {
        match self {
            Self::Decode => "Styx decode",
            Self::Encode => "Styx encode",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Decode => "decoder",
            Self::Encode => "encoder",
        }
    }
}

fn register_codec_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    node_id: impl Into<String>,
    codec: Arc<dyn Codec>,
    stage: CodecStage,
    options: StyxCodecNodeOptions,
) -> PluginResult<NodeHandle> {
    register_framelease_type();
    let node_id = node_id.into();
    let node = framelease_node_decl(&node_id, stage.label());
    registry.register_node_decl(node)?;

    #[cfg(target_os = "linux")]
    let shared_pool = Arc::new(Mutex::new(None::<(SharedBufferPool, usize)>));

    registry
        .handlers
        .try_on(&node_id, move |_node, _ctx, io| {
            let Some(frame) = io.take_owned::<FrameLease>("frame") else {
                return Ok(());
            };
            #[cfg(target_os = "linux")]
            let out = process_codec_frame(
                stage,
                Arc::clone(&codec),
                frame,
                options,
                Arc::clone(&shared_pool),
            )?;
            #[cfg(not(target_os = "linux"))]
            let out = codec
                .process(frame)
                .map_err(|err| codec_node_error(stage, codec.descriptor(), err))?;
            io.push_payload("frame", framelease_payload(out));
            Ok(())
        })
        .map_err(|_| "codec node handler register failed")?;

    Ok(NodeHandle::new(node_id))
}

pub(super) fn register_concrete_codec_node(
    registry: &mut daedalus::runtime::plugins::PluginRegistry,
    codec: Arc<dyn Codec>,
    options: StyxCodecNodeOptions,
) -> PluginResult<NodeHandle> {
    let descriptor = codec.descriptor().clone();
    let node_id = concrete_codec_node_id(&descriptor);
    let stage = match descriptor.kind {
        CodecKind::Decoder => CodecStage::Decode,
        CodecKind::Encoder => CodecStage::Encode,
    };
    register_codec_node(registry, node_id, codec, stage, options)
}

pub fn concrete_codec_node_id(descriptor: &CodecDescriptor) -> String {
    format!(
        "styx.codec.{}.{}.{}.{}",
        match descriptor.kind {
            CodecKind::Decoder => "decoder",
            CodecKind::Encoder => "encoder",
        },
        codec_id_part(&descriptor.input.to_string()),
        codec_id_part(&descriptor.output.to_string()),
        codec_id_part(descriptor.impl_name),
    )
}

fn codec_id_part(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

#[cfg(target_os = "linux")]
fn process_codec_frame(
    stage: CodecStage,
    codec: Arc<dyn Codec>,
    frame: FrameLease,
    options: StyxCodecNodeOptions,
    shared_pool: Arc<Mutex<Option<(SharedBufferPool, usize)>>>,
) -> Result<FrameLease, NodeError> {
    if !options.shared_output {
        return codec
            .process(frame)
            .map_err(|err| codec_node_error(stage, codec.descriptor(), err));
    }
    let pool =
        codec_shared_pool(stage, codec.descriptor(), &frame, &shared_pool).map_err(|err| {
            NodeError::Handler(format!(
                "{} {}:{} shared output pool failed: {err}",
                stage.name(),
                codec.descriptor().name,
                codec.descriptor().impl_name
            ))
        })?;
    match codec.process_shared(&frame, &pool) {
        Ok(Some(out)) => Ok(out),
        Ok(None) => codec
            .process(frame)
            .and_then(|out| require_exportable_codec_output(stage, codec.as_ref(), out, options))
            .map_err(|err| codec_node_error(stage, codec.descriptor(), err)),
        Err(err) => Err(codec_node_error(stage, codec.descriptor(), err)),
    }
}

#[cfg(target_os = "linux")]
fn codec_shared_pool(
    stage: CodecStage,
    descriptor: &CodecDescriptor,
    frame: &FrameLease,
    shared_pool: &Mutex<Option<(SharedBufferPool, usize)>>,
) -> Result<SharedBufferPool, NodeError> {
    let bytes = estimate_shared_output_bytes(stage, descriptor, frame).max(1);
    let mut guard = shared_pool
        .lock()
        .map_err(|_| NodeError::Handler("shared pool lock poisoned".into()))?;
    let recreate = guard
        .as_ref()
        .map(|(_, capacity)| *capacity < bytes)
        .unwrap_or(true);
    if recreate {
        *guard = Some((
            SharedBufferPool::with_limits(SHARED_CODEC_POOL_MIN, bytes, SHARED_CODEC_POOL_SPARE)
                .map_err(|err| NodeError::Handler(err.to_string()))?,
            bytes,
        ));
    }
    Ok(guard
        .as_ref()
        .expect("shared codec pool initialized")
        .0
        .clone())
}

#[cfg(target_os = "linux")]
fn estimate_shared_output_bytes(
    stage: CodecStage,
    descriptor: &CodecDescriptor,
    frame: &FrameLease,
) -> usize {
    let res = frame.meta().format.resolution;
    match estimated_format_bytes(
        descriptor.output,
        res.width.get() as usize,
        res.height.get() as usize,
    ) {
        Some(bytes) => bytes,
        None if stage == CodecStage::Encode => estimated_compressed_packet_pool_bytes(
            descriptor.input,
            descriptor.output,
            res.width.get() as usize,
            res.height.get() as usize,
            frame.payload_bytes(),
        )
        .unwrap_or_else(|| frame.payload_bytes().max(64 * 1024)),
        None => frame.payload_bytes().max(1),
    }
}

#[cfg(target_os = "linux")]
fn require_exportable_codec_output(
    stage: CodecStage,
    codec: &dyn Codec,
    frame: FrameLease,
    options: StyxCodecNodeOptions,
) -> Result<FrameLease, CodecError> {
    if options.owned_fallback {
        return Ok(frame);
    }
    match frame.export_backing() {
        Ok(_) => Ok(frame),
        Err(err) => Err(CodecError::Codec(format!(
            "{} {}:{} produced non-exportable output: {err}",
            stage.name(),
            codec.descriptor().name,
            codec.descriptor().impl_name
        ))),
    }
}

fn codec_node_error(stage: CodecStage, descriptor: &CodecDescriptor, err: CodecError) -> NodeError {
    let message = format!(
        "{} {}:{} failed: {err}",
        stage.name(),
        descriptor.name,
        descriptor.impl_name
    );
    match err {
        CodecError::Backpressure => NodeError::BackpressureDrop(message),
        CodecError::FormatMismatch { .. } | CodecError::Codec(_) => NodeError::Handler(message),
    }
}
