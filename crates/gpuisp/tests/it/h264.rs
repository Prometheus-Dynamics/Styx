//! `GpuConvert::motion_search` against `styx_core::simd::h264::full_search`, macroblock for
//! macroblock: shifted, noisy and flat pictures, ranges 0 to 16, sizes from one macroblock up.

use crate::common::contexts;
use styx_core::simd::h264::full_search;
use styx_gpuisp::{GpuConvert, MotionCandidate};

fn random(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed.wrapping_mul(2_654_435_761) | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s >> 24) as u8
        })
        .collect()
}

/// `reference` moved by (`sx`, `sy`) with a little noise, edges repeated.
fn moved(reference: &[u8], (w, h): (usize, usize), (sx, sy): (i32, i32), seed: u32) -> Vec<u8> {
    let noise = random(w * h, seed);
    (0..w * h)
        .map(|i| {
            let x = ((i % w) as i32 - sx).clamp(0, w as i32 - 1) as usize;
            let y = ((i / w) as i32 - sy).clamp(0, h as i32 - 1) as usize;
            reference[y * w + x].saturating_add(noise[i] >> 6)
        })
        .collect()
}

#[test]
fn motion_search_matches_the_cpu() {
    for ctx in contexts() {
        let mut c = GpuConvert::new(&ctx).unwrap();
        let mut got = Vec::new();
        for (i, &(w, h)) in [(16, 16), (48, 32), (176, 144)].iter().enumerate() {
            // A smooth picture with texture, so most offsets differ.
            let base: Vec<u8> = random(w * h, i as u32)
                .iter()
                .enumerate()
                .map(|(j, &n)| ((j % w) * 3 + (j / w) * 2) as u8 ^ (n >> 5))
                .collect();
            for (shift, range) in [((3, -2), 8), ((-7, 5), 16), ((0, 0), 0), ((12, 9), 4)] {
                let cur = moved(&base, (w, h), shift, range as u32);
                c.motion_search(&cur, &base, (w, h), range, &mut got)
                    .unwrap();
                assert_eq!(got.len(), w * h / 256);
                for (m, g) in got.iter().enumerate() {
                    let (mv, sad) =
                        full_search(&cur, &base, (w, h), (m % (w / 16), m / (w / 16)), range);
                    assert_eq!(
                        *g,
                        MotionCandidate { mv, sad },
                        "{w}x{h} {shift:?} range {range} mb {m}"
                    );
                }
            }
            let flat = vec![77u8; w * h];
            c.motion_search(&flat, &flat, (w, h), 16, &mut got).unwrap();
            assert!(got.iter().all(|g| g.mv == [0, 0] && g.sad == 0));
        }
    }
}
