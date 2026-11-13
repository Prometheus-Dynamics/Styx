//! The encoder on frames: I420, NV12 and grey frames from a pool in, MJPG frames out, through
//! `styx-codec-api`'s `Encoder` and `Registry`.

use styx_codec_api::{Backend, CodecError, CodecKind, Encoder, Implementation, Registry, Request};
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{
    BufferSize, FrameEntry, FrameLease, FramePool, Layouts, PlaneLayout, packed_layouts,
};
use styx_jpeg::{INPUTS, JpegEncoder, PlanarInput, StyxJpegEncoder, candidate};

/// A pool of `count` frames of `code` at `w` x `h`, with rows aligned to `stride_alignment`.
fn pool(
    code: FourCc,
    w: u32,
    h: u32,
    stride_alignment: Option<usize>,
    count: usize,
) -> (FramePool, Layouts) {
    let layouts = packed_layouts(code, w, h, stride_alignment, None).unwrap();
    let pool = FramePool::host(count, BufferSize::new(layouts.span().unwrap(), 0)).unwrap();
    (pool, layouts)
}

/// An output pool for MJPG frames of up to `cap` bytes.
fn output_pool(cap: usize, count: usize) -> FramePool {
    FramePool::host(count, BufferSize::new(cap, 0)).unwrap()
}

fn mjpg_entry(id: u64, w: u32, h: u32, cap: usize) -> FrameEntry {
    FrameEntry::new(
        id,
        0,
        MediaFormat::with_defaults(FourCc::MJPG, Resolution::new(w, h).unwrap()),
        Layouts::new(&[PlaneLayout::new(0, cap, cap)]).unwrap(),
    )
}

/// Fills every plane of `frame` with a pattern that differs per plane.
fn fill(frame: &mut FrameLease) {
    let mut planes = frame.write().expect("a sole, writable lease");
    for i in 0..planes.planes() {
        let plane = planes.plane(i).unwrap();
        for (k, b) in plane.data.iter_mut().enumerate() {
            *b = (k.wrapping_mul(7) ^ (i * 53)) as u8;
        }
    }
}

/// The encoder's JPEG of `frame`, computed directly from its planes.
fn direct(frame: &FrameLease, code: FourCc) -> Vec<u8> {
    let read = frame.read().unwrap();
    let (w, h) = {
        let r = frame.format().resolution;
        (r.width.get() as usize, r.height.get() as usize)
    };
    let p = |i| read.plane(i).unwrap();
    let pixels = match code {
        FourCc::I420 => PlanarInput::I420 {
            y: p(0).data,
            u: p(1).data,
            v: p(2).data,
            width: w,
            height: h,
            y_stride: p(0).stride,
            c_stride: p(1).stride,
        },
        FourCc::NV12 => PlanarInput::Nv12 {
            y: p(0).data,
            uv: p(1).data,
            width: w,
            height: h,
            y_stride: p(0).stride,
            uv_stride: p(1).stride,
        },
        _ => PlanarInput::Gray {
            y: p(0).data,
            width: w,
            height: h,
            stride: p(0).stride,
        },
    };
    let mut out = Vec::new();
    JpegEncoder::new(80).encode(&pixels, &mut out).unwrap();
    out
}

/// Encodes one frame of `code` (filled) and checks the MJPG frame: its entry, its bytes equal
/// the direct encoding, and they decode.
fn round_trip(code: FourCc, w: u32, h: u32, stride_alignment: Option<usize>) {
    let (inputs, _) = pool(code, w, h, stride_alignment, 2);
    let mut input = inputs
        .acquire(FrameEntry::new(
            7,
            123_456,
            MediaFormat::with_defaults(code, Resolution::new(w, h).unwrap()),
            packed_layouts(code, w, h, stride_alignment, None).unwrap(),
        ))
        .unwrap();
    fill(&mut input);
    let cap = (w * h * 3) as usize;
    let outputs = output_pool(cap, 2);
    let mut output = outputs.acquire(mjpg_entry(0, w, h, cap)).unwrap();

    let mut enc = StyxJpegEncoder::new(code, 80).unwrap();
    enc.encode(&input, &mut output).unwrap();

    let entry = *output.entry();
    assert_eq!(entry.id, 7);
    assert_eq!(entry.timestamp_ns, 123_456);
    assert_eq!(entry.format.code, FourCc::MJPG);
    assert_eq!(
        (
            entry.format.resolution.width.get(),
            entry.format.resolution.height.get()
        ),
        (w, h)
    );
    let read = output.read().unwrap();
    let bytes = read.plane(0).unwrap().data[..entry.layouts[0].len].to_vec();
    drop(read);
    assert_eq!(bytes, direct(&input, code), "{code} {w}x{h}");
    let mut d = jpeg_decoder::Decoder::new(&bytes[..]);
    d.decode().expect("decodes");
    let info = d.info().unwrap();
    assert_eq!((info.width as u32, info.height as u32), (w, h));
}

