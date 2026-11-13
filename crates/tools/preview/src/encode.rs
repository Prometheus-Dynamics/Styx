//! One source frame to one published JPEG: the planes are read in place, the JPEG is written
//! straight into a buffer of the producer's ring, and the buffer is published. No copy.

use styx_format::{FourCc, MediaFormat};
use styx_frame::{FrameEntry, FrameLease, Layouts, PlaneLayout};
use styx_jpeg::{JpegEncoder, PlanarInput};
use styx_socket::{BeginError, Producer};

/// Why one frame did not reach the ring.
#[derive(Debug)]
pub(crate) enum Failure {
    /// Every buffer of the output ring is in use: the frame is dropped and counted.
    NoBuffer,
    /// The source frame cannot be encoded (format, planes, readability, or the output buffer
    /// refused the frame).
    Refused,
    /// The encoder failed (the JPEG did not fit the buffer, or the output is not writable).
    Encode,
    /// The ring refused the publish.
    Publish,
}

/// Encodes `frame` and publishes it. Returns the JPEG's length in bytes.
///
/// `capacity` is the payload of one output buffer; a JPEG longer than that is an encode failure.
pub(crate) fn encode_publish<M>(
    producer: &mut Producer,
    encoder: &mut JpegEncoder,
    frame: &FrameLease<M>,
    capacity: usize,
) -> Result<usize, Failure> {
    let format = *frame.format();
    let width = format.resolution.width.get() as usize;
    let height = format.resolution.height.get() as usize;
    let read = frame.read().ok_or(Failure::Refused)?;
    let plane = |index: usize| read.plane(index).ok_or(Failure::Refused);
    let input = match format.code {
        FourCc::I420 | FourCc::YU12 => {
            let (y, u, v) = (plane(0)?, plane(1)?, plane(2)?);
            PlanarInput::I420 {
                y: y.data,
                u: u.data,
                v: v.data,
                width,
                height,
                y_stride: y.stride,
                c_stride: u.stride,
            }
        }
        FourCc::NV12 => {
            let (y, uv) = (plane(0)?, plane(1)?);
            PlanarInput::Nv12 {
                y: y.data,
                uv: uv.data,
                width,
                height,
                y_stride: y.stride,
                uv_stride: uv.stride,
            }
        }
        FourCc::GREY | FourCc::R8 => {
            let y = plane(0)?;
            PlanarInput::Gray {
                y: y.data,
                width,
                height,
                stride: y.stride,
            }
        }
        _ => return Err(Failure::Refused),
    };

    // The buffer is taken for the largest JPEG; the real length is set once it is written.
    let layouts =
        Layouts::new(&[PlaneLayout::new(0, capacity, capacity)]).ok_or(Failure::Refused)?;
    let entry = FrameEntry::new(
        frame.id(),
        frame.timestamp_ns(),
        MediaFormat::new(FourCc::MJPG, format.resolution, format.color, format.range),
        layouts,
    );
    let mut out = producer.begin(entry).map_err(|err| match err {
        BeginError::NoFreeBuffer => Failure::NoBuffer,
        BeginError::Frame(_) => Failure::Refused,
    })?;

    let len = {
        let mut planes = out.write().ok_or(Failure::Encode)?;
        let dst = planes.plane(0).ok_or(Failure::Encode)?;
        encoder
            .encode_fixed(&input, dst.data)
            .map_err(|_| Failure::Encode)?
    };

    let layouts = Layouts::new(&[PlaneLayout::new(0, len, len)]).ok_or(Failure::Encode)?;
    out.set_entry(FrameEntry::new(
        frame.id(),
        frame.timestamp_ns(),
        MediaFormat::new(FourCc::MJPG, format.resolution, format.color, format.range),
        layouts,
    ))
    .map_err(|_| Failure::Encode)?;
    producer.publish(out).map_err(|_| Failure::Publish)?;
    Ok(len)
}
