//! Scalers, crops and format converters on the Rockchip RGA, through Daedalus's `daedalus-rga`
//! (the stack's one RGA binding: mainline `rockchip-rga` over V4L2 mem2mem, or any mem2mem
//! device). [`RgaConverter`] is a [`Codec`] over that engine; Styx has no RGA code of its own.
//!
//! A dma-buf frame at the engine's layout (one dma-buf, offset 0, the planes at the layout's
//! offsets and strides) is read in place. Any other frame is copied into a dma-heap buffer
//! first. Both dma-heap buffers are made once per layout and reused. The result is copied out
//! into a host frame, so nothing the engine owns outlives a call. No rotation yet.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::PathBuf;
use std::sync::Arc;

use daedalus_rga::{Layout, PixelFormat, Rect, Rga};
use parking_lot::Mutex;
use smallvec::{SmallVec, smallvec};
use styx_core::prelude::*;
use styx_kernel::dma_heap::{self, Access, DmaBuf, DmaHeap};

use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

/// How a converter uses the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgaOptions {
    /// The mem2mem device; `None` takes `rockchip-rga` (`Rga::find`). `vim2m` in the tests.
    pub device: Option<PathBuf>,
    /// The part of the source to convert (`None`: the whole frame).
    pub crop: Option<Rect>,
    /// Hand dma-buf frames to the engine in place when their layout is one it reads; `false`
    /// copies every frame into a dma-heap buffer.
    pub import_dmabuf: bool,
}

impl Default for RgaOptions {
    fn default() -> Self {
        Self {
            device: None,
            crop: None,
            import_dmabuf: true,
        }
    }
}

/// The engine's pixel format for a Styx format, if it converts it.
fn pixel_format(code: FourCc) -> Option<PixelFormat> {
    Some(match code {
        FourCc::RG24 => PixelFormat::Rgb24,
        FourCc::BG24 => PixelFormat::Bgr24,
        // B, G, R, A in memory: the engine's `ABGR32`.
        FourCc::BGRA => PixelFormat::Abgr32,
        FourCc::GREY | FourCc::R8 => PixelFormat::Grey,
        FourCc::NV12 => PixelFormat::Nv12,
        FourCc::NV21 => PixelFormat::Nv21,
        FourCc::YU12 | FourCc::I420 => PixelFormat::Yuv420,
        _ => return None,
    })
}

/// The engine's layout of a `(width, height)` frame of Styx format `code`. The 4:2:0 formats
/// need even sides.
fn layout_of(code: FourCc, (width, height): (u32, u32)) -> Option<Layout> {
    let format = pixel_format(code)?;
    let subsampled = matches!(
        format,
        PixelFormat::Nv12 | PixelFormat::Nv21 | PixelFormat::Yuv420
    );
    if width == 0 || height == 0 || (subsampled && (width % 2 != 0 || height % 2 != 0)) {
        return None;
    }
    Some(Layout::packed(format, width, height))
}

/// One plane of a frame in an engine layout: where it starts, its row stride and its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plane {
    offset: usize,
    stride: usize,
    row_bytes: usize,
    rows: usize,
}

/// The planes of `layout`, in the order a Styx frame of its format has them. The chroma planes
/// of 4:2:0 are half the luma stride, and they follow the luma plane in the one buffer.
fn planes(layout: Layout) -> SmallVec<[Plane; 3]> {
    let stride = layout.stride as usize;
    let (width, height) = (layout.width as usize, layout.height as usize);
    let chroma_rows = height.div_ceil(2);
    match layout.format {
        PixelFormat::Nv12 | PixelFormat::Nv21 => smallvec![
            Plane {
                offset: 0,
                stride,
                row_bytes: width,
                rows: height,
            },
            Plane {
                offset: stride * height,
                stride,
                row_bytes: width,
                rows: chroma_rows,
            },
        ],
        PixelFormat::Yuv420 => {
            let (chroma_stride, chroma_width) = (stride / 2, width / 2);
            let u = stride * height;
            let v = u + chroma_stride * chroma_rows;
            smallvec![
                Plane {
                    offset: 0,
                    stride,
                    row_bytes: width,
                    rows: height,
                },
                Plane {
                    offset: u,
                    stride: chroma_stride,
                    row_bytes: chroma_width,
                    rows: chroma_rows,
                },
                Plane {
                    offset: v,
                    stride: chroma_stride,
                    row_bytes: chroma_width,
                    rows: chroma_rows,
                },
            ]
        }
        _ => smallvec![Plane {
            offset: 0,
            stride,
            row_bytes: width * layout.format.bytes() as usize,
            rows: height,
        }],
    }
}

