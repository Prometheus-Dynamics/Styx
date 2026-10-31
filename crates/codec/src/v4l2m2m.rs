//! Codecs on V4L2 memory-to-memory devices, driven directly through `styx-kernel` (no FFmpeg):
//! stateful encoders ([`V4l2M2mEncoder`]: H.264, HEVC, JPEG and VP8 hardware encoders, the
//! K230's VPU, vicodec) and scalers or converters ([`V4l2M2mConverter`]: the Rockchip RGA, the
//! K230's 2D unit, vim2m).
//!
//! Each codec opens its device on the first frame and reopens it when the frame size changes.
//! Frames are copied into the device's buffers at its strides; results are copied out into
//! host frames, so nothing the device owns outlives a call.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use styx_core::prelude::*;
use styx_kernel::v4l2::{
    BufType, CapabilityFlags, ControlValue, Controls, Format, ImageRows, M2mFrame, M2mSession,
    PixFormat, PixFormatMplane, PlaneFormat, VideoDevice,
};

use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

/// `V4L2_CID_MPEG_VIDEO_BITRATE`.
const CID_BITRATE: u32 = 0x0099_09cf;
/// `V4L2_CID_MPEG_VIDEO_GOP_SIZE`.
const CID_GOP_SIZE: u32 = 0x0099_09cb;
/// `V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME`.
const CID_FORCE_KEY_FRAME: u32 = 0x0099_09e5;

/// How a memory-to-memory codec uses its device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V4l2M2mOptions {
    /// The device node; `None` takes the first one that converts between the two formats.
    pub device: Option<PathBuf>,
    /// Buffers on each side of the device.
    pub buffers: u32,
    /// Encoders: the target bit rate in bits per second (`None` keeps the driver's).
    pub bitrate: Option<u32>,
    /// Encoders: frames from one keyframe to the next (`None` keeps the driver's).
    pub gop: Option<u32>,
    /// How long to wait for the device's result before reporting backpressure.
    pub timeout: Duration,
}

impl Default for V4l2M2mOptions {
    fn default() -> Self {
        Self {
            device: None,
            buffers: 4,
            bitrate: None,
            gop: None,
            timeout: Duration::from_secs(1),
        }
    }
}

/// The V4L2 codes for a Styx format, most likely first.
fn v4l2_codes(code: FourCc) -> Vec<u32> {
    let alias: &[&[u8; 4]] = match &code.to_u32().to_le_bytes() {
        b"RG24" => &[b"RGB3"],
        b"BG24" => &[b"BGR3"],
        b"I420" => &[b"YU12"],
        b"H265" => &[b"HEVC"],
        b"MJPG" => &[b"JPEG"],
        b"JPEG" => &[b"MJPG"],
        _ => &[],
    };
    std::iter::once(code.to_u32())
        .chain(alias.iter().map(|c| u32::from_le_bytes(**c)))
        .collect()
}

/// Whether the device is a memory-to-memory device, and multi-planar.
fn m2m_kind(dev: &VideoDevice) -> Option<bool> {
    let caps = dev.capabilities().device_caps;
    if caps.contains(CapabilityFlags::VIDEO_M2M_MPLANE) {
        Some(true)
    } else if caps.contains(CapabilityFlags::VIDEO_M2M) {
        Some(false)
    } else {
        None
    }
}

/// The V4L2 codes `dev` takes for `source` and gives for `dest`, if it converts between them.
fn device_codes(dev: &VideoDevice, source: FourCc, dest: FourCc) -> Option<(u32, u32)> {
    let mplane = m2m_kind(dev)?;
    let (out, cap) = if mplane {
        (BufType::VideoOutputMplane, BufType::VideoCaptureMplane)
    } else {
        (BufType::VideoOutput, BufType::VideoCapture)
    };
    let pick = |buf_type, code| {
        let have: Vec<u32> = dev
            .formats(buf_type)
            .ok()?
            .iter()
            .map(|f| f.fourcc.0)
            .collect();
        v4l2_codes(code).into_iter().find(|c| have.contains(c))
    };
    Some((pick(out, source)?, pick(cap, dest)?))
}

/// The first memory-to-memory device turning `source` frames into `dest` frames, with the
/// V4L2 codes it uses for them.
pub fn find_device(source: FourCc, dest: FourCc) -> Option<(PathBuf, u32, u32)> {
    styx_kernel::v4l2::list_video_nodes()
        .into_iter()
        .find_map(|path| {
            let dev = VideoDevice::open(&path).ok()?;
            let (s, d) = device_codes(&dev, source, dest)?;
            Some((path, s, d))
        })
}

