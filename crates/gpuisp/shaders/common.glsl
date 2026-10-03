// Shared by every GPU ISP shader: the parameter block (written by `src/params.rs`, whose word
// order this must match), the raw input, and the front end (unpack, black level, gains, lens
// shading) in the integer arithmetic of `styx-softisp` (`simd::scalar`), bit for bit.

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
} P;

layout(set = 0, binding = 1, std430) readonly buffer Lut {
    uint lut[];
};

// Lens shading: Q12 gains as u16 pairs, `[row parity][grid row][column]`, then per image row
// the grid row (low 16 bits) and the Q15 weight of the next (high 16 bits) from `lsc_ymap`.
layout(set = 0, binding = 2, std430) readonly buffer Lsc {
    uint lsc[];
};

layout(set = 1, binding = 0, std430) readonly buffer Raw {
    uint8_t raw[];
};

uint rb(uint i) {
    return uint(raw[P.in_offset + i]);
}

uint raw_sample(uint x, uint y) {
    uint base = y * P.in_stride;
    if (P.packing == PACK_RAW10) {
        uint g = base + (x >> 2) * 5u;
        uint lane = x & 3u;
        return (rb(g + lane) << 2) | ((rb(g + 4u) >> (2u * lane)) & 3u);
    } else if (P.packing == PACK_RAW12) {
        uint g = base + (x >> 1) * 3u;
        uint lane = x & 1u;
        return (rb(g + lane) << 4) | ((rb(g + 2u) >> (4u * lane)) & 15u);
    } else if (P.packing == PACK_U16) {
        uint o = base + 2u * x;
        return min(rb(o) | (rb(o + 1u) << 8), P.max_value);
    }
    return rb(base + x);
}

uint lsc_entry(uint i) {
    return (lsc[i >> 1] >> (16u * (i & 1u))) & 0xFFFFu;
}

uint gain_at(uint x, uint y) {
    if (P.lsc_on == 0u) {
        return P.flat_gain[(y & 1u) * 2u + (x & 1u)];
    }
    uint ym = lsc[P.lsc_ymap + y];
    uint i = ym & 0xFFFFu;
    int f = int(ym >> 16);
    uint row0 = (y & 1u) * P.lsc_rows;
    uint a = lsc_entry((row0 + i) * P.width + x);
    uint b = lsc_entry((row0 + min(i + 1u, P.lsc_rows - 1u)) * P.width + x);
    return uint(int(a) + (((int(b) - int(a)) * f) >> 15)) & 0xFFFFu;
}

// `simd::scalar::front_row` of the sample at column `x`, row `y` (inside the frame).
uint front(uint x, uint y) {
    uint v = raw_sample(x, y);
    uint bl = P.black[(y & 1u) * 2u + (x & 1u)];
    uint d = ((v > bl ? v - bl : 0u) << P.shift) & 0xFFFFu;
    return min((d * gain_at(x, y)) >> 16, WORK_MAX);
}

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

uint tone(uint v) {
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