fn codec_err(context: &str, e: impl std::fmt::Display) -> CodecError {
    CodecError::Codec(format!("rga {context}: {e}"))
}

/// The dma-heap buffers of one conversion: the copy-in for host frames and the engine's output.
struct Buffers {
    key: (Layout, Layout, Option<Rect>),
    input: DmaBuf,
    output: DmaBuf,
}

impl Buffers {
    fn new(key: (Layout, Layout, Option<Rect>)) -> Result<Self, CodecError> {
        let heap = DmaHeap::open("system").map_err(|e| codec_err("buffers", e))?;
        let (src, dst, _) = key;
        Ok(Self {
            key,
            input: heap
                .allocate(src.frame_len())
                .map_err(|e| codec_err("buffers", e))?,
            output: heap
                .allocate(dst.frame_len())
                .map_err(|e| codec_err("buffers", e))?,
        })
    }

    /// Copies `input`'s rows into the copy-in buffer at `src`'s layout.
    fn stage(&self, input: &FrameLease, src: Layout) -> Result<(), CodecError> {
        let views = input.planes_visible().map_err(|e| codec_err("input", e))?;
        let geometry = planes(src);
        if views.len() != geometry.len() {
            return Err(codec_err(
                "input",
                format!("{} planes, the layout has {}", views.len(), geometry.len()),
            ));
        }
        self.input
            .begin_cpu_access(Access::Write)
            .map_err(|e| codec_err("input", e))?;
        let mut map = self.input.map().map_err(|e| codec_err("input", e))?;
        let bytes = map.as_mut_slice();
        let mut result = Ok(());
        for (view, g) in views.iter().zip(&geometry) {
            if view.row_bytes() != g.row_bytes || view.len() != g.rows {
                result = Err(codec_err("input", "a plane at another size"));
                break;
            }
            for r in 0..g.rows {
                let Some(row) = view.row(r) else { break };
                let at = g.offset + r * g.stride;
                bytes[at..at + g.row_bytes].copy_from_slice(row.data());
            }
        }
        drop(map);
        self.input
            .end_cpu_access(Access::Write)
            .map_err(|e| codec_err("input", e))?;
        result
    }

    /// Copies the engine's output into `frame`'s planes (`dst`'s layout).
    fn read_out(&self, dst: Layout, frame: &mut FrameLease) -> Result<(), CodecError> {
        let geometry = planes(dst);
        if frame.planes().len() != geometry.len() {
            return Err(codec_err("output", "the frame's planes do not match"));
        }
        self.output
            .begin_cpu_access(Access::Read)
            .map_err(|e| codec_err("output", e))?;
        let map = self.output.map().map_err(|e| codec_err("output", e))?;
        let bytes = map.as_slice();
        let mut result = Ok(());
        for (i, g) in geometry.iter().enumerate() {
            let mut rows = match frame.visible_rows_mut(i) {
                Ok(rows) => rows,
                Err(e) => {
                    result = Err(codec_err("output", e));
                    break;
                }
            };
            if rows.row_bytes() != g.row_bytes || rows.len() != g.rows {
                result = Err(codec_err("output", "a plane at another size"));
                break;
            }
            rows.for_each_row_mut(|r, mut row| {
                let at = g.offset + r * g.stride;
                if let Some(src) = bytes.get(at..at + g.row_bytes) {
                    row.data().copy_from_slice(src);
                }
            });
        }
        drop(map);
        self.output
            .end_cpu_access(Access::Read)
            .map_err(|e| codec_err("output", e))?;
        result
    }
}

#[derive(Default)]
struct State {
    engine: Option<Rga>,
    buffers: Option<Buffers>,
}

/// A hardware scaler, cropper or format converter on the Rockchip RGA: raw frames in, raw frames
/// of another size or format out (the same size and format is a copy, through the engine).
pub struct RgaConverter {
    descriptor: CodecDescriptor,
    options: RgaOptions,
    size: Option<(u32, u32)>,
    state: Mutex<State>,
}

