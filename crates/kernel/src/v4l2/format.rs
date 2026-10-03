//! Formats: enumeration of pixel formats, frame sizes and intervals; get/set/try format;
//! streaming parameters (frame interval); selections.

use std::os::fd::AsFd;

use super::raw::{self, zeroed};
use super::{BufType, VideoDevice};
use crate::flags::flags;
use crate::ioctl::{self, cstr_field};
use crate::{Error, FourCc, Fraction, Rect, Result};

flags! {
    /// Format description flags (`V4L2_FMT_FLAG_*`).
    pub struct FormatFlags: u32 {
        const COMPRESSED = 0x0001;
        const EMULATED = 0x0002;
        const CONTINUOUS_BYTESTREAM = 0x0004;
        const DYN_RESOLUTION = 0x0008;
        const ENC_CAP_FRAME_INTERVAL = 0x0010;
        const CSC_COLORSPACE = 0x0020;
        const CSC_XFER_FUNC = 0x0040;
        const CSC_YCBCR_ENC = 0x0080;
        const CSC_QUANTIZATION = 0x0100;
        const META_LINE_BASED = 0x0200;
    }
}

/// A format a queue supports (`VIDIOC_ENUM_FMT`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatDesc {
    /// Enumeration index.
    pub index: u32,
    /// The queue it applies to.
    pub buf_type: BufType,
    /// Flags.
    pub flags: FormatFlags,
    /// Human-readable description.
    pub description: String,
    /// The pixel (or metadata) format.
    pub fourcc: FourCc,
    /// The media bus code this format was enumerated for (0 when not filtered).
    pub mbus_code: u32,
}

/// A discrete frame size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameSize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// A range of frame sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StepwiseSize {
    /// Minimum width.
    pub min_width: u32,
    /// Maximum width.
    pub max_width: u32,
    /// Width step (1 for continuous ranges).
    pub step_width: u32,
    /// Minimum height.
    pub min_height: u32,
    /// Maximum height.
    pub max_height: u32,
    /// Height step (1 for continuous ranges).
    pub step_height: u32,
}

/// The frame sizes a format supports (`VIDIOC_ENUM_FRAMESIZES`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameSizes {
    /// A list of sizes.
    Discrete(Vec<FrameSize>),
    /// Any size in a range, in steps.
    Stepwise(StepwiseSize),
    /// Any size in a range.
    Continuous(StepwiseSize),
}

/// The frame intervals a format and size support (`VIDIOC_ENUM_FRAMEINTERVALS`), in seconds
/// per frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameIntervals {
    /// A list of intervals.
    Discrete(Vec<Fraction>),
    /// Any interval in a range, in steps.
    Stepwise {
        /// Shortest interval (highest rate).
        min: Fraction,
        /// Longest interval.
        max: Fraction,
        /// Step.
        step: Fraction,
    },
    /// Any interval in a range.
    Continuous {
        /// Shortest interval (highest rate).
        min: Fraction,
        /// Longest interval.
        max: Fraction,
    },
}

/// A single-planar pixel format (`struct v4l2_pix_format`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PixFormat {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format.
    pub fourcc: FourCc,
    /// `enum v4l2_field` (1 = progressive).
    pub field: u32,
    /// Line stride in bytes (0 lets the driver choose).
    pub bytes_per_line: u32,
    /// Buffer size in bytes (set by the driver).
    pub size_image: u32,
    /// `enum v4l2_colorspace`.
    pub colorspace: u32,
    /// `V4L2_PIX_FMT_FLAG_*`.
    pub flags: u32,
    /// `enum v4l2_ycbcr_encoding` (or HSV encoding).
    pub ycbcr_enc: u32,
    /// `enum v4l2_quantization`.
    pub quantization: u32,
    /// `enum v4l2_xfer_func`.
    pub xfer_func: u32,
}

/// One plane of a multi-planar format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaneFormat {
    /// Line stride in bytes.
    pub bytes_per_line: u32,
    /// Plane size in bytes.
    pub size_image: u32,
}

