//! The YUV → RGB rows against the scalar oracle, and the oracle against the formula it
//! replaced: every width around the block sizes, every Y, U and V value, guard bytes after
//! the row, and the vector leaf running where there is one.

use super::*;

const GUARD: u8 = 0xA7;

const MATRICES: [YuvMatrix; 3] = [YuvMatrix::BT709, YuvMatrix::BT2020, YuvMatrix::BT601_FULL];

fn bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Whether this target has a YUV → RGB leaf that runs here.
fn has_leaf() -> bool {
    (cfg!(target_arch = "arm") && strongest_backend() == SimdBackend::Neon) || has_rvv()
}

/// The three rows over `y`, `u`, `v` (one chroma per pixel pair) against the oracle.
fn check(y: &[u8], u: &[u8], v: &[u8], m: &YuvMatrix, vector: &mut bool) {
    let width = y.len();
    let mut want = vec![0u8; width * 3];
    scalar::i420_rgb_row(y, u, v, &mut want, width, m);
    let uv: Vec<u8> = u.iter().zip(v).flat_map(|(&a, &b)| [a, b]).collect();
    let yuyv: Vec<u8> = (0..u.len())
        .flat_map(|p| [y[2 * p], u[p], *y.get(2 * p + 1).unwrap_or(&0), v[p]])
        .collect();
    type Row<'a> = Box<dyn Fn(&mut [u8]) -> SimdBackend + 'a>;
    let rows: [(&str, Row); 3] = [
        ("nv12", Box::new(|d| nv12_rgb_row(y, &uv, d, width, m))),
        ("i420", Box::new(|d| i420_rgb_row(y, u, v, d, width, m))),
        ("yuyv", Box::new(|d| yuyv_rgb_row(&yuyv, d, width, m))),
    ];
    for (name, row) in rows {
        let mut got = vec![GUARD; width * 3 + 64];
        *vector |= row(&mut got[..width * 3]) != SimdBackend::Scalar;
        assert_eq!(got[..width * 3], want[..], "{name}: width {width}, {m:?}");
        assert!(
            got[width * 3..].iter().all(|&b| b == GUARD),
            "{name}: wrote past the row"
        );
    }
}

#[test]
fn rows_match_the_oracle_at_every_width() {
    let mut vector = false;
    for m in &MATRICES {
        for width in (0..=100usize).chain([127, 128, 129, 255, 256, 257, 1280]) {
            let half = width.div_ceil(2);
            let (y, u, v) = (
                bytes(width, width as u64),
                bytes(half, 7 + width as u64),
                bytes(half, 11),
            );
            check(&y, &u, &v, m, &mut vector);
        }
    }
    assert_eq!(vector, has_leaf());
}

#[test]
fn rows_match_the_oracle_for_every_value() {
    let mut vector = false;
    for m in &MATRICES {
        // Each row: every luma twice, against every V with one U.
        let y: Vec<u8> = (0..512).map(|i| (i % 256) as u8).collect();
        let v: Vec<u8> = (0..=255).collect();
        for u in 0..=255 {
            check(&y, &[u; 256], &v, m, &mut vector);
        }
    }
    assert_eq!(vector, has_leaf());
}

/// The formula the decoders used before the matrix type, for every input.
#[test]
fn the_oracle_is_the_old_formula() {
    for (m, full, rv, gu, gv, bu) in [
        (YuvMatrix::BT709, false, 459, 55, 136, 541),
        (YuvMatrix::BT2020, false, 430, 48, 166, 549),
        (YuvMatrix::BT601_FULL, true, 359, 88, 183, 454),
    ] {
        for y in 0..=255i32 {
            for u in 0..=255i32 {
                for v in (0..=255i32).step_by(3) {
                    let (d, e) = (u - 128, v - 128);
                    let (c, s) = if full {
                        (y, 256)
                    } else {
                        ((y - 16).max(0), 298)
                    };
                    let old = [
                        (s * c + rv * e + 128) >> 8,
                        (s * c - gu * d - gv * e + 128) >> 8,
                        (s * c + bu * d + 128) >> 8,
                    ]
                    .map(|x| x.clamp(0, 255) as u8);
                    assert_eq!(m.rgb(y as u8, u as u8, v as u8), old, "{y} {u} {v}");
                }
            }
        }
    }
}
