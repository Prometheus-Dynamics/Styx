//! [`H264Encoder`]: [`StyxH264`] as a `styx-codec-api` [`Encoder`]. A raw frame goes in, one
//! Annex B packet comes out, written into the caller's output lease.

use std::fmt;
use std::marker::PhantomData;

use styx_codec_api::{
    Backend, Candidate, Codec, CodecDescriptor, CodecError, CodecKind, Encoder, Implementation,
};
use styx_format::{FourCc, MediaFormat};
use styx_frame::{FrameEntry, FrameLease, Layouts, PlaneLayout};

use crate::{RateTarget, StyxH264, StyxH264Input};

/// The name the descriptor and the registry log carry.
pub const NAME: &str = "styx-h264";
/// The raw formats the encoder takes: planar and semi-planar 4:2:0, and 8-bit grey.
pub const INPUTS: [FourCc; 5] = [
    FourCc::I420,
    FourCc::YU12,
    FourCc::NV12,
    FourCc::GREY,
    FourCc::R8,
];
/// The QP and IDR spacing [`H264Encoder::with_defaults`] uses.
pub const DEFAULT_QP: u8 = 26;
pub const DEFAULT_KEYFRAME_INTERVAL: u32 = 60;

/// H.264 from one of [`INPUTS`] frames of even sizes, one packet per frame.
///
/// The output lease must have one plane whose length is the packet capacity (for example
/// `Layouts::new(&[PlaneLayout::new(0, 0, capacity)])`); the encoder sets the entry's plane to
/// the packet's length. A packet that does not fit is refused with `CodecError::Rejected`.
///
/// Whether the picture is an IDR picture (a keyframe) is kept on the encoder
/// ([`H264Encoder::last_keyframe`]): the codec traits carry no flag for it.
pub struct H264Encoder<M = ()> {
    descriptor: CodecDescriptor,
    encoder: StyxH264,
    /// The last packet, kept between frames so a steady stream allocates nothing.
    packet: Vec<u8>,
    keyframe: bool,
    marker: PhantomData<fn() -> M>,
}

fn descriptor(input: FourCc) -> CodecDescriptor {
    CodecDescriptor {
        kind: CodecKind::Encoder,
        input,
        output: FourCc::H264,
        backend: Backend::Native,
        name: NAME,
    }
}

impl<M> H264Encoder<M> {
    /// An encoder for `input` frames at `qp` (see [`StyxH264::new`]).
    pub fn new(input: FourCc, qp: u8, keyframe_interval: u32) -> Result<Self, CodecError> {
        if !INPUTS.contains(&input) {
            return Err(CodecError::Rejected(format!(
                "{NAME} does not encode {input}"
            )));
        }
        Ok(Self {
            descriptor: descriptor(input),
            encoder: StyxH264::new(qp, keyframe_interval),
            packet: Vec::new(),
            keyframe: false,
            marker: PhantomData,
        })
    }

    /// [`Self::new`] at [`DEFAULT_QP`] and [`DEFAULT_KEYFRAME_INTERVAL`].
    pub fn with_defaults(input: FourCc) -> Result<Self, CodecError> {
        Self::new(input, DEFAULT_QP, DEFAULT_KEYFRAME_INTERVAL)
    }

    /// Holds `target` from the next frame on, or back to the fixed QP with `None`.
    pub fn set_rate(&mut self, target: Option<RateTarget>) {
        self.encoder.set_rate(target);
    }

    /// Whether motion vectors are refined to quarter pels (on by default).
    pub fn set_subpel(&mut self, on: bool) {
        self.encoder.set_subpel(on);
    }

    /// The next frame is an IDR picture (a keyframe).
    pub fn request_keyframe(&mut self) {
        self.encoder.force_keyframe();
    }

    /// Whether the last frame was an IDR picture.
    pub fn last_keyframe(&self) -> bool {
        self.keyframe
    }

    /// The QP of the last picture.
    pub fn qp(&self) -> u8 {
        self.encoder.qp()
    }
}

impl<M> fmt::Debug for H264Encoder<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("H264Encoder")
            .field("descriptor", &self.descriptor)
            .field("qp", &self.encoder.qp())
            .field("keyframe", &self.keyframe)
            .finish_non_exhaustive()
    }
}