#[test]
fn i420_frames_encode_to_decodable_mjpg_frames() {
    round_trip(FourCc::I420, 64, 48, None);
    round_trip(FourCc::I420, 17, 33, None);
}

#[test]
fn nv12_and_grey_frames_encode() {
    round_trip(FourCc::NV12, 64, 48, None);
    round_trip(FourCc::NV12, 34, 18, None);
    round_trip(FourCc::GREY, 40, 24, None);
}

/// styx-format gives an NV12 frame of odd width chroma rows of `width` bytes, not the
/// `2 * width.div_ceil(2)` the interleaved pairs need, so the frame's planes are too small for
/// the encoder to read. It is refused, not read past the row (the format crate's geometry is an
/// interface change outside this PR).
#[test]
fn nv12_frames_of_odd_width_are_refused_by_their_layout() {
    let (w, h) = (33u32, 17u32);
    let (inputs, _) = pool(FourCc::NV12, w, h, None, 1);
    let mut input = inputs
        .acquire(FrameEntry::new(
            1,
            0,
            MediaFormat::with_defaults(FourCc::NV12, Resolution::new(w, h).unwrap()),
            packed_layouts(FourCc::NV12, w, h, None, None).unwrap(),
        ))
        .unwrap();
    fill(&mut input);
    let outputs = output_pool(4096, 1);
    let mut output = outputs.acquire(mjpg_entry(0, w, h, 4096)).unwrap();
    let mut enc = StyxJpegEncoder::new(FourCc::NV12, 80).unwrap();
    assert!(matches!(
        enc.encode(&input, &mut output),
        Err(CodecError::Rejected(_))
    ));
}

#[test]
fn padded_strides_are_read_as_strides() {
    round_trip(FourCc::I420, 50, 30, Some(64));
    round_trip(FourCc::NV12, 50, 30, Some(64));
    round_trip(FourCc::GREY, 50, 30, Some(64));
}