fn codec_err(context: &str, e: impl std::fmt::Display) -> CodecError {
    CodecError::Codec(format!("v4l2m2m {context}: {e}"))
}

fn format_for(mplane: bool, code: u32, width: u32, height: u32, size_hint: u32) -> Format {
    let fourcc = styx_kernel::FourCc(code);
    if mplane {
        Format::Multi(PixFormatMplane {
            width,
            height,
            fourcc,
            field: 1,
            planes: if size_hint > 0 {
                vec![PlaneFormat {
                    bytes_per_line: 0,
                    size_image: size_hint,
                }]
            } else {
                Vec::new()
            },
            ..Default::default()
        })
    } else {
        Format::Single(PixFormat {
            width,
            height,
            fourcc,
            field: 1,
            size_image: size_hint,
            ..Default::default()
        })
    }
}

/// An open stream and the frames in flight through it.
struct Stream {
    session: M2mSession,
    width: u32,
    height: u32,
    /// The next source timestamp, which identifies a frame on its way through the device.
    next: u64,
    /// Metadata of the frames queued, by source timestamp.
    pending: VecDeque<(Duration, FrameMeta)>,
    /// Results taken from the device and not returned yet.
    ready: VecDeque<FrameLease>,
}

impl Stream {
    /// Opens `source` (`width` x `height` frames of `source`) to `dest` (`out_width` x
    /// `out_height` of `dest`), after `setup` sets the device's controls.
    #[allow(clippy::too_many_arguments)]
    fn open(
        options: &V4l2M2mOptions,
        source: FourCc,
        dest: FourCc,
        (width, height): (u32, u32),
        (out_width, out_height): (u32, u32),
        size_hint: u32,
        setup: impl FnOnce(&VideoDevice),
    ) -> Result<Self, CodecError> {
        let (dev, codes) = match &options.device {
            Some(path) => {
                let dev = VideoDevice::open(path).map_err(|e| codec_err("open", e))?;
                let codes = device_codes(&dev, source, dest).ok_or_else(|| {
                    codec_err(
                        "open",
                        format!("{} does not turn {source} into {dest}", path.display()),
                    )
                })?;
                (dev, codes)
            }
            None => {
                let (path, s, d) = find_device(source, dest).ok_or_else(|| {
                    codec_err("open", format!("no device turns {source} into {dest}"))
                })?;
                (
                    VideoDevice::open(&path).map_err(|e| codec_err("open", e))?,
                    (s, d),
                )
            }
        };
        let mplane = m2m_kind(&dev).unwrap_or(false);
        setup(&dev);
        let session = M2mSession::open(
            dev,
            &format_for(mplane, codes.0, width, height, 0),
            &format_for(mplane, codes.1, out_width, out_height, size_hint),
            options.buffers,
        )
        .map_err(|e| codec_err("open", e))?;
        Ok(Self {
            session,
            width,
            height,
            next: 0,
            pending: VecDeque::new(),
            ready: VecDeque::new(),
        })
    }

    /// Queues `input`, first taking results off the device (with `take`) until a source buffer
    /// is free, then waits up to `timeout` for a result. The oldest result, or backpressure
    /// when the device still holds every frame.
    fn run(
        &mut self,
        input: &FrameLease,
        timeout: Duration,
        mut take: impl FnMut(&M2mSession, &M2mFrame, FrameMeta) -> Result<FrameLease, CodecError>,
    ) -> Result<FrameLease, CodecError> {
        let planes = input.planes_visible().map_err(|e| codec_err("input", e))?;
        let rows: Vec<ImageRows<'_>> = planes
            .iter()
            .map(|p| ImageRows {
                data: p.as_bytes(),
                stride: p.stride(),
                row_bytes: p.row_bytes(),
                rows: p.len(),
            })
            .collect();
        while !self.session.can_push().map_err(|e| codec_err("queue", e))? {
            if !self.collect(timeout, &mut take)? {
                return Err(codec_err("queue", "the device returned no buffer"));
            }
        }
        let timestamp = Duration::from_micros(self.next);
        self.next += 1;
        if !self
            .session
            .push_image(&rows, timestamp)
            .map_err(|e| codec_err("queue", e))?
        {
            return Err(CodecError::Backpressure);
        }
        self.pending.push_back((timestamp, input.meta().clone()));
        if self.ready.is_empty() {
            self.collect(timeout, &mut take)?;
        }
        self.ready.pop_front().ok_or(CodecError::Backpressure)
    }

