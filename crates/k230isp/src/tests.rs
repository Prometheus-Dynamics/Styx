use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use styx_algo::Statistics;

use crate::regs::*;
use crate::stats::{self, Raw};
use crate::*;

/// A fake register file: untouched registers read as zero, every write is kept.
#[derive(Default)]
struct Fake(BTreeMap<u32, u32>);

impl RegisterAccess for Fake {
    type Error = core::convert::Infallible;

    fn read(&mut self, addr: u32) -> Result<u32, Self::Error> {
        Ok(self.get(addr))
    }

    fn write(&mut self, addr: u32, value: u32) -> Result<(), Self::Error> {
        self.0.insert(addr, value);
        Ok(())
    }
}

impl Fake {
    fn get(&self, addr: u32) -> u32 {
        self.0.get(&addr).copied().unwrap_or(0)
    }
}

/// `p` applied to a fresh register file.
fn applied(p: &Params) -> Fake {
    let mut f = Fake::default();
    p.apply(&mut f).unwrap();
    f
}

/// The data words written to `data` (the lens shading table words, in order).
fn table_words(p: &Params, data: u32) -> Vec<u32> {
    p.writes()
        .iter()
        .filter(|w| w.addr == data)
        .map(|w| w.value)
        .collect()
}

#[test]
fn measure_programs_the_ae_histogram_and_awb_windows() {
    let f = applied(&Params::new().measure(1280, 800));
    // Five blocks across and down; the AE blocks are even-sized.
    assert_eq!((f.get(AE_H_SIZE), f.get(AE_V_SIZE)), (256, 160));
    assert_eq!(f.get(AE_CTRL), AE_CTRL_LUMA_85_256);
    assert_eq!(f.get(HIST_PROP), (3 << 3) | HIST_MODE_Y);
    assert_eq!((f.get(HIST_H_SIZE), f.get(HIST_V_SIZE)), (256, 160));
    assert_eq!(f.get(HIST_WEIGHTS_0), HIST_WEIGHT_ONE_PACKED);
    assert_eq!(f.get(HIST_WEIGHT_44), 1);
    assert_eq!(f.get(ISP_AWB_PROP) & AWB_PROP_RGB_MEAS, AWB_PROP_RGB_MEAS);
    assert_eq!((f.get(AWB_H_SIZE), f.get(AWB_V_SIZE)), (1280, 800));
    // An odd frame rounds the blocks down to even.
    let f = applied(&Params::new().measure(1281, 799));
    assert_eq!((f.get(AE_H_SIZE), f.get(AE_V_SIZE)), (256, 158));
}

#[test]
fn black_level_is_fixed_and_clamped_to_the_field() {
    let f = applied(&Params::new().black_level([256, 0, 4000, 9000]));
    assert_eq!(f.get(BLS_CTRL), BLS_ENABLE);
    assert_eq!(f.get(BLS_A_FIXED), 256);
    assert_eq!(f.get(BLS_B_FIXED), 0);
    assert_eq!(f.get(BLS_C_FIXED), 4000);
    assert_eq!(f.get(BLS_D_FIXED), BLS_MAX as u32);
}

#[test]
fn white_balance_packs_10_bit_gains_and_clamps_them() {
    let f = applied(&Params::new().white_balance([1.5, 1.0, 2.0]));
    assert_eq!(f.get(ISP_AWB_GAIN_RB), (384 << 16) | 512);
    assert_eq!(f.get(ISP_AWB_GAIN_G), (256 << 16) | 256);
    // Past the 10-bit field (3.996) clamps; a zero or negative gain goes out as 1/256.
    let f = applied(&Params::new().white_balance([9.0, 0.0, -1.0]));
    assert_eq!(f.get(ISP_AWB_GAIN_RB), (1023 << 16) | 1);
    assert_eq!(f.get(ISP_AWB_GAIN_G), (1 << 16) | 1);
}

#[test]
fn colour_matrix_is_row_major_nine_bit_signed_and_clamped() {
    let m = [[1.5, -0.25, -0.25], [-0.25, 1.5, -0.25], [0.0, -0.5, 1.5]];
    let f = applied(&Params::new().colour_matrix(m, [0.0; 3]));
    assert_eq!(f.get(ISP_CC_COEFF_0), 192, "1.5 at 7 fractional bits");
    assert_eq!(f.get(ISP_CC_COEFF_0 + 4), 0x1E0, "-32 in nine bits");
    assert_eq!(
        f.get(ISP_CC_COEFF_0 + 4 * 7),
        0x1C0,
        "-64 at row 2, column 1"
    );
    // Out of the field: +255 and -256; the offsets clamp to their 12-bit field.
    let m = [[9.0, -9.0, 0.0], [0.0; 3], [0.0; 3]];
    let f = applied(&Params::new().colour_matrix(m, [5000.0, -5000.0, 0.4]));
    assert_eq!(f.get(ISP_CC_COEFF_0), 255);
    assert_eq!(f.get(ISP_CC_COEFF_0 + 4), 0x100, "-256");
    assert_eq!(f.get(ISP_CT_OFFSET_R), 2047);
    assert_eq!(f.get(ISP_CT_OFFSET_G), 0x800, "-2048");
    assert_eq!(f.get(ISP_CT_OFFSET_B), 0);
}