/// A multi-planar pixel format (`struct v4l2_pix_format_mplane`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PixFormatMplane {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format.
    pub fourcc: FourCc,
    /// `enum v4l2_field` (1 = progressive).
    pub field: u32,
    /// `enum v4l2_colorspace`.
    pub colorspace: u32,
    /// The planes (at most 8).
    pub planes: Vec<PlaneFormat>,
    /// `V4L2_PIX_FMT_FLAG_*`.
    pub flags: u8,
    /// `enum v4l2_ycbcr_encoding`.
    pub ycbcr_enc: u8,
    /// `enum v4l2_quantization`.
    pub quantization: u8,
    /// `enum v4l2_xfer_func`.
    pub xfer_func: u8,
}

/// A metadata format (`struct v4l2_meta_format`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetaFormat {
    /// Metadata format.
    pub fourcc: FourCc,
    /// Buffer size in bytes.
    pub buffer_size: u32,
    /// Width (line-based metadata only).
    pub width: u32,
    /// Height (line-based metadata only).
    pub height: u32,
    /// Line stride (line-based metadata only).
    pub bytes_per_line: u32,
}

/// A queue's format. The variant must match the queue type: `Single` for
/// `VideoCapture`/`VideoOutput`, `Multi` for the `*Mplane` types, `Meta` for metadata queues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Format {
    /// Single-planar video.
    Single(PixFormat),
    /// Multi-planar video.
    Multi(PixFormatMplane),
    /// Metadata.
    Meta(MetaFormat),
}

impl Format {
    /// Width and height (0×0 for non-line-based metadata).
    pub fn size(&self) -> FrameSize {
        let (width, height) = match self {
            Format::Single(p) => (p.width, p.height),
            Format::Multi(p) => (p.width, p.height),
            Format::Meta(m) => (m.width, m.height),
        };
        FrameSize { width, height }
    }

    /// The pixel or metadata format.
    pub fn fourcc(&self) -> FourCc {
        match self {
            Format::Single(p) => p.fourcc,
            Format::Multi(p) => p.fourcc,
            Format::Meta(m) => m.fourcc,
        }
    }

    fn from_raw(raw: &raw::v4l2_format) -> Result<Self> {
        let ty = BufType::from_raw(raw.type_)
            .ok_or_else(|| Error::Invalid(format!("unknown buffer type {}", raw.type_)))?;
        Ok(if ty.is_multiplanar() {
            // SAFETY: the kernel filled the member selected by `type_`; it is plain integers.
            let p = unsafe { raw.fmt.pix_mp };
            let n = usize::from(p.num_planes).min(raw::VIDEO_MAX_PLANES);
            let plane_fmt = p.plane_fmt;
            Format::Multi(PixFormatMplane {
                width: p.width,
                height: p.height,
                fourcc: FourCc(p.pixelformat),
                field: p.field,
                colorspace: p.colorspace,
                planes: plane_fmt[..n]
                    .iter()
                    .map(|pl| PlaneFormat {
                        bytes_per_line: pl.bytesperline,
                        size_image: pl.sizeimage,
                    })
                    .collect(),
                flags: p.flags,
                ycbcr_enc: p.ycbcr_enc,
                quantization: p.quantization,
                xfer_func: p.xfer_func,
            })
        } else if ty.is_meta() {
            // SAFETY: as above, the metadata member.
            let m = unsafe { raw.fmt.meta };
            Format::Meta(MetaFormat {
                fourcc: FourCc(m.dataformat),
                buffer_size: m.buffersize,
                width: m.width,
                height: m.height,
                bytes_per_line: m.bytesperline,
            })
        } else {
            // SAFETY: as above, the single-planar member.
            let p = unsafe { raw.fmt.pix };
            Format::Single(PixFormat {
                width: p.width,
                height: p.height,
                fourcc: FourCc(p.pixelformat),
                field: p.field,
                bytes_per_line: p.bytesperline,
                size_image: p.sizeimage,
                colorspace: p.colorspace,
                flags: p.flags,
                ycbcr_enc: p.ycbcr_enc,
                quantization: p.quantization,
                xfer_func: p.xfer_func,
            })
        })
    }