    /// Takes one result off the device within `timeout`; `false` if none came.
    fn collect(
        &mut self,
        timeout: Duration,
        take: &mut impl FnMut(&M2mSession, &M2mFrame, FrameMeta) -> Result<FrameLease, CodecError>,
    ) -> Result<bool, CodecError> {
        let Some(frame) = self
            .session
            .pull(timeout)
            .map_err(|e| codec_err("dequeue", e))?
        else {
            return Ok(false);
        };
        // The source frame's metadata, dropping that of frames the device skipped.
        let stamp = frame.buffer.timestamp;
        let mut meta = None;
        while let Some((t, m)) = self.pending.front() {
            if *t > stamp {
                break;
            }
            if *t == stamp {
                meta = Some(m.clone());
            }
            self.pending.pop_front();
        }
        let result = match meta {
            Some(meta)
                if !frame.is_error() && frame.buffer.planes.first().is_some_and(|p| p.0 > 0) =>
            {
                Some(take(&self.session, &frame, meta))
            }
            _ => None,
        };
        self.session
            .release(frame)
            .map_err(|e| codec_err("requeue", e))?;
        if let Some(result) = result {
            self.ready.push_back(result?);
        }
        Ok(true)
    }
}

/// A stateful hardware encoder: raw frames in, one compressed packet out per frame.
pub struct V4l2M2mEncoder {
    descriptor: CodecDescriptor,
    options: V4l2M2mOptions,
    stream: Mutex<Option<Stream>>,
    force_key: AtomicBool,
    pool: BufferPool,
}

impl V4l2M2mEncoder {
    /// An encoder from `input` frames to `output` packets (`H264`, `H265`, `MJPG`, `VP80`,
    /// `FWHT`, ...). The device opens on the first frame.
    pub fn new(input: FourCc, output: FourCc, options: V4l2M2mOptions) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input,
                output,
                name: "v4l2m2m",
                impl_name: "v4l2m2m",
            },
            options,
            stream: Mutex::new(None),
            force_key: AtomicBool::new(false),
            pool: BufferPool::lazy(
                crate::DEFAULT_CODEC_POOL_CHUNK_BYTES,
                crate::DEFAULT_CODEC_POOL_SPARE,
            ),
        }
    }

    /// Whether a device here encodes `input` frames to `output`.
    pub fn available(input: FourCc, output: FourCc) -> bool {
        find_device(input, output).is_some()
    }

    fn inter_coded(&self) -> bool {
        !matches!(
            &self.descriptor.output.to_u32().to_le_bytes(),
            b"MJPG" | b"JPEG"
        )
    }

    fn set_controls(&self, dev: &VideoDevice) {
        // Drivers without a control keep their own setting.
        if let Some(bitrate) = self.options.bitrate {
            let _ = dev.set_control(
                CID_BITRATE,
                ControlValue::Integer(bitrate.min(i32::MAX as u32) as i32),
            );
        }
        if let Some(gop) = self.options.gop {
            let _ = dev.set_control(
                CID_GOP_SIZE,
                ControlValue::Integer(gop.min(i32::MAX as u32) as i32),
            );
        }
    }
}

