use super::*;

/// The GPU, or `None` (and a note) where there is none: the kernels are optional.
fn gpu() -> Option<Gpu> {
    match Gpu::new() {
        Ok(g) => Some(g),
        Err(e) => {
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), "skipped: {e}");
            None
        }
    }
}

/// styx-core's `box_downscale_luma`, one level: `(a + b + c + d + 2) >> 2`, sizes rounded down.
fn half(src: &[u8], w: usize, h: usize, stride: usize) -> (Vec<u8>, usize, usize) {
    let (ow, oh) = (w / 2, h / 2);
    let mut out = vec![0u8; ow * oh];
    for y in 0..oh {
        for x in 0..ow {
            let p = |dx: usize, dy: usize| u32::from(src[(2 * y + dy) * stride + 2 * x + dx]);
            out[y * ow + x] = ((p(0, 0) + p(1, 0) + p(0, 1) + p(1, 1) + 2) >> 2) as u8;
        }
    }
    (out, ow, oh)
}

fn image(h: usize, stride: usize, seed: u32) -> Vec<u8> {
    let mut s = seed | 1;
    (0..stride * h)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s >> 24) as u8
        })
        .collect()
}

/// Every level matches the CPU's bit for bit: word-aligned and odd strides, odd sizes, a
/// last row without its stride's padding, and the plan reused and remade.
#[test]
fn levels_are_the_cpus_box_filter_bit_for_bit() {
    let Some(gpu) = gpu() else { return };
    eprintln_adapter(&gpu);
    let mut pyramid = BoxPyramid::new(&gpu);
    for (w, h, stride, levels, trim) in [
        (1280usize, 800usize, 1280usize, 3u8, false),
        (1280, 800, 1280, 3, false),
        (37, 23, 41, 2, false),
        (64, 48, 128, 4, true),
        (10, 6, 10, 1, false),
        (1281, 801, 1344, 3, true),
    ] {
        let data = image(h, stride, (w * h) as u32);
        let len = if trim {
            stride * (h - 1) + w
        } else {
            data.len()
        };
        let src = Luma {
            data: &data[..len],
            width: w as u32,
            height: h as u32,
            stride,
        };
        let mut expected = Vec::new();
        let (mut cur, mut cw, mut ch, mut cs) = (data.clone(), w, h, stride);
        for _ in 0..levels {
            let (next, nw, nh) = half(&cur, cw, ch, cs);
            expected.push((next.clone(), nw, nh));
            (cur, cw, ch, cs) = (next, nw, nh, nw);
        }
        let mut got = 0;
        pyramid
            .run(src, levels, |l| {
                let (want, ew, eh) = &expected[usize::from(l.level) - 1];
                assert_eq!((l.width as usize, l.height as usize), (*ew, *eh));
                for y in 0..*eh {
                    assert_eq!(
                        &l.data[y * l.stride..y * l.stride + ew],
                        &want[y * ew..(y + 1) * ew],
                        "{w}x{h} stride {stride}: level {} row {y}",
                        l.level
                    );
                }
                got += 1;
            })
            .unwrap();
        assert_eq!(got, levels, "{w}x{h}");
    }
}

#[test]
fn sizes_without_the_level_are_refused() {
    let Some(gpu) = gpu() else { return };
    eprintln_adapter(&gpu);
    let mut pyramid = BoxPyramid::new(&gpu);
    let data = vec![0u8; 16];
    let src = Luma {
        data: &data,
        width: 4,
        height: 4,
        stride: 4,
    };
    assert!(pyramid.run(src, 3, |_| {}).is_err());
    let short = Luma {
        data: &data[..10],
        ..src
    };
    assert!(pyramid.run(short, 1, |_| {}).is_err());
}

fn eprintln_adapter(gpu: &Gpu) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "adapter: {}", gpu.name());
}
