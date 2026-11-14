//! The hardware converter: scaling through Lemnos's `image.scaler/1` contract.
//!
//! A chip with an image scaler (the RGA, the G2D) serves a job stream. This converter opens it
//! writable, finds the formats it takes, and for each frame: writes the input image into buffer
//! [`IN_BUFFER`] of the stream, posts a job naming that buffer and [`OUT_BUFFER`], takes the
//! completion frame, and copies its bytes into the output frame. One job is in flight at a time,
//! so the two buffers are never shared with another job.
//!
//! The job's images are packed: each plane's rows at their visible width, one plane after
//! another, as [`layout::packed`] lays them out. Both sides of a job use the same format, so the
//! only thing a job changes is the size, and only the formats the scaler lists are offered.
//!
//! Bytes cross the ring twice (into the input buffer, out of the completion buffer); [`Stats`]
//! counts them. The output frame is written straight from the completion buffer, so nothing is
//! allocated per frame.

use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::Duration;

use lemnos_contract_image_scaler::{
    Filter as ScalerFilter, ImageRef, ImageScaler, JOB_BYTES, MAX_STREAM_NAME, ScaleJob,
};
use lemnos_core::{Encode, Writer};
use lemnos_linux_stream::LinuxStreams;
use styx_codec_api::{Backend, Codec, CodecDescriptor, CodecError, CodecKind, Converter};
use styx_format::{FourCc, PlaneGeometry};
use styx_frame::{CpuRead, FrameLease};
use styx_lemnos::{Client, FrameStream};

use crate::layout::{self, Filter};

/// The tag of a device that scales images.
pub const IMAGE_SCALER_TAG: &str = "image.scaler/1";

/// The name the hardware converter is registered and reported under.
pub const NAME: &str = "lemnos-image-scaler";

/// The stream buffer the input image is written to.
pub const IN_BUFFER: u32 = 0;
/// The stream buffer the job's output is written to, and its completion is published on.
pub const OUT_BUFFER: u32 = 1;

/// How long a job may take before the converter gives up on its completion.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(2);

/// What the converter has moved, since it was opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Jobs whose completion was copied out.
    pub jobs: u64,
    /// Bytes written into the input buffer.
    pub staged_bytes: u64,
    /// Bytes copied from the completion buffer into the output frame.
    pub copied_bytes: u64,
    /// Jobs the scaler's job stream refused at the post (its queue was full): nothing was queued,
    /// so the scaler never ran them. Not the stream's Lemnos `refused` stat, which counts only
    /// jobs that starved for a driver-picked buffer; this converter names both buffers itself.
    pub posts_refused: u64,
}

/// The descriptor of the hardware converter for `code` to `code`.
pub fn descriptor(code: FourCc) -> CodecDescriptor {
    CodecDescriptor {
        kind: CodecKind::Converter,
        input: code,
        output: code,
        backend: Backend::Native,
        name: NAME,
    }
}

/// A converter on the image scaler of a Lemnos service, for one format.
pub struct Lemnos {
    descriptor: CodecDescriptor,
    /// Kept alive for the stream's session.
    _client: Client,
    stream: FrameStream,
    max_width: usize,
    max_height: usize,
    identity: u64,
    stats: Stats,
}

