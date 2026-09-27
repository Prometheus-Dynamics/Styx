//! MJPEG frames from arbitrary bytes, as a 1280x720 camera stream, through every decoder that
//! runs without system libraries.

#![no_main]

use libfuzzer_sys::fuzz_target;
use styx_codec::Codec;
use styx_codec::mjpeg_turbojpeg::TurbojpegDecoder;
use styx_codec::mjpeg_turbojpeg_luma::{LumaDecodeOptions, TurbojpegLumaDecoder};
use styx_codec::mjpeg_zune::ZuneMjpegDecoder;
use styx_core::prelude::*;

fn frame(jpeg: &[u8]) -> FrameLease {
    let mut buf = BufferPool::with_limits(1, jpeg.len().max(1), 0).lease();
    buf.resize(jpeg.len());
    buf.as_mut_slice().copy_from_slice(jpeg);
    let res = Resolution::new(1280, 720).unwrap();
    FrameLease::single_plane(
        FrameMeta::new(MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb), 0),
        buf,
        jpeg.len(),
        jpeg.len(),
    )
}

fuzz_target!(|data: &[u8]| {
    let decoders: [Box<dyn Codec>; 3] = [
        Box::new(TurbojpegDecoder::new(FourCc::RG24)),
        Box::new(TurbojpegLumaDecoder::with_options(LumaDecodeOptions {
            threads: 4,
            ..Default::default()
        })),
        Box::new(ZuneMjpegDecoder::new(FourCc::RG24)),
    ];
    for decoder in decoders {
        let _ = decoder.process(frame(data));
    }
});