    fn to_raw(&self, ty: BufType) -> Result<raw::v4l2_format> {
        let mut raw: raw::v4l2_format = zeroed();
        raw.type_ = ty.to_raw();
        match self {
            Format::Single(p) if !ty.is_multiplanar() && !ty.is_meta() => {
                raw.fmt.pix = raw::v4l2_pix_format {
                    width: p.width,
                    height: p.height,
                    pixelformat: p.fourcc.0,
                    field: p.field,
                    bytesperline: p.bytes_per_line,
                    sizeimage: p.size_image,
                    colorspace: p.colorspace,
                    priv_: 0,
                    flags: p.flags,
                    ycbcr_enc: p.ycbcr_enc,
                    quantization: p.quantization,
                    xfer_func: p.xfer_func,
                };
            }
            Format::Multi(p) if ty.is_multiplanar() => {
                if p.planes.len() > raw::VIDEO_MAX_PLANES {
                    return Err(Error::Invalid(format!("{} planes (max 8)", p.planes.len())));
                }
                let mut mp = raw::v4l2_pix_format_mplane {
                    width: p.width,
                    height: p.height,
                    pixelformat: p.fourcc.0,
                    field: p.field,
                    colorspace: p.colorspace,
                    num_planes: p.planes.len() as u8,
                    flags: p.flags,
                    ycbcr_enc: p.ycbcr_enc,
                    quantization: p.quantization,
                    xfer_func: p.xfer_func,
                    ..Default::default()
                };
                let mut planes = [raw::v4l2_plane_pix_format::default(); raw::VIDEO_MAX_PLANES];
                for (dst, src) in planes.iter_mut().zip(&p.planes) {
                    dst.bytesperline = src.bytes_per_line;
                    dst.sizeimage = src.size_image;
                }
                mp.plane_fmt = planes;
                raw.fmt.pix_mp = mp;
            }
            Format::Meta(m) if ty.is_meta() => {
                raw.fmt.meta = raw::v4l2_meta_format {
                    dataformat: m.fourcc.0,
                    buffersize: m.buffer_size,
                    width: m.width,
                    height: m.height,
                    bytesperline: m.bytes_per_line,
                };
            }
            _ => {
                return Err(Error::Invalid(format!(
                    "format variant does not match queue type {ty:?}"
                )));
            }
        }
        Ok(raw)
    }
}

/// Streaming parameters of a queue (`VIDIOC_G_PARM`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamParams {
    /// `V4L2_CAP_TIMEPERFRAME` (0x1000) when the frame interval can be set.
    pub capability: u32,
    /// Capture/output mode (`V4L2_MODE_HIGHQUALITY`).
    pub mode: u32,
    /// The frame interval in seconds per frame.
    pub time_per_frame: Fraction,
    /// Driver-specific.
    pub extended_mode: u32,
    /// Number of buffers for read()/write() I/O.
    pub rw_buffers: u32,
}

impl StreamParams {
    /// True when the driver lets userspace set the frame interval.
    pub fn supports_time_per_frame(&self) -> bool {
        self.capability & 0x1000 != 0
    }
}

/// Selection targets (`V4L2_SEL_TGT_*`), shared by video nodes and subdevices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum SelectionTarget {
    /// Current crop rectangle.
    Crop = 0x0000,
    /// Default crop rectangle.
    CropDefault = 0x0001,
    /// Crop bounds.
    CropBounds = 0x0002,
    /// Native size of the device (e.g. a sensor's pixel array).
    NativeSize = 0x0003,
    /// Current compose rectangle.
    Compose = 0x0100,
    /// Default compose rectangle.
    ComposeDefault = 0x0101,
    /// Compose bounds.
    ComposeBounds = 0x0102,
    /// Compose rectangle including padding.
    ComposePadded = 0x0103,
}

impl VideoDevice {
    /// Enumerates the formats of a queue (`VIDIOC_ENUM_FMT`).
    pub fn formats(&self, buf_type: BufType) -> Result<Vec<FormatDesc>> {
        self.formats_for_mbus_code(buf_type, 0)
    }

