//! [`StyxH264Encoder`]: the encoder as a [`Codec`] (frames in, H.264 Annex B packets out).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use styx_core::prelude::*;

use super::{StyxH264, StyxH264Input};
#[cfg(target_os = "linux")]
use crate::shared_packet_frame;
use crate::{
    Codec, CodecDescriptor, CodecError, CodecKind, DEFAULT_CODEC_POOL_CHUNK_BYTES,
    DEFAULT_CODEC_POOL_SPARE,
};

/// H.264 from I420 (YU12), NV12 or GREY (R8) frames of even sizes with Styx's own encoder
/// ([`StyxH264`]). Each packet is one picture; IDR pictures carry SPS and PPS.
pub struct StyxH264Encoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    qp: u8,
    keyframe_interval: u32,
    force_key: AtomicBool,
    /// The encoder and its output, kept between frames.
    state: Mutex<(StyxH264, Vec<u8>)>,
}

impl StyxH264Encoder {
    /// The input formats taken.
    pub const INPUTS: [FourCc; 5] = [
        FourCc::I420,
        FourCc::YU12,
        FourCc::NV12,
        FourCc::GREY,
        FourCc::R8,
    ];
    /// The QP and IDR spacing [`Self::with_defaults`] uses.
    pub const DEFAULT_QP: u8 = 26;
    pub const DEFAULT_KEYFRAME_INTERVAL: u32 = 60;

    pub fn with_defaults(input: FourCc) -> Self {
        Self::new(input, Self::DEFAULT_QP, Self::DEFAULT_KEYFRAME_INTERVAL)
    }

    /// See [`StyxH264::new`] for `qp` and `keyframe_interval`.
    pub fn new(input: FourCc, qp: u8, keyframe_interval: u32) -> Self {
        Self::with_pool(
            input,
            qp,
            keyframe_interval,
            BufferPool::lazy(DEFAULT_CODEC_POOL_CHUNK_BYTES, DEFAULT_CODEC_POOL_SPARE),
        )
    }

    pub fn with_pool(input: FourCc, qp: u8, keyframe_interval: u32, pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input,
                output: FourCc::H264,
                name: "h264",
                impl_name: "styx-h264",
            },
            pool,
            qp,
            keyframe_interval,
            force_key: AtomicBool::new(false),
            state: Mutex::new((StyxH264::new(qp, keyframe_interval), Vec::new())),
        }
    }

    /// Encode `input` and hand the packet and whether it is inter-coded to `emit`.
    fn encode<T>(
        &self,
        input: &FrameLease,
        emit: impl FnOnce(&FrameMeta, &[u8], bool) -> Result<T, CodecError>,
    ) -> Result<T, CodecError> {
        let meta = input.meta();
        if meta.format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: meta.format.code,
            });
        }
        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let planes = input.planes();
        let plane = |i: usize| {
            planes
                .get(i)
                .ok_or_else(|| CodecError::Codec("h264 encoder frame missing plane".into()))
        };
        let first = plane(0)?;
        let ch = height.div_ceil(2);
        let pixels = match self.descriptor.input {
            FourCc::GREY | FourCc::R8 => StyxH264Input::Gray {
                y: first.data(),
                width,
                height,
                stride: first.stride().max(width),
            },
            FourCc::I420 | FourCc::YU12 => {
                let y_stride = first.stride().max(width);
                if planes.len() >= 3 {
                    StyxH264Input::I420 {
                        y: first.data(),
                        u: plane(1)?.data(),
                        v: plane(2)?.data(),
                        width,
                        height,
                        y_stride,
                        c_stride: plane(1)?.stride().max(width.div_ceil(2)),
                    }
                } else {
                    // One plane: Y, then U and V at half the stride.
                    let data = first.data();
                    let c_stride = y_stride.div_ceil(2);
                    let (y, rest) = data.split_at((y_stride * height).min(data.len()));
                    let (u, v) = rest.split_at((c_stride * ch).min(rest.len()));
                    StyxH264Input::I420 {
                        y,
                        u,
                        v,
                        width,
                        height,
                        y_stride,
                        c_stride,
                    }
                }
            }
            FourCc::NV12 => {
                if planes.len() >= 2 {
                    StyxH264Input::Nv12 {
                        y: first.data(),
                        uv: plane(1)?.data(),
                        width,
                        height,
                        y_stride: first.stride().max(width),
                        uv_stride: plane(1)?.stride().max(width),
                    }
                } else {
                    let stride = first.stride().max(width);
                    let data = first.data();
                    let (y, uv) = data.split_at((stride * height).min(data.len()));
                    StyxH264Input::Nv12 {
                        y,
                        uv,
                        width,
                        height,
                        y_stride: stride,
                        uv_stride: stride,
                    }
                }
            }
            other => {
                return Err(CodecError::Codec(format!(
                    "styx-h264 does not encode {other}"
                )));
            }
        };
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (encoder, packet) = &mut *guard;
        if self.force_key.swap(false, Ordering::AcqRel) {
            encoder.force_keyframe();
        }
        let idr = encoder.encode(&pixels, packet)?;
        emit(meta, packet, !idr)
    }
}

impl Codec for StyxH264Encoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn memory_stats(&self) -> Option<BufferPoolStats> {
        Some(self.pool.stats())
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.process_ref(&input)
    }

    fn process_ref(&self, input: &FrameLease) -> Result<FrameLease, CodecError> {
        self.encode(input, |meta, data, delta| {
            let mut buf = self.pool.lease();
            buf.resize(data.len());
            buf.as_mut_slice()[..data.len()].copy_from_slice(data);
            let mut packet = FrameLease::single_plane(
                FrameMeta::new(
                    MediaFormat::new(
                        self.descriptor.output,
                        meta.format.resolution,
                        meta.format.color,
                    ),
                    meta.timestamp,
                )
                .with_residency(FrameResidency::CompressedPacket),
                buf,
                data.len(),
                data.len(),
            );
            packet.meta_mut().delta = delta;
            Ok(packet)
        })
    }

    fn request_keyframe(&self) {
        self.force_key.store(true, Ordering::Release);
    }

    fn new_instance(&self) -> Option<Arc<dyn Codec>> {
        Some(Arc::new(Self::new(
            self.descriptor.input,
            self.qp,
            self.keyframe_interval,
        )))
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        self.encode(input, |meta, data, delta| {
            let mut packet = shared_packet_frame(&self.descriptor, meta, data, pool)?;
            packet.meta_mut().delta = delta;
            Ok(Some(packet))
        })
    }
}