impl Codec for V4l2M2mEncoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.process_ref(&input)
    }

    fn process_ref(&self, input: &FrameLease) -> Result<FrameLease, CodecError> {
        let format = input.meta().format;
        if format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: format.code,
            });
        }
        let (width, height) = (
            format.resolution.width.get(),
            format.resolution.height.get(),
        );
        let mut guard = self.stream.lock();
        if guard
            .as_ref()
            .is_none_or(|s| (s.width, s.height) != (width, height))
        {
            if let Some(old) = guard.take() {
                let _ = old.session.close();
            }
            // Room for a keyframe well above the raw 4:2:0 size.
            let hint = (width * height * 3 / 2).max(64 * 1024);
            *guard = Some(Stream::open(
                &self.options,
                self.descriptor.input,
                self.descriptor.output,
                (width, height),
                (width, height),
                hint,
                |dev| self.set_controls(dev),
            )?);
        }
        let stream = guard.as_mut().expect("opened above");
        if self.force_key.swap(false, Ordering::AcqRel) {
            let _ = stream
                .session
                .device()
                .set_control(CID_FORCE_KEY_FRAME, ControlValue::Integer(1));
        }
        let (output, inter) = (self.descriptor.output, self.inter_coded());
        let pool = &self.pool;
        stream.run(input, self.options.timeout, |session, frame, meta| {
            let data = session.data(frame, 0);
            let mut buf = pool.lease();
            buf.replace_owned(data.to_vec());
            let mut packet = FrameLease::single_plane(
                FrameMeta::new(
                    MediaFormat::new(output, meta.format.resolution, meta.format.color),
                    meta.timestamp,
                )
                .with_residency(FrameResidency::CompressedPacket),
                buf,
                data.len(),
                data.len(),
            );
            packet.meta_mut().delta = inter && !frame.is_keyframe();
            Ok(packet)
        })
    }

    fn memory_stats(&self) -> Option<BufferPoolStats> {
        Some(self.pool.stats())
    }

    fn request_keyframe(&self) {
        self.force_key.store(true, Ordering::Release);
    }

    fn new_instance(&self) -> Option<Arc<dyn Codec>> {
        Some(Arc::new(Self::new(
            self.descriptor.input,
            self.descriptor.output,
            self.options.clone(),
        )))
    }
}

/// A hardware scaler or format converter: raw frames in, raw frames of another size or
/// format out.
pub struct V4l2M2mConverter {
    descriptor: CodecDescriptor,
    options: V4l2M2mOptions,
    size: Option<(u32, u32)>,
    stream: Mutex<Option<Stream>>,
}

impl V4l2M2mConverter {
    /// A converter from `input` frames to `output` frames of `size` (`None` keeps each
    /// frame's size).
    pub fn new(
        input: FourCc,
        output: FourCc,
        size: Option<(u32, u32)>,
        options: V4l2M2mOptions,
    ) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input,
                output,
                name: "v4l2m2m-convert",
                impl_name: "v4l2m2m",
            },
            options,
            size,
            stream: Mutex::new(None),
        }
    }

    /// Whether a device here converts `input` frames to `output`.
    pub fn available(input: FourCc, output: FourCc) -> bool {
        find_device(input, output).is_some()
    }

    /// The device node in use, once a frame opened it.
    pub fn device_path(&self) -> Option<PathBuf> {
        self.options
            .device
            .clone()
            .or_else(|| find_device(self.descriptor.input, self.descriptor.output).map(|(p, ..)| p))
    }
}

impl Codec for V4l2M2mConverter {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.process_ref(&input)
    }

    fn process_ref(&self, input: &FrameLease) -> Result<FrameLease, CodecError> {
        let format = input.meta().format;
        if format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: format.code,
            });
        }
        let (width, height) = (
            format.resolution.width.get(),
            format.resolution.height.get(),
        );
        let mut guard = self.stream.lock();
        if guard
            .as_ref()
            .is_none_or(|s| (s.width, s.height) != (width, height))
        {
            if let Some(old) = guard.take() {
                let _ = old.session.close();
            }
            *guard = Some(Stream::open(
                &self.options,
                self.descriptor.input,
                self.descriptor.output,
                (width, height),
                self.size.unwrap_or((width, height)),
                0,
                |_| {},
            )?);
        }
        let stream = guard.as_mut().expect("opened above");
        let output = self.descriptor.output;
        stream.run(input, self.options.timeout, |session, frame, meta| {
            let dest = session.dest_format();
            let (w, h) = match dest {
                Format::Single(p) => (p.width, p.height),
                Format::Multi(m) => (m.width, m.height),
                Format::Meta(_) => (0, 0),
            };
            let resolution = Resolution::new(w, h)
                .ok_or_else(|| codec_err("output", format!("a {w}x{h} frame")))?;
            let format = MediaFormat::new(output, resolution, meta.format.color);
            let mut out = FrameLease::allocate_host_owned(format, meta.timestamp)
                .map_err(|e| codec_err("output", e))?;
            let count = out.planes().len();
            let mut shape = Vec::with_capacity(count);
            for i in 0..count {
                let rows = out
                    .visible_rows_mut(i)
                    .map_err(|e| codec_err("output", e))?;
                shape.push((rows.row_bytes(), rows.len()));
            }
            let planes = session.image_planes(frame, &shape);
            for (i, (data, stride)) in planes.into_iter().enumerate() {
                let mut rows = out
                    .visible_rows_mut(i)
                    .map_err(|e| codec_err("output", e))?;
                let row_bytes = rows.row_bytes();
                rows.for_each_row_mut(|r, mut row| {
                    if let Some(src) = data.get(r * stride..r * stride + row_bytes) {
                        row.data().copy_from_slice(src);
                    }
                });
            }
            let m = out.meta_mut();
            m.capture_instant = meta.capture_instant;
            m.timing = meta.timing;
            m.clock = meta.clock;
            m.backend = meta.backend;
            Ok(out)
        })
    }

    fn new_instance(&self) -> Option<Arc<dyn Codec>> {
        Some(Arc::new(Self::new(
            self.descriptor.input,
            self.descriptor.output,
            self.size,
            self.options.clone(),
        )))
    }
}

