use super::*;

fn code(bytes: &[u8; 4]) -> FourCc {
    FourCc::new(*bytes)
}

#[test]
fn every_row_round_trips() {
    for m in MAPPINGS {
        assert_eq!(to_drm(m.fourcc), Some(m.drm), "{}", m.fourcc);
        assert_eq!(
            from_drm(m.drm.fourcc, m.drm.modifier),
            Some(m.fourcc),
            "{}",
            m.fourcc
        );
        assert_eq!(m.drm.registry(), Some(m.registry));
    }
}

#[test]
fn rows_are_one_to_one() {
    for (i, a) in MAPPINGS.iter().enumerate() {
        for b in &MAPPINGS[i + 1..] {
            assert_ne!(a.fourcc, b.fourcc, "{} listed twice", a.fourcc);
            assert_ne!(
                a.drm, b.drm,
                "{} and {} share a DRM format",
                a.fourcc, b.fourcc
            );
        }
    }
}

#[test]
fn aliases_map_as_their_code_and_come_back_as_it() {
    for &(alias, canonical) in ALIASES {
        assert!(
            MAPPINGS.iter().all(|m| m.fourcc != alias),
            "{alias} has a row"
        );
        let drm = to_drm(alias).unwrap_or_else(|| panic!("{alias} unmapped"));
        assert_eq!(Some(drm), to_drm(canonical));
        assert_eq!(from_drm(drm.fourcc, drm.modifier), Some(canonical));
    }
}

#[test]
fn every_styx_format_is_mapped_or_listed_as_unmapped() {
    let consts = [
        FourCc::R8,
        FourCc::GREY,
        FourCc::RG24,
        FourCc::RGB3,
        FourCc::BGR3,
        FourCc::BG24,
        FourCc::RGBA,
        FourCc::BGRA,
        FourCc::NV12,
        FourCc::NV21,
        FourCc::YUYV,
        FourCc::UYVY,
        FourCc::YVYU,
        FourCc::VYUY,
        FourCc::I420,
        FourCc::YU12,
        FourCc::YV12,
        FourCc::D32F,
        FourCc::R16,
        FourCc::XR24,
        FourCc::XB24,
        FourCc::RG48,
        FourCc::BG48,
        FourCc::MJPG,
        FourCc::JPEG,
        FourCc::H264,
        FourCc::H265,
        FourCc::HEVC,
        FourCc::RGGB,
        FourCc::BGGR,
        FourCc::GBRG,
        FourCc::GRBG,
    ];
    // Codes `FourCc` describes without a constant (raw Bayer, 4:2:2 / 4:4:4 YUV, RGB48).
    let named = [
        b"BA81", b"BG10", b"GB10", b"BA10", b"GR10", b"RG10", b"BG12", b"GB12", b"BA12", b"GR12",
        b"RG12", b"BG16", b"GB16", b"GR16", b"RG16", b"BYR2", b"pBAA", b"pGAA", b"pgAA", b"pRAA",
        b"pBCC", b"pGCC", b"pgCC", b"pRCC", b"RGB6", b"NV16", b"NV61", b"NV24", b"NV42", b"YU16",
        b"YV16", b"YU24", b"YV24",
    ];
    for fourcc in consts.into_iter().chain(named.map(code)) {
        assert!(
            to_drm(fourcc).is_some() != UNMAPPED.contains(&fourcc),
            "{fourcc}: mapped and unmapped, or neither"
        );
        // Every raw Bayer format Styx knows has a (libcamera) DRM code.
        if fourcc.is_bayer_raw() {
            assert_eq!(
                mapping(fourcc).map(|m| m.registry),
                Some(DrmRegistry::Libcamera)
            );
        }
    }
}

#[test]
fn spot_checks_against_drm_fourcc_h() {
    // Values as `drm_fourcc.h` computes them.
    assert_eq!(drm_fourcc(b"NV12"), 0x3231_564e);
    assert_eq!(drm_fourcc(b"BG24"), 0x3432_4742);
    let linear = |c: &[u8; 4]| Some(DrmFormat::linear(c));
    // V4L2 RGB24 (bytes R, G, B) is DRM BGR888, not RGB888.
    assert_eq!(to_drm(FourCc::RG24), linear(b"BG24"));
    assert_eq!(to_drm(FourCc::RGB3), linear(b"BG24"));
    assert_eq!(to_drm(FourCc::BG24), linear(b"RG24"));
    assert_eq!(to_drm(FourCc::RGBA), linear(b"AB24"));
    assert_eq!(to_drm(FourCc::BGRA), linear(b"AR24"));
    assert_eq!(to_drm(FourCc::XR24), linear(b"XR24"));
    assert_eq!(to_drm(FourCc::GREY), linear(b"GREY"));
    assert_eq!(to_drm(code(b"Y10 ")), linear(b"R10 "));
    assert_eq!(to_drm(code(b"Y16 ")), linear(b"R16 "));
    assert_eq!(to_drm(FourCc::YUYV), linear(b"YUYV"));
    assert_eq!(to_drm(FourCc::I420), linear(b"YU12"));
    // Bayer: libcamera's codes, the CSI-2 packing as a modifier.
    assert_eq!(to_drm(FourCc::BGGR), linear(b"BA81"));
    assert_eq!(to_drm(code(b"BA10")), linear(b"BA10"));
    assert_eq!(
        to_drm(code(b"pBAA")),
        Some(DrmFormat {
            fourcc: drm_fourcc(b"BG10"),
            modifier: 0x0b00_0000_0000_0001,
        })
    );
    assert_eq!(to_drm(code(b"Y10P")), Some(DrmFormat::csi2_packed(b"R10 ")));
    assert_eq!(to_drm(code(b"GR14")), linear(b"BA14"));
    // DRM `RGB6` is libcamera's 16-bit RGGB; V4L2 `RGB6` (48-bit RGB) is DRM BGR161616.
    assert_eq!(to_drm(code(b"RG16")), linear(b"RGB6"));
    assert_eq!(to_drm(code(b"RGB6")), linear(b"BG48"));
    assert_eq!(from_drm(drm_fourcc(b"RGB6"), 0), Some(code(b"RG16")));
    // Unpacked and packed RAW10 differ only in the modifier.
    assert_eq!(from_drm(drm_fourcc(b"BG10"), 0), Some(code(b"BG10")));
    assert_eq!(
        from_drm(drm_fourcc(b"BG10"), MIPI_FORMAT_MOD_CSI2_PACKED),
        Some(code(b"pBAA"))
    );
}

#[test]
fn no_drm_format_for_compressed_unknown_or_inexact() {
    for fourcc in UNMAPPED {
        assert_eq!(to_drm(*fourcc), None, "{fourcc}");
    }
    assert_eq!(to_drm(code(b"XXXX")), None);
    // The modifier must be the row's.
    assert_eq!(from_drm(drm_fourcc(b"NV12"), DRM_FORMAT_MOD_INVALID), None);
    assert_eq!(
        from_drm(drm_fourcc(b"NV12"), MIPI_FORMAT_MOD_CSI2_PACKED),
        None
    );
    assert_eq!(
        from_drm(drm_fourcc(b"RG16") | DRM_FORMAT_BIG_ENDIAN, 0),
        None
    );
    assert_eq!(from_drm(DRM_FORMAT_INVALID, 0), None);
    // DRM formats Styx has no code for.
    assert_eq!(from_drm(drm_fourcc(b"RG16"), 0), None); // RGB565
    assert_eq!(from_drm(drm_fourcc(b"P010"), 0), None);
}
