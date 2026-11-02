// The fp16 variant's arithmetic (`full16.comp`, `half16.comp`; the output stage and the
// statistics are the integer ones). Working values are float16 on the integer scale, 0..4094:
// the front end, the colour matrix and the bilinear demosaic run in float16, the tone curve
// is the integer table (or quadratics) at the nearest index. Not bit-exact: `styx-softisp`'s
// `Arithmetic::Half` is the reference, within its own bounds (`tests/it/quality.rs`).

#extension GL_EXT_shader_explicit_arithmetic_types_float16 : require

// Front end: the sample less its black level, times its Q12 gain (lens shading included)
// scaled to the 12-bit range: `gain * 2^(shift - 16)` is the integer arithmetic's `2^-bits`.
float16_t front16(uint x, uint y) {
    uint v = raw_sample(x, y);
    uint bl = P.black[(y & 1u) * 2u + (x & 1u)];
    float16_t d = float16_t(float(v > bl ? v - bl : 0u));
    float16_t g = float16_t(float(gain_at(x, y)) * exp2(float(P.shift) - 16.0));
    return min(d * g, float16_t(4094.0));
}

// Colour matrix coefficient `k` (Q12 on the wire), as float16.
float16_t ccm16(uint k) {
    return float16_t(float(P.ccm[k]) * (1.0 / 4096.0));
}

// The colour matrix, clamped to the working range.
f16vec3 colour16(f16vec3 c) {
    if (P.ccm_on == 0u) {
        return c;
    }
    f16vec3 o;
    for (uint k = 0u; k < 3u; k++) {
        float16_t s = ccm16(3u * k) * c.r + ccm16(3u * k + 1u) * c.g + ccm16(3u * k + 2u) * c.b;
        o[k] = min(max(s, float16_t(0.0)), float16_t(4094.0));
    }
    return o;
}

// The 12-bit index of a working value: its nearest integer.
uint index16(float16_t v) {
    return uint(clamp(float(v) + 0.5, 0.0, 4095.0));
}

// The 8-bit output of working values `c` through the tone curve.
uvec3 tone16(f16vec3 c) {
    return uvec3(tone(index16(c.r)), tone(index16(c.g)), tone(index16(c.b)));
}

// R, mean G, B of a quad `[top-left, top-right, bottom-left, bottom-right]` (fp16).
f16vec3 quad16(float16_t q[4]) {
    float16_t g = (q[P.quad_idx[1]] + q[P.quad_idx[2]]) * float16_t(0.5);
    return f16vec3(q[P.quad_idx[0]], g, q[P.quad_idx[3]]);
}