    /// Enumerates the formats of a queue that can be produced from media bus code `mbus_code`
    /// (only meaningful for media-controller-centric nodes; 0 means all).
    pub fn formats_for_mbus_code(
        &self,
        buf_type: BufType,
        mbus_code: u32,
    ) -> Result<Vec<FormatDesc>> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut raw: raw::v4l2_fmtdesc = zeroed();
            raw.index = index;
            raw.type_ = buf_type.to_raw();
            raw.mbus_code = mbus_code;
            // SAFETY: VIDIOC_ENUM_FMT takes a `v4l2_fmtdesc`.
            match unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_ENUM_FMT, &mut raw) } {
                Ok(_) => out.push(FormatDesc {
                    index,
                    buf_type,
                    flags: FormatFlags(raw.flags),
                    description: cstr_field(&raw.description),
                    fourcc: FourCc(raw.pixelformat),
                    mbus_code: raw.mbus_code,
                }),
                Err(e) if e.is_invalid_argument() => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Enumerates the frame sizes of a pixel format (`VIDIOC_ENUM_FRAMESIZES`).
    pub fn frame_sizes(&self, fourcc: FourCc) -> Result<FrameSizes> {
        let mut discrete = Vec::new();
        for index in 0.. {
            let mut raw: raw::v4l2_frmsizeenum = zeroed();
            raw.index = index;
            raw.pixel_format = fourcc.0;
            // SAFETY: VIDIOC_ENUM_FRAMESIZES takes a `v4l2_frmsizeenum`.
            match unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_ENUM_FRAMESIZES, &mut raw) } {
                Ok(_) => {}
                Err(e) if e.is_invalid_argument() && index > 0 => break,
                Err(e) => return Err(e),
            }
            let u = raw.u;
            let range = StepwiseSize {
                min_width: u[0],
                max_width: u[1],
                step_width: u[2],
                min_height: u[3],
                max_height: u[4],
                step_height: u[5],
            };
            match raw.type_ {
                1 => discrete.push(FrameSize {
                    width: u[0],
                    height: u[1],
                }),
                2 => return Ok(FrameSizes::Continuous(range)),
                3 => return Ok(FrameSizes::Stepwise(range)),
                t => return Err(Error::Invalid(format!("unknown frame size type {t}"))),
            }
        }
        Ok(FrameSizes::Discrete(discrete))
    }

    /// Enumerates the frame intervals of a pixel format at a size
    /// (`VIDIOC_ENUM_FRAMEINTERVALS`).
    pub fn frame_intervals(
        &self,
        fourcc: FourCc,
        width: u32,
        height: u32,
    ) -> Result<FrameIntervals> {
        let mut discrete = Vec::new();
        for index in 0.. {
            let mut raw: raw::v4l2_frmivalenum = zeroed();
            raw.index = index;
            raw.pixel_format = fourcc.0;
            raw.width = width;
            raw.height = height;
            // SAFETY: VIDIOC_ENUM_FRAMEINTERVALS takes a `v4l2_frmivalenum`.
            match unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_ENUM_FRAMEINTERVALS, &mut raw) } {
                Ok(_) => {}
                Err(e) if e.is_invalid_argument() && index > 0 => break,
                Err(e) => return Err(e),
            }
            let [a, b, c] = raw.u;
            match raw.type_ {
                1 => discrete.push(a),
                2 => return Ok(FrameIntervals::Continuous { min: a, max: b }),
                3 => {
                    return Ok(FrameIntervals::Stepwise {
                        min: a,
                        max: b,
                        step: c,
                    });
                }
                t => return Err(Error::Invalid(format!("unknown frame interval type {t}"))),
            }
        }
        Ok(FrameIntervals::Discrete(discrete))
    }

    /// The current format of a queue (`VIDIOC_G_FMT`).
    pub fn format(&self, buf_type: BufType) -> Result<Format> {
        let mut raw: raw::v4l2_format = zeroed();
        raw.type_ = buf_type.to_raw();
        // SAFETY: VIDIOC_G_FMT takes a `v4l2_format`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_G_FMT, &mut raw)? };
        Format::from_raw(&raw)
    }

    /// Sets the format of a queue (`VIDIOC_S_FMT`); returns what the driver chose.
    pub fn set_format(&self, buf_type: BufType, format: &Format) -> Result<Format> {
        let mut raw = format.to_raw(buf_type)?;
        // SAFETY: VIDIOC_S_FMT takes a `v4l2_format`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_S_FMT, &mut raw)? };
        Format::from_raw(&raw)
    }

    /// Asks what the driver would choose for a format, without changing anything
    /// (`VIDIOC_TRY_FMT`).
    pub fn try_format(&self, buf_type: BufType, format: &Format) -> Result<Format> {
        let mut raw = format.to_raw(buf_type)?;
        // SAFETY: VIDIOC_TRY_FMT takes a `v4l2_format`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_TRY_FMT, &mut raw)? };
        Format::from_raw(&raw)
    }

    /// The streaming parameters of a queue (`VIDIOC_G_PARM`).
    pub fn stream_params(&self, buf_type: BufType) -> Result<StreamParams> {
        let mut raw: raw::v4l2_streamparm = zeroed();
        raw.type_ = buf_type.to_raw();
        // SAFETY: VIDIOC_G_PARM takes a `v4l2_streamparm`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_G_PARM, &mut raw)? };
        // SAFETY: capture and output parameters share one plain-integer layout.
        let p = unsafe { raw.parm.capture };
        Ok(StreamParams {
            capability: p.capability,
            mode: p.capturemode,
            time_per_frame: p.timeperframe,
            extended_mode: p.extendedmode,
            rw_buffers: p.readbuffers,
        })
    }

    /// Sets the frame interval of a queue (`VIDIOC_S_PARM`); returns the interval the driver
    /// chose.
    pub fn set_frame_interval(&self, buf_type: BufType, interval: Fraction) -> Result<Fraction> {
        let mut raw: raw::v4l2_streamparm = zeroed();
        raw.type_ = buf_type.to_raw();
        raw.parm.capture = raw::v4l2_captureparm {
            timeperframe: interval,
            ..Default::default()
        };
        // SAFETY: VIDIOC_S_PARM takes a `v4l2_streamparm`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_S_PARM, &mut raw)? };
        // SAFETY: the kernel wrote back the capture/output parameters (plain integers).
        Ok(unsafe { raw.parm.capture }.timeperframe)
    }

    /// Reads a selection rectangle (`VIDIOC_G_SELECTION`).
    pub fn selection(&self, buf_type: BufType, target: SelectionTarget) -> Result<Rect> {
        let mut raw = raw::v4l2_selection {
            type_: buf_type.to_raw(),
            target: target as u32,
            ..Default::default()
        };
        // SAFETY: VIDIOC_G_SELECTION takes a `v4l2_selection`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_G_SELECTION, &mut raw)? };
        Ok(raw.r)
    }

    /// Sets a selection rectangle (`VIDIOC_S_SELECTION`) with `V4L2_SEL_FLAG_*` flags; returns
    /// the rectangle the driver chose.
    pub fn set_selection(
        &self,
        buf_type: BufType,
        target: SelectionTarget,
        rect: Rect,
        flags: u32,
    ) -> Result<Rect> {
        let mut raw = raw::v4l2_selection {
            type_: buf_type.to_raw(),
            target: target as u32,
            flags,
            r: rect,
            ..Default::default()
        };
        // SAFETY: VIDIOC_S_SELECTION takes a `v4l2_selection`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_S_SELECTION, &mut raw)? };
        Ok(raw.r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiplanar_format_round_trips_through_raw() {
        let fmt = Format::Multi(PixFormatMplane {
            width: 1920,
            height: 1080,
            fourcc: FourCc::NV12,
            field: 1,
            planes: vec![
                PlaneFormat {
                    bytes_per_line: 1920,
                    size_image: 1920 * 1080,
                },
                PlaneFormat {
                    bytes_per_line: 1920,
                    size_image: 1920 * 540,
                },
            ],
            ..Default::default()
        });
        let raw = fmt.to_raw(BufType::VideoCaptureMplane).unwrap();
        assert_eq!(Format::from_raw(&raw).unwrap(), fmt);
        assert!(fmt.to_raw(BufType::VideoCapture).is_err());
    }

    #[test]
    fn single_and_meta_formats_round_trip() {
        let single = Format::Single(PixFormat {
            width: 640,
            height: 480,
            fourcc: FourCc::YUYV,
            field: 1,
            bytes_per_line: 1280,
            size_image: 614_400,
            ..Default::default()
        });
        let raw = single.to_raw(BufType::VideoCapture).unwrap();
        assert_eq!(Format::from_raw(&raw).unwrap(), single);
        let meta = Format::Meta(MetaFormat {
            fourcc: FourCc::new(b"RPFS"),
            buffer_size: 4096,
            ..Default::default()
        });
        let raw = meta.to_raw(BufType::MetaCapture).unwrap();
        assert_eq!(Format::from_raw(&raw).unwrap(), meta);
    }
}
