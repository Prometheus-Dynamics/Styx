//! Raw frames between the front and back end compressed to 8 bits a sample (PiSP mode 1, the
//! one the temporal-denoise average uses): the front end compresses its raw output, the back
//! end decompresses its input. Half the raw buffers' memory and the back end's read traffic,
//! for a small loss of precision (measured on Raze #1: matches to sensor noise).

use lemnos_drivers_pisp::be::BackEnd;
use lemnos_drivers_pisp::fe::FrontEnd;
use lemnos_drivers_pisp::uapi::{CompressConfig, DecompressConfig, bayer_enable, fe_enable};

/// Subtracted before compression and added back after (the temporal-denoise buffers' value).
pub const RAW_COMPRESS_OFFSET: u16 = 2048;
/// Mode 1: companding.
pub const RAW_COMPRESS_MODE: u8 = 1;

/// The front end's raw output 0 compressed (its format must be the device's compressed one).
pub fn compress_raw_output(fe: &mut FrontEnd) {
    fe.set_compress(
        0,
        CompressConfig {
            offset: RAW_COMPRESS_OFFSET,
            pad: 0,
            mode: RAW_COMPRESS_MODE,
        },
    );
    fe.enable(fe_enable::compress(0), true);
}

/// The back end decompressing a compressed raw input, as [`compress_raw_output`] wrote it.
pub fn decompress_raw_input(be: &mut BackEnd) {
    let cfg = be.config_mut();
    cfg.decompress = DecompressConfig {
        offset: RAW_COMPRESS_OFFSET,
        pad: 0,
        mode: RAW_COMPRESS_MODE,
    };
    cfg.global.bayer_enables |= bayer_enable::DECOMPRESS;
}
