//! The load image: the settings this block can be given, encoded with the Lemnos library's own
//! tuning writers into a zeroed buffer of the library's size, and checked before anything would
//! hand it to the kernel.
//!
//! What is written, and nothing else:
//!
//! - each of the three channels' black level enable and bit depths (`ISP_CHn_EXPAND_CFG0`);
//! - the module enables, the one BLC bit when a black level is set and no other bit;
//! - the temporal denoise record enable, when the caller asks for it.
//!
//! Every other word is zero, and so is every address and extent word; the kernel writes those.
//! Any setting outside that set is refused by [`crate::gaps`] before a word is written.

use styx_format::FourCc;
use styx_settings::IspSettings;

use crate::gaps::{self, Unavailable};
use crate::library::layout::{LOAD_BYTES, Refusal, check_load, module};
use crate::library::tuning::{self, ChannelExpand, expand};

/// The options that are the caller's, not the settings': nothing in [`IspSettings`] names them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// The temporal denoise's record enable (`d3d_rec_en`). Off by default: the temporal denoise
    /// tables are not written here, so the record stays off unless the caller states otherwise.
    pub temporal_record: bool,
}

/// Why a load image was not made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// A setting has no encoding here (see [`crate::gaps`]).
    Unavailable(Unavailable),
    /// The input format is not a Bayer raw format with a sample width, so no bit depth.
    Input(FourCc),
    /// The library's tuning writer refused a value.
    Tuning(tuning::Error),
    /// The library's check refused the image.
    Refused(Refusal),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(error) => write!(f, "{error}"),
            Self::Input(code) => write!(
                f,
                "input format {code:?} is not a Bayer raw format with a sample width"
            ),
            Self::Tuning(error) => write!(f, "the load image does not take the value: {error:?}"),
            Self::Refused(refusal) => write!(f, "{}", refusal_message(*refusal)),
        }
    }
}

impl std::error::Error for EncodeError {}

/// The words the library's check refuses, as a sentence. The address and extent words are the
/// kernel's: a caller's image never names them, so a refusal is a bug in the image's making.
#[must_use]
pub fn refusal_message(refusal: Refusal) -> String {
    match refusal {
        Refusal::Length => format!("the load image is not {LOAD_BYTES:#x} bytes"),
        Refusal::Address(offset) => {
            format!(
                "load image address word at {offset:#x} is not zero: addresses are the kernel's"
            )
        }
        Refusal::Extent(offset) => {
            format!("load image extent word at {offset:#x} is not zero: extents are the kernel's")
        }
    }
}

/// A load image under construction: [`LOAD_BYTES`] bytes, zero until a setter writes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadImage {
    bytes: Box<[u8; LOAD_BYTES]>,
}

impl Default for LoadImage {
    fn default() -> Self {
        Self::new()
    }
}

impl LoadImage {
    /// A zeroed image.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: Box::new([0; LOAD_BYTES]),
        }
    }

    /// The whole image.
    #[must_use]
    pub fn bytes(&self) -> &[u8; LOAD_BYTES] {
        &self.bytes
    }

    /// The image if it may go to the kernel: the library's `check_load`, which refuses any address
    /// or extent word that is not zero.
    pub fn checked(&self) -> Result<crate::library::layout::Checked<'_>, Refusal> {
        check_load(&self.bytes[..])
    }

    /// The library's writer over this image.
    fn writer(&mut self) -> tuning::Image<'_> {
        tuning::Image::new(&mut self.bytes)
    }
}

/// The load image for `settings` on a sensor whose raw format is `input`.
///
/// Refuses, before writing anything, a setting [`crate::gaps`] names as unavailable. The black
/// level enables the three channels together when any channel's black level is above zero: the
/// public headers give the enables per channel but no map from the Bayer channels to them, so
/// every channel takes the same enable. The black level's amount is not encoded anywhere in the
/// public headers, so the block applies whatever offset it already holds; see the crate docs.
/// Both bit depths are the input's sample width, so the block does not expand the samples.
pub fn encode(
    settings: &IspSettings,
    input: FourCc,
    options: Options,
) -> Result<LoadImage, EncodeError> {
    gaps::check(settings).map_err(EncodeError::Unavailable)?;
    let bits = input_bits(input)?;
    let black = settings.black_cfa().iter().any(|&level| level > 0.0);
    let mut image = LoadImage::new();
    {
        let mut writer = image.writer();
        for channel in 0..expand::CHANNELS {
            writer
                .set_channel_expand(
                    channel,
                    ChannelExpand {
                        black_level: black,
                        inverse_black_level: false,
                        input_bits: bits,
                        output_bits: bits,
                    },
                )
                .map_err(EncodeError::Tuning)?;
        }
        writer
            .set_modules(if black { module::BLC } else { 0 })
            .map_err(EncodeError::Tuning)?;
        writer.set_temporal_record(options.temporal_record);
    }
    image.checked().map_err(EncodeError::Refused)?;
    Ok(image)
}

/// The sample width of a Bayer raw format, which the block takes as both its input and output
/// bit depth.
fn input_bits(input: FourCc) -> Result<u8, EncodeError> {
    let info = input.info().ok_or(EncodeError::Input(input))?;
    let bits = info.sample_bits;
    if info.cfa.is_none() || bits == 0 || bits > expand::BITS_MAX {
        return Err(EncodeError::Input(input));
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_zero_extent_word_is_refused_and_named() {
        let mut image = LoadImage::new();
        // The input size word: LOAD_REGISTERS_OFFSET (0x100) + INPUT_SIZE (0x20).
        image.bytes[0x120] = 1;
        let refusal = image.checked().expect_err("refused");
        assert_eq!(refusal, Refusal::Extent(0x120));
        assert!(refusal_message(refusal).contains("extent word at 0x120"));
        assert_eq!(
            check_load(&image.bytes()[..]).err(),
            Some(Refusal::Extent(0x120))
        );
    }

    #[test]
    fn an_address_word_is_refused_and_named() {
        let mut image = LoadImage::new();
        // Control block 0's load address: AHB_OFFSET (0x20) + LOAD_ADDR (0x4).
        image.bytes[0x24] = 1;
        let refusal = image.checked().expect_err("refused");
        assert_eq!(refusal, Refusal::Address(0x24));
        assert!(refusal_message(refusal).contains("address word at 0x24"));
    }

    #[test]
    fn the_writer_puts_each_word_at_its_register_offset() {
        let mut image = LoadImage::new();
        image.writer().set_modules(module::BLC).expect("BLC");
        // LOAD_REGISTERS_OFFSET (0x100) + MODULE_BYPASS1 (0x34).
        assert_eq!(&image.bytes()[0x134..0x138], &module::BLC.to_le_bytes());
    }

    #[test]
    fn input_bits_come_from_the_format_and_only_bayer_has_them() {
        assert_eq!(input_bits(FourCc::RGGB10P).ok(), Some(10));
        assert_eq!(input_bits(FourCc::GRBG).ok(), Some(8));
        assert!(matches!(
            input_bits(FourCc::NV12),
            Err(EncodeError::Input(_))
        ));
    }
}