impl<M> Codec<M> for H264Encoder<M> {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl<M> Encoder<M> for H264Encoder<M> {
    fn encode(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        let format = *input.format();
        let code = format.code;
        if code != self.descriptor.input {
            return Err(CodecError::Rejected(format!(
                "{NAME} takes {} frames, got {code}",
                self.descriptor.input
            )));
        }
        let width = format.resolution.width.get() as usize;
        let height = format.resolution.height.get() as usize;
        let read = input
            .read()
            .ok_or_else(|| CodecError::Failed("input frame is not mapped for the CPU".into()))?;
        let plane = |index: usize| {
            read.plane(index)
                .ok_or_else(|| CodecError::Rejected(format!("{code} frame has no plane {index}")))
        };
        let pixels = match code {
            FourCc::GREY | FourCc::R8 => {
                let y = plane(0)?;
                StyxH264Input::Gray {
                    y: y.data,
                    width,
                    height,
                    stride: y.stride.max(width),
                }
            }
            FourCc::I420 | FourCc::YU12 => {
                let (y, u, v) = (plane(0)?, plane(1)?, plane(2)?);
                StyxH264Input::I420 {
                    y: y.data,
                    u: u.data,
                    v: v.data,
                    width,
                    height,
                    y_stride: y.stride.max(width),
                    c_stride: u.stride.max(width.div_ceil(2)),
                }
            }
            FourCc::NV12 => {
                let (y, uv) = (plane(0)?, plane(1)?);
                StyxH264Input::Nv12 {
                    y: y.data,
                    uv: uv.data,
                    width,
                    height,
                    y_stride: y.stride.max(width),
                    uv_stride: uv.stride.max(width),
                }
            }
            other => {
                return Err(CodecError::Rejected(format!(
                    "{NAME} does not encode {other}"
                )));
            }
        };
        let idr = self.encoder.encode(&pixels, &mut self.packet)?;
        drop(read);
        self.keyframe = idr;

        let size = self.packet.len();
        {
            let mut planes = output
                .write()
                .ok_or_else(|| CodecError::Failed("output frame is not writable".into()))?;
            let out = planes
                .plane(0)
                .ok_or_else(|| CodecError::Rejected("output frame has no packet plane".into()))?;
            let capacity = out.data.len();
            let dst = out.data.get_mut(..size).ok_or_else(|| {
                CodecError::Rejected(format!(
                    "a packet of {size} bytes does not fit the output's {capacity}"
                ))
            })?;
            dst.copy_from_slice(&self.packet);
        }
        let layouts = Layouts::new(&[PlaneLayout::new(0, 0, size)])
            .ok_or_else(|| CodecError::Failed("packet layout".into()))?;
        let out_format = MediaFormat::new(
            self.descriptor.output,
            format.resolution,
            format.color,
            format.range,
        );
        let entry = FrameEntry::new(input.id(), input.timestamp_ns(), out_format, layouts);
        output
            .set_entry(entry)
            .map_err(|error| CodecError::Failed(format!("output frame: {error}")))
    }
}

/// One candidate per [`INPUTS`] format, for a `styx-codec-api` registry. Each builds a
/// [`H264Encoder`] at the defaults.
pub fn candidates<M: 'static>() -> Vec<Candidate<M>> {
    INPUTS
        .iter()
        .map(|&input| {
            Candidate::new(descriptor(input), move || {
                let encoder = H264Encoder::<M>::with_defaults(input)?;
                Ok(Implementation::Encoder(Box::new(encoder)))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    use styx_codec_api::{CodecKind, Registry, Request, Verdict};
    use styx_format::{ColorRange, ColorSpace, Resolution};
    use styx_frame::{BufferSize, FramePool, packed_layouts};

    use super::*;

    /// A `w` x `h` I420 picture of a moving square over a gradient.
    fn picture(w: usize, h: usize, shift: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h * 3 / 2);
        for r in 0..h {
            for x in 0..w {
                let inside = (x + shift) % w < w / 3 && r < h / 2;
                out.push(if inside {
                    200
                } else {
                    ((x * 255 / w + r * 64 / h) % 256) as u8
                });
            }
        }
        for plane in 0..2 {
            for r in 0..h / 2 {
                for x in 0..w / 2 {
                    out.push((64 + plane * 100 + (x + r) % 32) as u8);
                }
            }
        }
        out
    }

    fn ffmpeg_decodes(stream: &[u8]) -> Option<Vec<u8>> {
        let spawned = Command::new("ffmpeg")
            .args([
                "-v", "error", "-f", "h264", "-i", "pipe:0", "-f", "rawvideo", "-pix_fmt",
                "yuv420p", "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                eprintln!("skipped the FFmpeg decode check: no ffmpeg binary ({error})");
                return None;
            }
        };
        let mut stdin = child.stdin.take().unwrap();
        let data = stream.to_vec();
        let feeder = std::thread::spawn(move || stdin.write_all(&data));
        let out = child.wait_with_output().unwrap();
        feeder.join().unwrap().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(out.stdout)
    }

    fn raw_frame(pool: &FramePool, w: u32, h: u32, id: u64, pixels: &[u8]) -> FrameLease {
        let layouts = packed_layouts(FourCc::I420, w, h, None, None).unwrap();
        let format = MediaFormat::new(
            FourCc::I420,
            Resolution::new(w, h).unwrap(),
            ColorSpace::Bt709,
            ColorRange::Limited,
        );
        let mut frame = pool
            .acquire(FrameEntry::new(id, id * 33_000_000, format, layouts))
            .unwrap();
        let mut planes = frame.write().unwrap();
        let mut offset = 0;
        for index in 0..3 {
            let mut plane = planes.plane(index).unwrap();
            let (pw, ph) = if index == 0 {
                (w as usize, h as usize)
            } else {
                (w as usize / 2, h as usize / 2)
            };
            for row in 0..ph {
                let src = &pixels[offset + row * pw..][..pw];
                plane.row_bytes_mut(row, pw).unwrap().copy_from_slice(src);
            }
            offset += pw * ph;
        }
        frame
    }

    fn output_pool(capacity: usize) -> FramePool {
        FramePool::host(2, BufferSize::new(capacity, 0)).unwrap()
    }

    fn packet_lease(pool: &FramePool, capacity: usize, id: u64) -> FrameLease {
        let layouts = Layouts::new(&[PlaneLayout::new(0, 0, capacity)]).unwrap();
        let format = MediaFormat::with_defaults(FourCc::H264, Resolution::new(16, 16).unwrap());
        pool.acquire(FrameEntry::new(id, 0, format, layouts))
            .unwrap()
    }

    #[test]
    fn encodes_frames_into_the_output_lease() {
        let (w, h) = (48, 32);
        let pixels_per = w * h * 3 / 2;
        let pool = FramePool::host(1, BufferSize::new(pixels_per, 0)).unwrap();
        let outs = output_pool(64 * 1024);
        let mut encoder = H264Encoder::<()>::new(FourCc::I420, 26, 3).unwrap();
        assert_eq!(encoder.descriptor().backend, Backend::Native);
        assert_eq!(encoder.descriptor().output, FourCc::H264);
        let mut stream = Vec::new();
        let mut keyframes = Vec::new();
        for i in 0..5 {
            let input = raw_frame(&pool, w as u32, h as u32, i, &picture(w, h, 2 * i as usize));
            if i == 1 {
                encoder.request_keyframe();
            }
            let mut out = packet_lease(&outs, 64 * 1024, i);
            encoder.encode(&input, &mut out).unwrap();
            assert_eq!(out.format().code, FourCc::H264);
            assert_eq!(out.timestamp_ns(), input.timestamp_ns());
            let packet = out.read().unwrap().plane(0).unwrap().data.to_vec();
            stream.extend_from_slice(&packet);
            keyframes.push(encoder.last_keyframe());
        }
        // IDR at the start, on request, then every third picture after the request.
        assert_eq!(keyframes, [true, true, false, false, true]);
        if let Some(decoded) = ffmpeg_decodes(&stream) {
            assert_eq!(decoded.len(), 5 * pixels_per);
        }
    }

    #[test]
    fn a_packet_that_does_not_fit_is_refused() {
        let (w, h) = (48, 32);
        let pool = FramePool::host(1, BufferSize::new(w * h * 3 / 2, 0)).unwrap();
        let input = raw_frame(&pool, w as u32, h as u32, 0, &picture(w, h, 0));
        let outs = output_pool(8);
        let mut out = packet_lease(&outs, 8, 0);
        let mut encoder = H264Encoder::<()>::with_defaults(FourCc::I420).unwrap();
        match encoder.encode(&input, &mut out) {
            Err(CodecError::Rejected(why)) => assert!(why.contains("does not fit"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn wrong_input_formats_are_refused() {
        assert!(H264Encoder::<()>::new(FourCc::H264, 26, 1).is_err());
        let mut encoder = H264Encoder::<()>::with_defaults(FourCc::NV12).unwrap();
        let pool = FramePool::host(1, BufferSize::new(48 * 32 * 3 / 2, 0)).unwrap();
        let input = raw_frame(&pool, 48, 32, 0, &picture(48, 32, 0));
        let outs = output_pool(4096);
        let mut out = packet_lease(&outs, 4096, 0);
        assert!(matches!(
            encoder.encode(&input, &mut out),
            Err(CodecError::Rejected(_))
        ));
    }

    #[test]
    fn the_registry_picks_the_native_encoder_first() {
        let mut registry: Registry<()> = Registry::new();
        for candidate in candidates::<()>() {
            registry.register(candidate);
        }
        let selection = registry
            .select(Request::new(CodecKind::Encoder, FourCc::I420, FourCc::H264))
            .expect("selects");
        assert_eq!(
            selection.implementation().descriptor().backend,
            Backend::Native
        );
        assert_eq!(selection.implementation().descriptor().name, NAME);
        assert!(
            selection
                .decisions()
                .iter()
                .any(|d| matches!(d.verdict, Verdict::Chosen))
        );
    }
}
