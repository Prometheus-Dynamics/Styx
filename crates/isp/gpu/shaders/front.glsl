// The raw input and the front end (unpack, black level, gains, lens shading) of the integer
// arithmetic of `styx-softisp` (`simd::scalar`), bit for bit. Included after `common.glsl` by
// the shaders that read the raw frame.

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