impl Lemnos {
    /// Connects to the Lemnos service at `socket` as `client_name`, finds the first device
    /// providing `image.scaler/1`, and opens its job stream for `code`.
    ///
    /// Unavailable when there is no service, no scaler, the scaler does not take `code`, or its
    /// stream has fewer than two buffers. Nothing is started until the first frame.
    pub fn open(
        socket: impl AsRef<FsPath>,
        client_name: &str,
        code: FourCc,
    ) -> Result<Self, CodecError> {
        let client = styx_lemnos::connect(socket, client_name).map_err(unavailable)?;
        let entry = client
            .find(IMAGE_SCALER_TAG)
            .map_err(unavailable)?
            .ok_or_else(|| {
                CodecError::Unavailable(format!("no device provides {IMAGE_SCALER_TAG}"))
            })?;
        let mut scaler = client
            .open::<dyn ImageScaler>(&entry.name)
            .map_err(unavailable)?;
        let info = ImageScaler::info(&mut scaler).map_err(status)?;
        let want = code.to_u32();
        let count = ImageScaler::format_count(&mut scaler).map_err(status)?;
        let mut takes = false;
        for index in 0..count {
            if ImageScaler::format(&mut scaler, index).map_err(status)? == want {
                takes = true;
                break;
            }
        }
        if !takes {
            return Err(CodecError::Unavailable(format!(
                "{} does not take {code}",
                entry.name
            )));
        }
        let mut name = [0u8; MAX_STREAM_NAME];
        let len = ImageScaler::job_stream(&mut scaler, &mut name).map_err(status)?;
        let stream_name = std::str::from_utf8(&name[..len])
            .map_err(|_| CodecError::Unavailable("the job stream name is not UTF-8".to_string()))?;
        drop(scaler);
        let mut stream = client
            .open_stream_writable(entry.index, stream_name, Arc::new(LinuxStreams))
            .map_err(unavailable)?;
        // Two buffers: one for the input image, one for the output. Writing nothing to buffer 1
        // is refused when the stream has no such buffer.
        if stream.write(OUT_BUFFER, 0, &[]).is_err() {
            return Err(CodecError::Unavailable(format!(
                "the job stream of {} has fewer than two buffers",
                entry.name
            )));
        }
        Ok(Self {
            descriptor: descriptor(code),
            _client: client,
            stream,
            max_width: info.max_width as usize,
            max_height: info.max_height as usize,
            identity: 0,
            stats: Stats::default(),
        })
    }

    /// Completions this converter's reader was too late for (`FrameStream::skipped`). It reads each
    /// completion of the one job it has in flight, so this stays 0.
    pub fn skipped(&self) -> u64 {
        self.stream.skipped()
    }

    /// What the converter has moved so far.
    pub fn stats(&self) -> Stats {
        self.stats
    }
}

fn unavailable(error: impl std::fmt::Display) -> CodecError {
    CodecError::Unavailable(error.to_string())
}

fn status(error: impl std::fmt::Debug) -> CodecError {
    CodecError::Failed(format!("image.scaler call: {error:?}"))
}

fn failed(context: &str, error: impl std::fmt::Display) -> CodecError {
    CodecError::Failed(format!("{context}: {error}"))
}

fn image(buffer: u32, width: usize, height: usize, code: FourCc, stride: usize) -> ImageRef {
    ImageRef {
        buffer,
        width: width as u32,
        height: height as u32,
        fourcc: code.to_u32(),
        stride: stride as u32,
    }
}

impl std::fmt::Debug for Lemnos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lemnos")
            .field("descriptor", &self.descriptor)
            .field("max", &(self.max_width, self.max_height))
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl<M> Codec<M> for Lemnos {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

/// The planes of a packed image of `code`, `width` x `height`.
fn geometry(
    code: FourCc,
    width: usize,
    height: usize,
) -> Result<styx_format::Planes<PlaneGeometry>, CodecError> {
    layout::packed(code, width, height).map_err(|e| CodecError::Rejected(e.to_string()))
}

/// Bytes of a packed image: all planes, rows at their visible width.
fn packed_len(planes: &[PlaneGeometry]) -> usize {
    planes.iter().map(|p| p.row_bytes * p.rows).sum()
}

fn plane_of<'a, M>(
    read: &CpuRead<'a, M>,
    index: usize,
) -> Result<styx_frame::Plane<'a>, CodecError> {
    read.plane(index)
        .ok_or_else(|| CodecError::Rejected(format!("the frame has no plane {index}")))
}

