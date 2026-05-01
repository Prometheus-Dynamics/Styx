#[inline(always)]
pub(super) unsafe fn avg2_u16(
    a: std::arch::aarch64::uint16x8_t,
    b: std::arch::aarch64::uint16x8_t,
) -> std::arch::aarch64::uint16x8_t {
    use std::arch::aarch64::{
        vaddq_u32, vcombine_u16, vget_high_u16, vget_low_u16, vmovl_u16, vmovn_u32, vshrq_n_u32,
    };
    unsafe {
        let a0 = vmovl_u16(vget_low_u16(a));
        let a1 = vmovl_u16(vget_high_u16(a));
        let b0 = vmovl_u16(vget_low_u16(b));
        let b1 = vmovl_u16(vget_high_u16(b));
        let lo = vshrq_n_u32(vaddq_u32(a0, b0), 1);
        let hi = vshrq_n_u32(vaddq_u32(a1, b1), 1);
        vcombine_u16(vmovn_u32(lo), vmovn_u32(hi))
    }
}

#[inline(always)]
pub(super) unsafe fn avg4_u16(
    a: std::arch::aarch64::uint16x8_t,
    b: std::arch::aarch64::uint16x8_t,
    c: std::arch::aarch64::uint16x8_t,
    d: std::arch::aarch64::uint16x8_t,
) -> std::arch::aarch64::uint16x8_t {
    use std::arch::aarch64::{
        vaddq_u32, vcombine_u16, vget_high_u16, vget_low_u16, vmovl_u16, vmovn_u32, vshrq_n_u32,
    };

    unsafe {
        let a0 = vmovl_u16(vget_low_u16(a));
        let a1 = vmovl_u16(vget_high_u16(a));
        let b0 = vmovl_u16(vget_low_u16(b));
        let b1 = vmovl_u16(vget_high_u16(b));
        let c0 = vmovl_u16(vget_low_u16(c));
        let c1 = vmovl_u16(vget_high_u16(c));
        let d0 = vmovl_u16(vget_low_u16(d));
        let d1 = vmovl_u16(vget_high_u16(d));

        let lo = vshrq_n_u32(vaddq_u32(vaddq_u32(a0, b0), vaddq_u32(c0, d0)), 2);
        let hi = vshrq_n_u32(vaddq_u32(vaddq_u32(a1, b1), vaddq_u32(c1, d1)), 2);
        vcombine_u16(vmovn_u32(lo), vmovn_u32(hi))
    }
}

#[inline(always)]
pub(super) unsafe fn shift_u16x8_to_u8(
    v: std::arch::aarch64::uint16x8_t,
    shift: u32,
) -> std::arch::aarch64::uint8x8_t {
    use std::arch::aarch64::{vmovn_u16, vshrn_n_u16};
    unsafe {
        match shift {
            0 => vmovn_u16(v),
            1 => vshrn_n_u16(v, 1),
            2 => vshrn_n_u16(v, 2),
            3 => vshrn_n_u16(v, 3),
            4 => vshrn_n_u16(v, 4),
            5 => vshrn_n_u16(v, 5),
            6 => vshrn_n_u16(v, 6),
            7 => vshrn_n_u16(v, 7),
            8 => vshrn_n_u16(v, 8),
            _ => vshrn_n_u16(v, 8),
        }
    }
}
