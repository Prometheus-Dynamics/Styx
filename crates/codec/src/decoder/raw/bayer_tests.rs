use super::*;
use crate::CodecRegistry;

const W: usize = 16;
const H: usize = 8;

/// A BGGR mosaic of one colour (10-bit samples).
fn mosaic(rgb: [u16; 3]) -> Vec<u16> {
    (0..W * H)
        .map(|i| {
            let (x, y) = (i % W, i / W);
            match (x & 1, y & 1) {
                (0, 0) => rgb[2],
                (1, 1) => rgb[0],
                _ => rgb[1],
            }
        })
        .collect()
}

/// CSI-2 packed RAW10 rows: high bytes, then a byte of the four low bit pairs.
fn pack_raw10(m: &[u16]) -> Vec<u8> {
    m.chunks(4)
        .flat_map(|px| {
            let low = px
                .iter()
                .enumerate()
                .fold(0u8, |acc, (i, &v)| acc | (((v & 3) as u8) << (2 * i)));
            [
                (px[0] >> 2) as u8,
                (px[1] >> 2) as u8,
                (px[2] >> 2) as u8,
                (px[3] >> 2) as u8,
                low,
            ]
        })
        .collect()
}

fn frame(code: [u8; 4], bytes: &[u8], stride: usize) -> FrameLease {
    let pool = BufferPool::with_limits(1, bytes.len(), 1);
    let mut buf = pool.lease();
    buf.resize(bytes.len());
    buf.as_mut_slice().copy_from_slice(bytes);
    let res = Resolution::new(W as u32, H as u32).unwrap();
    FrameLease::single_plane(
        FrameMeta::new(
            MediaFormat::new(FourCc::new(code), res, ColorSpace::Unknown),
            7,
        ),
        buf,
        bytes.len(),
        stride,
    )
}

#[test]
fn registry_finds_every_output_for_bayer_inputs() {
    let registry = CodecRegistry::with_enabled_codecs().unwrap();
    let handle = registry.handle();
    for code in [*b"pBAA", *b"pRCC", *b"BA81", *b"RG10"] {
        for out in [FourCc::RG24, FourCc::NV12, FourCc::GREY] {
            let codec = handle
                .lookup_for_output_where(FourCc::new(code), out, |_| true)
                .unwrap();
            assert_eq!(codec.descriptor().impl_name, "softisp");
        }
    }
}

#[test]
fn packed_and_unpacked_decode_to_the_colour() {
    let m = mosaic([800, 480, 160]);
    let packed = pack_raw10(&m);
    let unpacked: Vec<u8> = m.iter().flat_map(|v| v.to_le_bytes()).collect();
    let want = [200u8, 120, 40];
    for (code, bytes, stride) in [(*b"pBAA", &packed, W / 4 * 5), (*b"BG10", &unpacked, W * 2)] {
        let rgb = SoftIspDecoder::new(FourCc::new(code), SoftIspOutput::Rgb24, 64, 64).unwrap();
        let out = rgb.process(frame(code, bytes, stride)).unwrap();
        assert_eq!(out.meta().format.code, FourCc::RG24);
        assert_eq!(out.meta().timestamp, 7);
        // Within a code: CPUs with FP16 arithmetic run the software ISP in fp16
        // (`styx_softisp::Arithmetic`), which rounds where the integer path truncates.
        let near = |a: u8, b: u8| a.abs_diff(b) <= 1;
        assert!(
            out.planes()[0]
                .data()
                .as_chunks::<3>()
                .0
                .iter()
                .all(|p| p.iter().zip(want).all(|(&a, b)| near(a, b)))
        );

        let grey = SoftIspDecoder::new(FourCc::new(code), SoftIspOutput::Luma, 64, 64).unwrap();
        let out = grey.process(frame(code, bytes, stride)).unwrap();
        assert!(out.planes()[0].data().iter().all(|&v| near(v, 120)));

        let nv12 = SoftIspDecoder::new(FourCc::new(code), SoftIspOutput::Nv12, 64, 64).unwrap();
        let out = nv12.process(frame(code, bytes, stride)).unwrap();
        let planes = out.planes();
        assert_eq!(planes.len(), 2);
        assert_eq!(
            (planes[0].data().len(), planes[1].data().len()),
            (W * H, W * H / 2)
        );
        assert_eq!(out.meta().format.color, ColorSpace::Bt709);
    }
}

#[test]
fn parameters_apply_and_stats_are_kept() {
    let m = mosaic([400, 400, 400]);
    let packed = pack_raw10(&m);
    let dec = SoftIspDecoder::new(FourCc::new(*b"pBAA"), SoftIspOutput::Rgb24, 64, 64).unwrap();
    assert!(dec.last_stats().is_none());
    dec.set_params(IspParams {
        white_balance: Some(styx_softisp::WhiteBalance {
            r: 2.0,
            g: 1.0,
            b: 1.0,
        }),
        stats: Some(styx_softisp::StatsConfig {
            zones_x: 2,
            zones_y: 2,
            ..Default::default()
        }),
        ..Default::default()
    });
    let out = dec.process(frame(*b"pBAA", &packed, W / 4 * 5)).unwrap();
    let px = &out.planes()[0].data()[..3];
    assert!(
        px.iter()
            .zip([200u8, 100, 100])
            .all(|(a, b)| a.abs_diff(b) <= 1),
        "{px:?}"
    );
    let stats = dec.last_stats().unwrap();
    assert_eq!(stats.gains, [2.0, 1.0, 1.0]);
    assert_eq!(stats.samples, (W * H / 4) as u32);
}

#[cfg(target_os = "linux")]
#[test]
fn nv12_into_shared_memory() {
    let m = mosaic([800, 480, 160]);
    let packed = pack_raw10(&m);
    let dec = SoftIspDecoder::new(FourCc::new(*b"pBAA"), SoftIspOutput::Nv12, 64, 64).unwrap();
    let pool = SharedBufferPool::with_capacity(2, 4096).unwrap();
    let owned = dec.process(frame(*b"pBAA", &packed, W / 4 * 5)).unwrap();
    let shared = dec
        .process_shared(&frame(*b"pBAA", &packed, W / 4 * 5), &pool)
        .unwrap()
        .unwrap();
    let (a, b) = (owned.planes(), shared.planes());
    assert_eq!(a[0].data(), b[0].data());
    assert_eq!(a[1].data(), b[1].data());
}