#[test]
fn gamma_is_17_points_with_a_10_bit_delta_and_enables_the_output() {
    let f = applied(&Params::new().gamma(|x| x));
    assert_eq!(f.get(ISP_GAMMA_OUT_MODE), GAMMA_OUT_EQU_SEGM);
    assert_eq!(f.get(ISP_GAMMA_OUT_Y_0), 0);
    assert_eq!(f.get(ISP_GAMMA_OUT_Y_0 + 4 * 8), 512);
    assert_eq!(f.get(ISP_GAMMA_OUT_Y_0 + 4 * 16), 1023);
    assert_ne!(f.get(ISP_CTRL) & CTRL_GAMMA_OUT_ENABLE, 0);
    // A step bigger than the 10-bit delta is spread over the points after it.
    let f = applied(&Params::new().gamma(|x| if x < 0.5 { 0.0 } else { 1.0 }));
    let y = |i: u32| f.get(ISP_GAMMA_OUT_Y_0 + 4 * i);
    assert_eq!((y(7), y(8), y(9), y(10)), (0, 511, 1022, 1023));
}

#[test]
fn lens_shading_resamples_to_the_vertices_of_the_packed_table() {
    // A 2x2 table: the far corner takes the far cell's gain, the middle the mean.
    let r = [1.0, 1.5, 2.0, 2.5];
    let flat = [1.0; 4];
    let p = Params::new().lens_shading(1280, 800, (2, 2), [&r, &flat, &flat]);
    let t = table_words(&p, LSC_R_TABLE_DATA);
    assert_eq!(t.len(), LSC_ROWS * LSC_WORDS_PER_ROW);
    // Word k holds row k / 9, samples 2 * (k % 9) and its next (low and high 12 bits).
    let sample =
        |row: usize, col: usize| (t[row * LSC_WORDS_PER_ROW + col / 2] >> (12 * (col % 2))) & 0xFFF;
    assert_eq!(sample(0, 0), 1024);
    assert_eq!(
        sample(8, 0),
        2048,
        "bottom-left vertex takes the bottom-left cell"
    );
    assert_eq!(sample(8, 8), 2560, "far corner takes the far cell");
    assert_eq!(sample(4, 4), 1792, "middle is the mean");
    assert_eq!(sample(0, 9), 1024, "beyond the vertices stays at unity");
    // The gains beyond the 12-bit sample's full scale clamp.
    let p = Params::new().lens_shading(1280, 800, (1, 1), [&[5.0], &[1.0], &[1.0]]);
    assert_eq!(table_words(&p, LSC_R_TABLE_DATA)[0] & 0xFFF, 4095);
    // Sector sizes split the frame in pixels; 1280 over eight is 160 each. Gradients are
    // 32768 / size.
    let f = applied(&p);
    assert_eq!(f.get(LSC_XSIZE_0), 160 | (160 << 16));
    assert_eq!(f.get(LSC_XSIZE_0 + 12), 160 | (160 << 16));
    assert_eq!(f.get(LSC_XGRAD_0), 205 | (205 << 16), "(32768 + 80) / 160");
    assert_eq!(f.get(LSC_CTRL), LSC_EN);
    // An odd frame: the sizes still sum to the frame.
    let f = applied(&Params::new().lens_shading(1281, 801, (1, 1), [&[1.0], &[1.0], &[1.0]]));
    let sum: u32 = (0..4u32)
        .flat_map(|n| {
            let w = f.get(LSC_XSIZE_0 + 4 * n);
            [w & 0x3FF, w >> 16]
        })
        .sum();
    assert_eq!(sum, 1281);
    // A table of the wrong size changes nothing.
    let p = Params::new().lens_shading(1280, 800, (3, 3), [&r, &flat, &flat]);
    assert!(p.writes().is_empty());
}

#[test]
fn apply_keeps_the_control_bits_it_does_not_own() {
    let mut f = Fake::default();
    f.write(ISP_CTRL, 0x1234).unwrap();
    Params::new().gamma(|x| x).apply(&mut f).unwrap();
    assert_eq!(
        f.get(ISP_CTRL),
        0x1234 | CTRL_GAMMA_OUT_ENABLE | CTRL_GEN_CFG_UPD
    );
}