/// Whether `path` is a memory-to-memory device node.
pub fn is_m2m_device(path: &Path) -> bool {
    VideoDevice::open(path)
        .ok()
        .as_ref()
        .and_then(m2m_kind)
        .is_some()
}

#[cfg(test)]
#[allow(clippy::print_stderr)]
mod tests {
    use super::*;

    fn gradient(code: FourCc, width: u32, height: u32) -> FrameLease {
        let format = MediaFormat::new(
            code,
            Resolution::new(width, height).unwrap(),
            ColorSpace::Srgb,
        );
        let mut frame = FrameLease::allocate_host_owned(format, 7).unwrap();
        let count = frame.planes().len();
        for i in 0..count {
            let mut rows = frame.visible_rows_mut(i).unwrap();
            rows.for_each_row_mut(|r, mut row| {
                for (x, b) in row.data().iter_mut().enumerate() {
                    *b = (x + r * 3 + i * 40) as u8;
                }
            });
        }
        frame
    }

    #[test]
    fn styx_codes_map_to_v4l2_codes() {
        let code = |c: &[u8; 4]| u32::from_le_bytes(*c);
        assert_eq!(v4l2_codes(FourCc::RG24), [code(b"RG24"), code(b"RGB3")]);
        assert_eq!(v4l2_codes(FourCc::H265), [code(b"H265"), code(b"HEVC")]);
        assert_eq!(v4l2_codes(FourCc::NV12), [code(b"NV12")]);
    }

    #[test]
    fn a_missing_device_is_an_error_not_a_panic() {
        let options = V4l2M2mOptions {
            device: Some("/nonexistent/video99".into()),
            ..Default::default()
        };
        let codec = V4l2M2mEncoder::new(FourCc::NV12, FourCc::H264, options);
        assert!(matches!(
            codec.process_ref(&gradient(FourCc::NV12, 64, 48)),
            Err(CodecError::Codec(_))
        ));
        assert!(matches!(
            codec.process_ref(&gradient(FourCc::YUYV, 64, 48)),
            Err(CodecError::FormatMismatch { .. })
        ));
    }

    /// vim2m (`modprobe vim2m`): RGB frames through the converter come back the same, at the
    /// same size, with their metadata. Skipped without the module.
    #[test]
    fn vim2m_converts_frames() {
        let rgb = FourCc::RGB3;
        if !V4l2M2mConverter::available(rgb, rgb) {
            eprintln!("skipped: no vim2m (modprobe vim2m)");
            return;
        }
        let codec = V4l2M2mConverter::new(rgb, rgb, None, V4l2M2mOptions::default());
        let input = gradient(rgb, 64, 48);
        for _ in 0..3 {
            let out = codec.process_ref(&input).unwrap();
            assert_eq!(out.meta().format.code, rgb);
            assert_eq!(out.meta().timestamp, 7);
            assert_eq!(out.meta().format.resolution, input.meta().format.resolution);
        }
    }

    /// vicodec (`modprobe vicodec`): YU12 frames encode to FWHT packets, the first a keyframe.
    /// Skipped without the module.
    #[test]
    fn vicodec_encodes_frames() {
        let fwht = FourCc::new(*b"FWHT");
        if !V4l2M2mEncoder::available(FourCc::YU12, fwht) {
            eprintln!("skipped: no vicodec (modprobe vicodec)");
            return;
        }
        let codec = V4l2M2mEncoder::new(FourCc::YU12, fwht, V4l2M2mOptions::default());
        let input = gradient(FourCc::YU12, 64, 48);
        let first = codec.process_ref(&input).unwrap();
        assert_eq!(first.meta().format.code, fwht);
        assert!(!first.meta().delta);
        assert!(!first.planes()[0].data().is_empty());
        for _ in 0..3 {
            codec.process_ref(&input).unwrap();
        }
    }
}