#[test]
fn a_frame_that_does_not_fit_the_output_is_rejected() {
    let (w, h) = (64u32, 48u32);
    let (inputs, _) = pool(FourCc::I420, w, h, None, 1);
    let mut input = inputs
        .acquire(FrameEntry::new(
            1,
            0,
            MediaFormat::with_defaults(FourCc::I420, Resolution::new(w, h).unwrap()),
            packed_layouts(FourCc::I420, w, h, None, None).unwrap(),
        ))
        .unwrap();
    fill(&mut input);
    let outputs = output_pool(64, 1);
    let mut output = outputs.acquire(mjpg_entry(0, w, h, 64)).unwrap();
    let mut enc = StyxJpegEncoder::new(FourCc::I420, 80).unwrap();
    match enc.encode(&input, &mut output) {
        Err(CodecError::Rejected(why)) => assert!(why.contains("does not fit"), "{why}"),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[test]
fn a_frame_of_another_format_is_rejected() {
    let (w, h) = (16u32, 16u32);
    let (inputs, _) = pool(FourCc::NV12, w, h, None, 1);
    let input = inputs
        .acquire(FrameEntry::new(
            1,
            0,
            MediaFormat::with_defaults(FourCc::NV12, Resolution::new(w, h).unwrap()),
            packed_layouts(FourCc::NV12, w, h, None, None).unwrap(),
        ))
        .unwrap();
    let outputs = output_pool(4096, 1);
    let mut output = outputs.acquire(mjpg_entry(0, w, h, 4096)).unwrap();
    let mut enc = StyxJpegEncoder::new(FourCc::I420, 80).unwrap();
    assert!(matches!(
        enc.encode(&input, &mut output),
        Err(CodecError::Rejected(_))
    ));
}

#[test]
fn formats_it_does_not_take_are_refused() {
    assert!(StyxJpegEncoder::new(FourCc::RG24, 80).is_err());
    assert!(candidate::<()>(FourCc::RG24, 80).is_none());
    for code in INPUTS {
        assert!(candidate::<()>(code, 80).is_some(), "{code}");
    }
}

#[test]
fn the_registry_chooses_the_native_encoder_first() {
    let mut registry: Registry = Registry::new();
    registry.register(candidate::<()>(FourCc::I420, 80).unwrap());
    let selection = registry
        .select(Request::new(CodecKind::Encoder, FourCc::I420, FourCc::MJPG))
        .expect("a native encoder");
    let descriptor = selection.implementation().descriptor().clone();
    assert_eq!(descriptor.backend, Backend::Native);
    assert_eq!(
        (descriptor.input, descriptor.output),
        (FourCc::I420, FourCc::MJPG)
    );
    assert!(matches!(
        selection.into_implementation(),
        Implementation::Encoder(_)
    ));
}

#[test]
fn restart_rows_put_one_marker_after_each_mcu_row_but_the_last() {
    let (w, h) = (64usize, 96usize);
    let cw = w / 2;
    let (y, u, v) = (
        vec![90u8; w * h],
        vec![120u8; cw * h / 2],
        vec![140u8; cw * h / 2],
    );
    let input = PlanarInput::I420 {
        y: &y,
        u: &u,
        v: &v,
        width: w,
        height: h,
        y_stride: w,
        c_stride: cw,
    };
    let mut out = Vec::new();
    JpegEncoder::new(60)
        .with_restart_rows(true)
        .encode(&input, &mut out)
        .unwrap();
    let sos = out.windows(2).position(|p| p == [0xFF, 0xDA]).unwrap();
    let markers = out[sos..]
        .windows(2)
        .filter(|p| p[0] == 0xFF && (0xD0..=0xD7).contains(&p[1]))
        .count();
    assert_eq!(markers, h / 16 - 1);
    assert!(out.windows(2).any(|p| p == [0xFF, 0xDD]), "a DRI segment");
    let mut d = jpeg_decoder::Decoder::new(&out[..]);
    d.decode().expect("decodes with restarts");
}

#[test]
fn fixed_output_gives_the_same_bytes_as_a_vec() {
    let (w, h) = (40usize, 24usize);
    let (y, u, v) = (
        vec![77u8; w * h],
        vec![90u8; w * h / 4],
        vec![200u8; w * h / 4],
    );
    let input = PlanarInput::I420 {
        y: &y,
        u: &u,
        v: &v,
        width: w,
        height: h,
        y_stride: w,
        c_stride: w / 2,
    };
    let mut vec_out = Vec::new();
    let mut enc = JpegEncoder::new(85);
    enc.encode(&input, &mut vec_out).unwrap();
    let mut buf = vec![0u8; vec_out.len() + 10];
    let n = enc.encode_fixed(&input, &mut buf).unwrap();
    assert_eq!(&buf[..n], &vec_out[..]);
    assert!(enc.encode_fixed(&input, &mut buf[..n - 1]).is_err());
}

#[test]
fn encode_blocks_gives_the_bytes_of_the_pixels_it_came_from() {
    // The CPU's own coefficients, coded through the block path: the same bytes.
    use styx_jpeg::CoefficientGrid;
    let (w, h) = (50usize, 30usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (ys, cs) = (w, cw);
    let y: Vec<u8> = (0..ys * h).map(|i| (i * 31 % 251) as u8).collect();
    let u: Vec<u8> = (0..cs * ch).map(|i| (i * 17 % 241) as u8).collect();
    let v: Vec<u8> = (0..cs * ch).map(|i| (i * 13 % 239) as u8).collect();
    let input = PlanarInput::I420 {
        y: &y,
        u: &u,
        v: &v,
        width: w,
        height: h,
        y_stride: ys,
        c_stride: cs,
    };
    let mut want = Vec::new();
    let mut enc = JpegEncoder::new(70);
    enc.encode(&input, &mut want).unwrap();

    // Coefficients for the same padded planes, computed with styx-simd directly.
    let grid = CoefficientGrid::new((w, h), false);
    let (pw, ph) = (w.next_multiple_of(16), h.next_multiple_of(16));
    let mut blocks = vec![0i16; grid.blocks() * 64];
    let [yq, cq] = *enc.quant();
    let pad = |src: &[u8], stride: usize, rw: usize, rh: usize, pw: usize, ph: usize| {
        let mut out = vec![0u8; pw * ph];
        for r in 0..ph {
            let sr = r.min(rh - 1);
            for c in 0..pw {
                out[r * pw + c] = src[sr * stride + c.min(rw - 1)];
            }
        }
        out
    };
    let (yp, up, vp) = (
        pad(&y, ys, w, h, pw, ph),
        pad(&u, cs, cw, ch, pw / 2, ph / 2),
        pad(&v, cs, cw, ch, pw / 2, ph / 2),
    );
    let luma_band = pw / 8 * 64;
    for band in 0..ph / 8 {
        styx_simd::jpeg::fdct_quant_band(
            &yp[band * 8 * pw..],
            pw,
            pw / 8,
            &yq,
            &mut blocks[band * luma_band..],
        );
    }
    let chroma_band = (pw / 16) * 64;
    let base = ph / 8 * (pw / 8) * 64;
    for band in 0..ph / 16 {
        let row = base + band * 2 * chroma_band;
        styx_simd::jpeg::fdct_quant_band(
            &up[band * 8 * (pw / 2)..],
            pw / 2,
            pw / 16,
            &cq,
            &mut blocks[row..],
        );
        styx_simd::jpeg::fdct_quant_band(
            &vp[band * 8 * (pw / 2)..],
            pw / 2,
            pw / 16,
            &cq,
            &mut blocks[row + chroma_band..],
        );
    }
    let mut got = Vec::new();
    enc.encode_blocks((w, h), false, &blocks, &mut got).unwrap();
    assert_eq!(got, want);
}
