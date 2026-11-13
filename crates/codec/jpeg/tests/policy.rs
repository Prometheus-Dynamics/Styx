//! The SIMD policy is a switch, not a change of output: the scalar oracle and the vector leaves
//! give the same bytes. (A separate test binary, because the policy is process-wide.)

use styx_jpeg::{JpegEncoder, PlanarInput};
use styx_simd::{SimdPolicy, policy, set_policy};

fn encode_all(q: u8) -> Vec<Vec<u8>> {
    let mut outs = Vec::new();
    for (w, h) in [(1usize, 1usize), (17, 33), (64, 48), (321, 241)] {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y: Vec<u8> = (0..w * h).map(|i| (i * 37 % 253) as u8).collect();
        let u: Vec<u8> = (0..cw * ch).map(|i| (i * 11 % 247) as u8).collect();
        let v: Vec<u8> = (0..cw * ch).map(|i| (i * 29 % 233) as u8).collect();
        let mut out = Vec::new();
        JpegEncoder::new(q)
            .encode(
                &PlanarInput::I420 {
                    y: &y,
                    u: &u,
                    v: &v,
                    width: w,
                    height: h,
                    y_stride: w,
                    c_stride: cw,
                },
                &mut out,
            )
            .unwrap();
        outs.push(out);
    }
    outs
}

#[test]
fn scalar_and_vector_policies_give_the_same_bytes() {
    set_policy(SimdPolicy::Scalar);
    assert_eq!(policy(), SimdPolicy::Scalar);
    let scalar = encode_all(73);
    set_policy(SimdPolicy::Auto);
    let auto = encode_all(73);
    assert_eq!(scalar, auto);
}
