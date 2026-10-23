// One pyramid level: the level above halved by a rounded 2x2 box filter,
// (a + b + c + d + 2) >> 2, as styx-core's `box_downscale_luma`.
//
// Pixels are 8-bit, four to a 32-bit word (little-endian), rows `in_stride` and `out_stride`
// words apart. Each invocation makes one output word: four output pixels from two words of
// each of two input rows. Pixels past the level's width are don't-care.

struct Params {
    in_stride: u32,
    out_stride: u32,
    out_height: u32,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> p: Params;

// The sums of a word's two horizontal pixel pairs.
fn pairs(w: u32) -> vec2<u32> {
    return vec2<u32>((w & 0xffu) + ((w >> 8u) & 0xffu), ((w >> 16u) & 0xffu) + (w >> 24u));
}

fn word(i: u32, row_end: u32) -> u32 {
    if (i < row_end) {
        return src[i];
    }
    return 0u;
}

@compute @workgroup_size(16, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    let y = id.y;
    if (x >= p.out_stride || y >= p.out_height) {
        return;
    }
    let top = 2u * y * p.in_stride;
    let bottom = top + p.in_stride;
    let i = 2u * x;
    let a = pairs(word(top + i, top + p.in_stride)) + pairs(word(bottom + i, bottom + p.in_stride));
    let b = pairs(word(top + i + 1u, top + p.in_stride)) + pairs(word(bottom + i + 1u, bottom + p.in_stride));
    let q = (vec4<u32>(a, b) + vec4<u32>(2u)) >> vec4<u32>(2u);
    dst[y * p.out_stride + x] = q.x | (q.y << 8u) | (q.z << 16u) | (q.w << 24u);
}
