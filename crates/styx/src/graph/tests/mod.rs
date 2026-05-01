use super::codec_nodes::register_concrete_codec_node;
use super::test_support::*;
use super::*;
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::capture_api::{CameraRequest, CaptureRequest, CaptureStartPolicy};
use crate::core::prelude::{
    BufferPool, ColorSpace, FourCc, FrameLease, FrameMeta, FrameResidency, FrameTransform,
    MediaFormat, Resolution, Rotation90,
};
use crate::{BackendHandle, BackendKind, DeviceIdentity, ProbedBackend, ProbedDevice};
use daedalus::Plugin;
use daedalus::registry::capability::{NodeDecl, PortDecl};
use styx_capture::prelude::{CaptureDescriptor, Interval, Mode, ModeId};
use styx_codec::prelude::{Codec, CodecDescriptor, CodecError, CodecKind, PassthroughDecoder};

fn test_frame() -> FrameLease {
    test_frame_with_timestamp(7)
}

fn test_frame_with_timestamp(timestamp: u64) -> FrameLease {
    let format = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(2, 2).unwrap(),
        ColorSpace::Srgb,
    );
    let layout = crate::core::prelude::plane_layout_from_dims(
        NonZeroU32::new(2).unwrap(),
        NonZeroU32::new(2).unwrap(),
        3,
    );
    let pool = BufferPool::lazy(layout.len, 1);
    FrameLease::single_plane(
        FrameMeta::new(format, timestamp),
        pool.lease(),
        layout.len,
        layout.stride,
    )
}

fn codec_node_options() -> StyxCodecNodeOptions {
    StyxCodecNodeOptions {
        #[cfg(target_os = "linux")]
        shared_output: false,
        #[cfg(target_os = "linux")]
        owned_fallback: true,
    }
}

struct Rg24Encoder {
    descriptor: CodecDescriptor,
}

struct TestExternalBacking {
    data: Arc<[u8]>,
    kind: &'static str,
}

impl crate::core::prelude::ExternalBacking for TestExternalBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        (index == 0).then_some(self.data.as_ref())
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.data.len())
    }

    fn backing_kind(&self) -> &'static str {
        self.kind
    }
}

impl Rg24Encoder {
    fn new() -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input: FourCc::RG24,
                output: FourCc::RG24,
                name: "test-rg24",
                impl_name: "graph-test",
            },
        }
    }
}

impl Codec for Rg24Encoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        if input.meta().format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: input.meta().format.code,
            });
        }
        Ok(input)
    }
}

fn virtual_device() -> ProbedDevice {
    let interval = Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(30).unwrap(),
    };
    let format = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(2, 2).unwrap(),
        ColorSpace::Srgb,
    );
    let mode = Mode {
        id: ModeId {
            format,
            interval: Some(interval),
        },
        format,
        intervals: smallvec::smallvec![interval],
        interval_stepwise: None,
    };
    ProbedDevice {
        identity: DeviceIdentity {
            display: "virtual-graph-source".into(),
            keys: vec!["virtual-graph-source".into()],
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::Virtual,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor {
                modes: vec![mode],
                controls: vec![],
            },
            properties: vec![],
        }],
    }
}

mod core;
mod fanout;
mod sinks;
mod sources;