impl RgaConverter {
    /// A converter from `input` frames to `output` frames of `size` (`None` keeps each frame's
    /// size). The engine opens on the first frame.
    pub fn new(
        input: FourCc,
        output: FourCc,
        size: Option<(u32, u32)>,
        options: RgaOptions,
    ) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Converter,
                input,
                output,
                name: "rga-convert",
                impl_name: "rga",
            },
            options,
            size,
            state: Mutex::new(State::default()),
        }
    }

    /// Whether the RGA here (`rockchip-rga`) converts `input` frames to `output`, as far as the
    /// formats go: the device's own formats are only known when a frame opens it.
    pub fn available(input: FourCc, output: FourCc) -> bool {
        // The RGA is a block of the SoC: look for its node once.
        static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        pixel_format(input).is_some()
            && pixel_format(output).is_some()
            && *PRESENT.get_or_init(|| daedalus_rga::find_m2m_device("rockchip-rga").is_some())
    }

    /// The formats the registry offers conversions between (every pair of two different ones).
    pub const FORMATS: [FourCc; 7] = [
        FourCc::NV12,
        FourCc::NV21,
        FourCc::YU12,
        FourCc::RG24,
        FourCc::BG24,
        FourCc::BGRA,
        FourCc::GREY,
    ];

    /// The source's dma-buf when the engine can read `input` in place at `src`'s layout: one
    /// dma-buf from offset 0, each plane at its offset and stride, the buffer holding the frame.
    fn in_place<'a>(&self, input: &'a FrameLease, src: Layout) -> Option<BorrowedFd<'a>> {
        if !self.options.import_dmabuf {
            return None;
        }
        let first = input.dmabuf_plane(0)?;
        if first.offset != 0 {
            return None;
        }
        let views = input.planes_visible().ok()?;
        let geometry = planes(src);
        if views.len() != geometry.len() {
            return None;
        }
        let id = dma_heap::dmabuf_id(first.fd);
        for (i, (view, g)) in views.iter().zip(&geometry).enumerate() {
            let at = input.dmabuf_plane(i)?;
            let same = at.fd.as_raw_fd() == first.fd.as_raw_fd()
                || (id.is_some() && dma_heap::dmabuf_id(at.fd) == id);
            if !same
                || at.offset != g.offset
                || view.stride() != g.stride
                || view.row_bytes() != g.row_bytes
                || view.len() != g.rows
            {
                return None;
            }
        }
        let size = dma_heap::dmabuf_size(first.fd).ok()?;
        (size >= src.frame_len() as u64).then_some(first.fd)
    }
}

impl Codec for RgaConverter {
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
        let src = layout_of(self.descriptor.input, (width, height))
            .ok_or_else(|| codec_err("input", format!("{} at {width}x{height}", format.code)))?;
        let out_size = self.size.unwrap_or((width, height));
        let dst = layout_of(self.descriptor.output, out_size).ok_or_else(|| {
            codec_err(
                "output",
                format!(
                    "{} at {}x{}",
                    self.descriptor.output, out_size.0, out_size.1
                ),
            )
        })?;

        let mut guard = self.state.lock();
        let state = &mut *guard;
        if state.engine.is_none() {
            state.engine = Some(match &self.options.device {
                Some(path) => Rga::on_device(path),
                None => Rga::find().ok_or_else(|| codec_err("open", "no rockchip-rga device"))?,
            });
        }
        let key = (src, dst, self.options.crop);
        if !state.buffers.as_ref().is_some_and(|b| b.key == key) {
            state.buffers = Some(Buffers::new(key)?);
        }
        let (Some(engine), Some(buffers)) = (state.engine.as_mut(), state.buffers.as_ref()) else {
            return Err(codec_err("open", "no engine or buffers"));
        };

        let src_fd = match self.in_place(input, src) {
            Some(fd) => fd,
            None => {
                buffers.stage(input, src)?;
                buffers.input.as_fd()
            }
        };
        engine
            .blit(
                src,
                src_fd,
                src.frame_len(),
                self.options.crop,
                dst,
                buffers.output.as_fd(),
                dst.frame_len(),
            )
            .map_err(|e| codec_err("convert", e))?;