#[test]
fn every_write_lands_inside_the_isp_blocks() {
    let p = Params::new()
        .measure(1280, 800)
        .black_level([256; 4])
        .white_balance([1.0; 3])
        .colour_matrix(
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            [0.0; 3],
        )
        .gamma(|x| x)
        .lens_shading(1280, 800, (2, 2), [&[1.0; 4], &[1.0; 4], &[1.0; 4]]);
    assert!(p.writes().len() > 600);
    for w in p.writes() {
        assert!((ISP_BASE..DNR3_BASE).contains(&w.addr), "{:#x}", w.addr);
    }
}

#[test]
fn the_register_map_matches_the_struct_offsets() {
    // Offsets from the members of `mrv_all_regs.h`.
    assert_eq!(AE_MEAN_0 + 4 * 24, AE_BASE + 0x74, "isp_exp_mean_44");
    assert_eq!(HIST_BIN_0 + 4 * 15, HIST_BASE + 0x50, "histogram result 15");
    assert_eq!(HIST_WEIGHT_44, HIST_BASE + 0x6C);
    assert_eq!(ISP_AWB_GAIN_RB, ISP_BASE + 0x13C);
    assert_eq!(ISP_CC_COEFF_0 + 4 * 8, ISP_BASE + 0x190, "isp_cc_coeff_8");
    assert_eq!(LSC_YGRAD_0 + 12, LSC_BASE + 0x40, "isp_lsc_ygrad_67");
    assert_eq!(LSC_YSIZE_0 + 12, LSC_BASE + 0x60, "isp_lsc_ysize_67");
    assert_eq!(AWB_V_SIZE, AWB_BASE + 0x10, "awb_meas_v_size");
    assert_eq!(BLS_D_FIXED, BLS_BASE + 0x34, "isp_bls_d_fixed");
}

#[test]
fn statistics_decode_what_the_registers_hold() {
    let mut f = Fake::default();
    // Block (x, y) reads 10 * y + x; the registers' upper bits are not part of the mean.
    for y in 0..5u32 {
        for x in 0..5u32 {
            f.0.insert(AE_MEAN_0 + 4 * (x + 5 * y), 10 * y + x);
        }
    }
    // A bin's count in 16 integer and 4 fractional bits; bit 24 is outside the field.
    for n in 0..16u32 {
        f.0.insert(HIST_BIN_0 + 4 * n, (n << 4) | 8 | (1 << 24));
    }
    // RGB measurement mode: red in 7..0, blue in 15..8, green in 23..16.
    f.0.insert(ISP_AWB_MEAN, (200 << 16) | (30 << 8) | 10);
    f.0.insert(ISP_AWB_WHITE_CNT, 1000);

    let raw = stats::read_raw(&mut f).unwrap();
    assert_eq!(raw.ae[7], 12, "block (x 2, y 1), row-major");
    assert_eq!(raw.hist[3], (3 << 4) | 8);
    assert_eq!(raw.awb_mean, [10, 200, 30]);
    assert_eq!(raw.awb_white, 1000);

    let mut out = Statistics::default();
    stats::statistics(&raw, 256 * 160, &mut out);
    let luma = out.luma.as_ref().unwrap();
    assert_eq!((luma.width, luma.height, luma.zones.len()), (5, 5, 25));
    let z = luma.zones[7];
    assert_eq!(z.counted, 256 * 160);
    assert!((z.y / f64::from(z.counted) - 12.0 / 255.0).abs() < 1e-9);
    assert_eq!((out.colour.width, out.colour.height), (1, 1));
    let c = out.colour.zones[0];
    assert_eq!(c.counted, 1000);
    assert!((c.g / 1000.0 - 200.0 / 255.0).abs() < 1e-9);
    assert_eq!(out.histogram.bins().len(), 16);
    assert_eq!(out.histogram.bins()[3], 3, "fraction dropped");
    assert!(out.before_wb && out.before_lsc);
}

#[test]
fn statistics_with_no_white_pixels_count_nothing() {
    let raw = Raw {
        awb_mean: [255, 255, 255],
        ..Raw::default()
    };
    let mut out = Statistics::default();
    stats::statistics(&raw, 100, &mut out);
    let c = out.colour.zones[0];
    assert_eq!((c.counted, c.r, c.g, c.b), (0, 0.0, 0.0, 0.0));
}

#[cfg(feature = "device")]
#[test]
fn ioctl_numbers_are_the_iocs_of_the_header() {
    use crate::device::*;
    // _IOC(dir, type, nr, size): write 1, read 2, read-write 3.
    const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
        (dir << 30) | (size << 16) | ((b'v' as u32) << 8) | nr
    }
    assert_eq!(VVCAM_ISP_RESET, ioc(1, 0x01, 4));
    assert_eq!(VVCAM_ISP_READ_REG, ioc(3, 0x02, 8));
    assert_eq!(VVCAM_ISP_WRITE_REG, ioc(1, 0x03, 8));
    assert_eq!(VVCAM_ISP_CLOSE_IRQ, ioc(1, 0x07, 4));
    assert_eq!(VVCAM_ISP_FE_START, ioc(1, 0x08, 16));
}
