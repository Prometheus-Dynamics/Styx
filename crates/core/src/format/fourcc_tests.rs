use super::{BitDepth, ChromaSubsampling, FourCc, FrameStorageKind, PackedChannelOrder};

#[test]
fn classifies_compressed_formats() {
    assert!(FourCc::MJPG.is_compressed());
    assert!(FourCc::H264.is_compressed());
    assert!(!FourCc::RG24.is_compressed());
    assert!(FourCc::JPEG.is_jpeg_encoded());
    assert!(!FourCc::H264.is_jpeg_encoded());
}

#[test]
fn estimates_common_raw_frame_bytes() {
    assert_eq!(FourCc::RG24.estimated_frame_bytes(640, 480), Some(921_600));
    assert_eq!(FourCc::NV12.estimated_frame_bytes(4, 2), Some(12));
    assert_eq!(FourCc::MJPG.estimated_frame_bytes(4, 2), None);
}

#[test]
fn exposes_generic_packed_layout_info() {
    let info = FourCc::BGR3.layout_info();
    assert_eq!(info.storage, FrameStorageKind::Packed);
    assert_eq!(info.packed.unwrap().bytes_per_pixel, 3);
    assert_eq!(info.packed.unwrap().order, PackedChannelOrder::Bgr);
    assert_eq!(info.bit_depth, BitDepth::U8x3);
    assert_eq!(info.first_plane_visible_row_bytes(8), Some(24));
}

#[test]
fn exposes_generic_semiplanar_layout_info() {
    let info = FourCc::NV12.layout_info();
    assert_eq!(info.storage, FrameStorageKind::SemiPlanar);
    assert_eq!(info.planes.planes, 2);
    assert_eq!(info.planes.subsampling, Some(ChromaSubsampling::Cs420));
    assert_eq!(info.first_plane_visible_row_bytes(8), Some(8));
    assert_eq!(info.estimated_frame_bytes(8, 2), Some(24));
}

#[test]
fn classifies_raw_bayer_formats() {
    assert!(FourCc::RGGB.is_bayer_raw());
    assert!(FourCc::BGGR.is_bayer_raw());
    assert!(FourCc::GBRG.is_bayer_raw());
    assert!(FourCc::GRBG.is_bayer_raw());
    assert!(!FourCc::RG24.is_bayer_raw());
    // V4L2's 8-bit BGGR, the 16-bit containers and CSI-2 packed RAW10 / RAW12: row bytes.
    for (code, row) in [
        (*b"BA81", 1280),
        (*b"BG10", 2560),
        (*b"BYR2", 2560),
        (*b"pBAA", 1600),
        (*b"pRCC", 1920),
    ] {
        let code = FourCc::new(code);
        assert!(code.is_bayer_raw());
        let info = code.layout_info();
        assert_eq!(info.first_plane_visible_row_bytes(1280), Some(row));
        assert_eq!(info.estimated_frame_bytes(1280, 800), Some(row * 800));
    }
}