        let resolution = Resolution::new(out_size.0, out_size.1)
            .ok_or_else(|| codec_err("output", "an empty frame"))?;
        let media = MediaFormat::new(self.descriptor.output, resolution, format.color);
        let mut out = FrameLease::allocate_host_owned(media, input.meta().timestamp)
            .map_err(|e| codec_err("output", e))?;
        buffers.read_out(dst, &mut out)?;
        let m = out.meta_mut();
        let source = input.meta();
        m.capture_instant = source.capture_instant;
        m.timing = source.timing;
        m.clock = source.clock;
        m.backend = source.backend.clone();
        Ok(out)
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

#[cfg(test)]
#[allow(clippy::print_stderr)]
mod tests {
    use super::*;

    fn frame(code: FourCc, width: u32, height: u32, seed: usize) -> FrameLease {
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
                    *b = (x + r * 3 + i * 40 + seed) as u8;
                }
            });
        }
        frame
    }

    /// The rows of every plane of `frame`, concatenated.
    fn bytes_of(frame: &FrameLease) -> Vec<u8> {
        let mut out = Vec::new();
        for plane in frame.planes_visible().unwrap().iter() {
            for r in 0..plane.len() {
                out.extend_from_slice(plane.row(r).unwrap().data());
            }
        }
        out
    }

    /// `input` in a system dma-heap buffer, at the frame's own (packed) layout.
    fn dmabuf_frame(heap: &DmaHeap, input: &FrameLease) -> FrameLease {
        let len = input.meta().format.resolution.width.get() as usize
            * input.meta().format.resolution.height.get() as usize
            * 3;
        let buf = heap.allocate(len).unwrap();
        buf.begin_cpu_access(Access::Write).unwrap();
        {
            let mut map = buf.map().unwrap();
            map.as_mut_slice()[..len].copy_from_slice(&bytes_of(input));
        }
        buf.end_cpu_access(Access::Write).unwrap();
        let format = input.meta().format;
        let stride = format.resolution.width.get() as usize * 3;
        let layouts = smallvec![PlaneLayout {
            offset: 0,
            len,
            stride,
        }];
        let fds = vec![FrameFdPlane {
            fd: buf.into_fd(),
            offset: 0,
            len,
        }];
        FrameLease::from_dmabuf(FrameMeta::new(format, 7), layouts, fds).unwrap()
    }

    fn vim2m() -> Option<PathBuf> {
        let path = daedalus_rga::find_m2m_device("vim2m");
        if path.is_none() {
            eprintln!("skipped: no vim2m (modprobe vim2m)");
        }
        path
    }

    #[test]
    fn styx_formats_map_to_engine_layouts() {
        let rgb = layout_of(FourCc::RG24, (64, 48)).unwrap();
        assert_eq!(rgb, Layout::packed(PixelFormat::Rgb24, 64, 48));
        assert_eq!(
            layout_of(FourCc::BGRA, (64, 48)),
            Some(Layout::packed(PixelFormat::Abgr32, 64, 48))
        );
        assert_eq!(
            layout_of(FourCc::R8, (8, 4)),
            Some(Layout::packed(PixelFormat::Grey, 8, 4))
        );
        assert_eq!(
            layout_of(FourCc::I420, (64, 48)),
            layout_of(FourCc::YU12, (64, 48))
        );
        assert!(layout_of(FourCc::YUYV, (64, 48)).is_none());
        assert!(layout_of(FourCc::NV12, (63, 48)).is_none());
        assert!(layout_of(FourCc::RG24, (0, 48)).is_none());
    }

    #[test]
    fn plane_geometry_covers_each_layout_exactly() {
        for (code, size) in [
            (FourCc::RG24, (64, 48)),
            (FourCc::NV12, (64, 48)),
            (FourCc::NV21, (10, 6)),
            (FourCc::I420, (10, 6)),
            (FourCc::I420, (62, 46)),
        ] {
            let layout = layout_of(code, size).unwrap();
            let geometry = planes(layout);
            let end = geometry
                .iter()
                .map(|p| p.offset + (p.rows - 1) * p.stride + p.row_bytes)
                .max()
                .unwrap();
            assert_eq!(end, layout.frame_len(), "{code} {size:?}");
            for p in &geometry {
                assert!(p.row_bytes <= p.stride, "{code} {size:?}");
            }
        }
    }

    #[test]
    fn a_missing_device_is_an_error_not_a_panic() {
        let options = RgaOptions {
            device: Some("/nonexistent/video99".into()),
            ..Default::default()
        };
        let codec = RgaConverter::new(FourCc::RG24, FourCc::BG24, None, options);
        assert!(matches!(
            codec.process_ref(&frame(FourCc::RG24, 64, 48, 0)),
            Err(CodecError::Codec(_))
        ));
        assert!(matches!(
            codec.process_ref(&frame(FourCc::NV12, 64, 48, 0)),
            Err(CodecError::FormatMismatch { .. })
        ));
    }

    /// vim2m: RGB frames convert to BGR (exactly the CPU's channel swap) and come back the same
    /// through the same-format copy, at the same size with their metadata. A pair vim2m does not
    /// take is skipped. Skipped without vim2m.
    #[test]
    fn vim2m_converts_host_frames_like_the_cpu() {
        let Some(path) = vim2m() else { return };
        let engine = || Rga::on_device(&path);
        let (rgb, bgr) = (FourCc::RG24, FourCc::BG24);
        let input = frame(rgb, 64, 48, 5);
        let src = layout_of(rgb, (64, 48)).unwrap();
        let dst = layout_of(bgr, (64, 48)).unwrap();
        if engine().frame_lens(src, dst).is_err() {
            eprintln!("skipped: vim2m does not take RG24 to BG24");
            return;
        }
        let swapped: Vec<u8> = bytes_of(&input)
            .chunks(3)
            .flat_map(|px| [px[2], px[1], px[0]])
            .collect();
        let converter = RgaConverter::new(
            rgb,
            bgr,
            None,
            RgaOptions {
                device: Some(path.clone()),
                ..Default::default()
            },
        );
        for _ in 0..3 {
            let out = converter.process_ref(&input).unwrap();
            assert_eq!(out.meta().format.code, bgr);
            assert_eq!(out.meta().timestamp, 7);
            assert_eq!(out.meta().format.resolution, input.meta().format.resolution);
            assert_eq!(bytes_of(&out), swapped);
        }
        let copy = RgaConverter::new(
            rgb,
            rgb,
            None,
            RgaOptions {
                device: Some(path),
                ..Default::default()
            },
        );
        assert_eq!(
            bytes_of(&copy.process_ref(&input).unwrap()),
            bytes_of(&input)
        );
    }

    /// vim2m scales: the output has the requested size and carries the source's content.
    #[test]
    fn vim2m_scales_frames() {
        let Some(path) = vim2m() else { return };
        let rgb = FourCc::RG24;
        // vim2m's smallest frame is 32 rows high.
        let src = layout_of(rgb, (64, 48)).unwrap();
        let dst = layout_of(rgb, (64, 32)).unwrap();
        if let Err(e) = Rga::on_device(&path).frame_lens(src, dst) {
            eprintln!("skipped: vim2m does not scale RG24 ({e})");
            return;
        }
        let converter = RgaConverter::new(
            rgb,
            rgb,
            Some((64, 32)),
            RgaOptions {
                device: Some(path),
                ..Default::default()
            },
        );
        let out = converter.process_ref(&frame(rgb, 64, 48, 0)).unwrap();
        assert_eq!(
            out.meta().format.resolution,
            Resolution::new(64, 32).unwrap()
        );
        assert!(bytes_of(&out).iter().any(|&b| b != 0));
    }

    /// vim2m reads a dma-buf frame at its own layout in place, and the output is the same as
    /// the host frame's. Skipped without vim2m or the system dma-heap.
    #[test]
    fn vim2m_reads_dmabuf_frames_in_place() {
        let Some(path) = vim2m() else { return };
        let Ok(heap) = DmaHeap::open("system") else {
            eprintln!("skipped: no system dma-heap");
            return;
        };
        let rgb = FourCc::RG24;
        let host = frame(rgb, 64, 48, 9);
        let input = dmabuf_frame(&heap, &host);
        let converter = RgaConverter::new(
            rgb,
            rgb,
            None,
            RgaOptions {
                device: Some(path),
                ..Default::default()
            },
        );
        let src = layout_of(rgb, (64, 48)).unwrap();
        assert!(converter.in_place(&input, src).is_some());
        let out = converter.process_ref(&input).unwrap();
        assert_eq!(bytes_of(&out), bytes_of(&host));
    }
}