impl<M> Converter<M> for Lemnos {
    fn convert(
        &mut self,
        input: &FrameLease<M>,
        output: &mut FrameLease<M>,
    ) -> Result<(), CodecError> {
        let code = self.descriptor.input;
        let (src, dst) = (input.format(), output.format());
        if src.code != code || dst.code != code {
            return Err(CodecError::Rejected(format!(
                "this converter takes {code} to {code}, got {} to {}",
                src.code, dst.code
            )));
        }
        let (sw, sh) = (
            src.resolution.width.get() as usize,
            src.resolution.height.get() as usize,
        );
        let (dw, dh) = (
            dst.resolution.width.get() as usize,
            dst.resolution.height.get() as usize,
        );
        if !layout::fits(code, sw, sh) || !layout::fits(code, dw, dh) {
            return Err(CodecError::Rejected(format!(
                "{sw}x{sh} to {dw}x{dh}: sides do not suit {code}"
            )));
        }
        if sw.max(dw) > self.max_width || sh.max(dh) > self.max_height {
            return Err(CodecError::Rejected(format!(
                "{sw}x{sh} to {dw}x{dh} is past the scaler's {}x{}",
                self.max_width, self.max_height
            )));
        }
        let in_planes = geometry(code, sw, sh)?;
        let out_planes = geometry(code, dw, dh)?;
        let in_stride = in_planes[0].row_bytes;
        let out_stride = out_planes[0].row_bytes;
        let out_len = packed_len(&out_planes);
        let filter = match layout::filter_for(sw, sh, dw, dh) {
            Filter::Area => ScalerFilter::Area,
            Filter::Bilinear => ScalerFilter::Bilinear,
        };

        // Stage the input image into the input buffer, plane by plane, row by row.
        let read = input
            .read()
            .ok_or_else(|| CodecError::Failed("the input frame is not readable".to_string()))?;
        let mut offset = 0usize;
        for (index, g) in in_planes.iter().enumerate() {
            let p = plane_of(&read, index)?;
            for r in 0..g.rows {
                let row = p.row_bytes(r, g.row_bytes).ok_or_else(|| {
                    CodecError::Rejected(format!("input plane {index} is too small"))
                })?;
                self.stream
                    .write(IN_BUFFER, offset + r * g.row_bytes, row)
                    .map_err(|e| {
                        CodecError::Rejected(format!(
                            "the input image does not fit a stream buffer: {e}"
                        ))
                    })?;
            }
            offset += g.row_bytes * g.rows;
        }
        drop(read);
        let staged = offset as u64;

        // Post the job, then take its completion.
        self.identity = self.identity.wrapping_add(1).max(1);
        let identity = self.identity;
        let job = ScaleJob {
            identity,
            input: image(IN_BUFFER, sw, sh, code, in_stride),
            output: image(OUT_BUFFER, dw, dh, code, out_stride),
            filter,
        };
        let mut encoded = [0u8; JOB_BYTES];
        let mut writer = Writer::new(&mut encoded);
        job.encode(&mut writer).map_err(status)?;
        let used = writer.len();
        if !self
            .stream
            .post_job(&encoded[..used])
            .map_err(|e| failed("post job", e))?
        {
            self.stats.posts_refused += 1;
            return Err(CodecError::Failed(
                "the scaler's job queue is full".to_string(),
            ));
        }
        loop {
            let Some(frame) = self
                .stream
                .next_frame(JOB_TIMEOUT)
                .map_err(|e| failed("completion", e))?
            else {
                return Err(CodecError::Failed(format!(
                    "no completion within {} s",
                    JOB_TIMEOUT.as_secs()
                )));
            };
            if frame.identity() != identity {
                // A completion of a job this converter gave up on: give its buffer back and look on.
                self.stream
                    .release(frame)
                    .map_err(|e| failed("release", e))?;
                continue;
            }
            if frame.is_empty() {
                self.stream
                    .release(frame)
                    .map_err(|e| failed("release", e))?;
                return Err(CodecError::Failed("the scaler failed the job".to_string()));
            }
            if frame.len() != out_len {
                let got = frame.len();
                self.stream
                    .release(frame)
                    .map_err(|e| failed("release", e))?;
                return Err(CodecError::Failed(format!(
                    "completion of {got} bytes, expected {out_len}"
                )));
            }
            let bytes = self.stream.bytes(&frame);
            let mut out = output.write().ok_or_else(|| {
                CodecError::Failed("the output frame is not writable".to_string())
            })?;
            let mut at = 0usize;
            for (index, g) in out_planes.iter().enumerate() {
                let mut plane = out.plane(index).ok_or_else(|| {
                    CodecError::Rejected(format!("the output has no plane {index}"))
                })?;
                for r in 0..g.rows {
                    let dst_row = plane.row_bytes_mut(r, g.row_bytes).ok_or_else(|| {
                        CodecError::Rejected(format!("output plane {index} is too small"))
                    })?;
                    dst_row.copy_from_slice(&bytes[at + r * g.row_bytes..][..g.row_bytes]);
                }
                at += g.row_bytes * g.rows;
            }
            self.stream
                .release(frame)
                .map_err(|e| failed("release", e))?;
            self.stats.jobs += 1;
            self.stats.staged_bytes += staged;
            self.stats.copied_bytes += out_len as u64;
            return Ok(());
        }
    }
}
