// Shared by every GPU ISP shader: the parameter block (written by `src/params.rs`, whose word
// order this must match), the tone and colour helpers and the output conversions of
// `styx-softisp`'s integer arithmetic, bit for bit. The raw input and the front end are in
// `front.glsl`.

#extension GL_EXT_shader_8bit_storage : require

const uint WORK_MAX = 4095u;

const uint PACK_U8 = 0u;
const uint PACK_U16 = 1u;
const uint PACK_RAW10 = 2u;
const uint PACK_RAW12 = 3u;

const uint OUT_RGB24 = 0u;
const uint OUT_NV12 = 1u;
const uint OUT_I420 = 2u;
const uint OUT_LUMA = 3u;
// The picture's JFIF Y, Cb, Cr planes, one after another (the output stages' input).
const uint OUT_PLANES = 4u;

layout(set = 0, binding = 0, std430) readonly buffer Params {
    uint width;
    uint height;
    uint in_stride;
    uint in_offset;
    uint packing;
    uint max_value;
    uint shift;
    uint lsc_on;
    uint lsc_rows;
    uint lsc_ymap;
    // [row parity][column parity]
    uint black[4];
    uint flat_gain[4];
    // RowKind per row parity.
    uint green_even[2];
    uint x_is_red[2];
    // Quad positions (top-left, top-right, bottom-left, bottom-right) of R, G, G, B.
    uint quad_idx[4];
    uint demosaic;
    uint ccm_on;
    int ccm[9];
    uint tone_lut;
    uint yuv_y[3];
    uint yuv_y_offset;
    int yuv_u[3];
    int yuv_v[3];
    uint out_kind;
    uint out_width;
    uint out_height;
    uint out_offset[3];
    uint out_stride[3];
    uint zones_x;
    uint zones_y;
    uint bins;
    uint saturation;
    uint row_step;
    // Window origin in the frame (even; 0 for the whole frame): the picture's outputs are the
    // window's, its samples the frame's (out_width/out_height are the window's size).
    uint win_x;
    uint win_y;
    // Output stages: the picture's size (its planes are `pic_w` x `pic_h` each), the window's
    // origin in the picture, the denoise and sharpening parameters (Q8 strengths, quarter-code
    // thresholds, sharpening codes).
    uint pic_w;
    uint pic_h;
    uint crop_x;
    uint crop_y;
    uint denoise_luma;
    uint denoise_chroma;
    uint luma_strength;
    uint chroma_threshold;
    uint chroma_strength;
    uint sharpen_on;
    int sharpen_amount;
    int sharpen_threshold;
    int sharpen_limit;
} P;

layout(set = 0, binding = 1, std430) readonly buffer Lut {
    uint lut[];
};

// Mirror `i` into `0..n`, keeping its parity (`pipeline::reflect`).
uint reflect_into(int i, uint n) {
    int m = int(n);
    int r = i < 0 ? -i : (i >= m ? 2 * m - 2 - i : i);
    return uint(clamp(r, 0, m - 1));
}

// `simd::scalar::ccm_term`.
int ccm_term(uint x, int c) {
    return ((int(x) << 3) * c + (1 << 14)) >> 15;
}

int sat16(int v) {
    return clamp(v, -32768, 32767);
}

uvec3 colour(uvec3 rgb) {
    if (P.ccm_on == 0u) {
        return rgb;
    }
    uvec3 o;
    for (int k = 0; k < 3; k++) {
        int s = sat16(sat16(ccm_term(rgb.r, P.ccm[3 * k]) + ccm_term(rgb.g, P.ccm[3 * k + 1]))
            + ccm_term(rgb.b, P.ccm[3 * k + 2]));
        o[k] = uint(clamp(s, 0, int(WORK_MAX)));
    }
    return o;
}

// `simd::poly::PolyTone::apply` with `x` below 4096: the quadratic of its half octave, from
// `lut[3 segment + k]` (k = 0, 1, 2; two's complement), in the CPU's 16-bit saturating
// arithmetic. `mulhrs` is `vpmulhrsw`; its products stay within 16 bits here.
int mulhrs(int a, int b) {
    return (a * b + (1 << 14)) >> 15;
}

uint poly_tone(uint x) {
    uint v = min(x, WORK_MAX) + 32u;
    uint e = min(uint(findMSB(v)), 11u);
    int t = int((v << (14u - e)) - (1u << 14));
    uint seg = 2u * (e - 5u) + uint(min(t >> 13, 1));
    int inner = clamp(int(lut[3u * seg + 1u]) + mulhrs(int(lut[3u * seg + 2u]), t), -32768, 32767);
    int y = clamp(int(lut[3u * seg]) + mulhrs(inner, t), -32768, 32767);
    return uint(clamp(mulhrs(y, 1024), 0, 255));
}

// Tone mode (`P.tone_lut`): 0 plain narrowing, 1 the 4096-entry table, 2 the quadratics.
uint tone(uint v) {
    if (P.tone_lut == 2u) {
        return poly_tone(v);
    }
    if (P.tone_lut != 0u) {
        return lut[min(v, WORK_MAX)];
    }
    return min(v >> 4, 255u);
}

uvec3 tone3(uvec3 v) {
    return uvec3(tone(v.r), tone(v.g), tone(v.b));
}

// `simd::scalar::rgb_to_y_row`.
uint luma8(uvec3 c) {
    uint s = P.yuv_y[0] * c.r + P.yuv_y[1] * c.g + P.yuv_y[2] * c.b;
    return min(((s + 128u) >> 8) + P.yuv_y_offset, 255u);
}

// `simd::scalar::chroma` with the Cb (`v` false) or Cr coefficients.
uint chroma8(ivec3 c, bool v) {
    int s = v ? P.yuv_v[0] * c.r + P.yuv_v[1] * c.g + P.yuv_v[2] * c.b
              : P.yuv_u[0] * c.r + P.yuv_u[1] * c.g + P.yuv_u[2] * c.b;
    return uint(clamp(128 + ((s + 64) >> 7), 0, 255));
}

// R, mean G, B of the quad `[top-left, top-right, bottom-left, bottom-right]`.
uvec3 quad_colours(uint q[4]) {
    uint g = (q[P.quad_idx[1]] + q[P.quad_idx[2]] + 1u) >> 1;
    return uvec3(q[P.quad_idx[0]], g, q[P.quad_idx[3]]);
}

// `simd::post::scalar::rgb_to_planes_row`: JFIF (Y, Cb, Cr) of an RGB pixel.
uvec3 planes_of(uvec3 c) {
    int r = int(c.r), g = int(c.g), b = int(c.b);
    int y = (77 * r + 150 * g + 29 * b + 128) >> 8;
    int cb = ((-43 * r - 85 * g + 128 * b + 128) >> 8) + 128;
    int cr = ((128 * r - 107 * g - 21 * b + 128) >> 8) + 128;
    return uvec3(clamp(y, 0, 255), clamp(cb, 0, 255), clamp(cr, 0, 255));
}

// `simd::post::scalar::planes_to_rgb_row` of one pixel (Y, Cb, Cr).
uvec3 rgb_of(uint y, uint cb, uint cr) {
    int yi = int(y), cbi = int(cb) - 128, cri = int(cr) - 128;
    return uvec3(
        uint(clamp((256 * yi + 359 * cri + 128) >> 8, 0, 255)),
        uint(clamp((256 * yi - 88 * cbi - 183 * cri + 128) >> 8, 0, 255)),
        uint(clamp((256 * yi + 454 * cbi + 128) >> 8, 0, 255)));
}
