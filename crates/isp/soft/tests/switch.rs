//! A settings change between frames (as the 3A loop makes one: `SoftIspStage::apply` on the ISP
//! thread after a frame is published) is never applied to part of a frame: every picture is made
//! wholly with the settings before the change or wholly with those after it.

use crate::common::*;

use styx_format::FourCc;
use styx_format::{MediaFormat, Resolution};
use styx_frame::{FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_settings::IspSettings;
use styx_soft::SoftIspStage;
use styx_soft::params::{Demosaic, YuvMatrix};

/// A flat raw frame: every pixel at `code` (RAW10, CSI-2 packed), so a picture made with one set
/// of settings is one colour.
fn flat(pool: &FramePool<()>, id: u64, code: u16) -> FrameLease<()> {
    let layouts = packed_layouts(RAW, W, H, None, None).unwrap();
    let format = MediaFormat::with_defaults(RAW, Resolution::new(W, H).unwrap());
    let mut lease = pool
        .acquire(FrameEntry::new(id, 1_000 * id, format, layouts))
        .unwrap();
    let hi = (code >> 2) as u8;
    let lo = (code & 3) as u8;
    let group = [hi, hi, hi, hi, lo | lo << 2 | lo << 4 | lo << 6];
    {
        let mut planes = lease.write().unwrap();
        let mut plane = planes.plane(0).unwrap();
        for row in 0..H as usize {
            for (i, b) in plane
                .row_bytes_mut(row, RAW_STRIDE)
                .unwrap()
                .iter_mut()
                .enumerate()
            {
                *b = group[i % 5];
            }
        }
    }
    lease
}

fn neutral(red: f64) -> IspSettings {
    let mut s = IspSettings::neutral(64.0 / 1023.0);
    s.wb = [red, 1.0, 1.4];
    s
}

/// The one colour of an RGB3 picture; panics naming the first pixel that differs.
fn one_colour(picture: &FrameLease<()>, n: usize) -> [u8; 3] {
    let read = picture.read().unwrap();
    let plane = read.plane(0).unwrap();
    let first = [plane.data[0], plane.data[1], plane.data[2]];
    for y in 0..H as usize {
        let row = &plane.data[y * plane.stride..][..W as usize * 3];
        for (x, px) in row.as_chunks::<3>().0.iter().enumerate() {
            assert_eq!(
                *px, first,
                "picture {n}: pixel ({x}, {y}) is {px:?}, pixel (0, 0) {first:?}: two settings in one frame"
            );
        }
    }
    first
}

#[test]
fn a_settings_change_between_frames_is_never_applied_to_part_of_a_frame() {
    let raw_pool = picture_pool(RAW, W, H, 1);
    let raw = flat(&raw_pool, 1, 400);
    let out_pool = picture_pool(FourCc::RGB3, W, H, 2);
    let (a, b) = (neutral(1.0), neutral(2.2));
    let options = options(Demosaic::Bilinear, YuvMatrix::Bt709Limited, true);

    // Each settings' picture on its own.
    let mut expect = [[0u8; 3]; 2];
    for (k, s) in [&a, &b].into_iter().enumerate() {
        let mut stage = SoftIspStage::new(RAW, W, H, s, options.clone()).unwrap();
        let mut out = picture(&out_pool, FourCc::RGB3, W, H, 1);
        stage.process(&raw, &mut out, None).unwrap();
        expect[k] = one_colour(&out, k);
    }
    assert_ne!(
        expect[0], expect[1],
        "the two settings make different pictures"
    );

    // The loop's pattern: process, publish, apply the next settings, process the next frame. Every
    // picture is one colour, the one of the settings in force when it was made.
    let mut stage = SoftIspStage::new(RAW, W, H, &a, options).unwrap();
    let mut stats = styx_aaa::Statistics::default();
    for n in 0..24 {
        let k = (n / 3) % 2;
        let mut out = picture(&out_pool, FourCc::RGB3, W, H, 10 + n as u64);
        stage.process(&raw, &mut out, Some(&mut stats)).unwrap();
        assert_eq!(one_colour(&out, n), expect[k], "picture {n}");
        if n % 3 == 2 {
            stage.apply(if k == 0 { &b } else { &a }).unwrap();
        }
    }
}
