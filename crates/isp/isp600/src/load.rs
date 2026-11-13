//! The load image: a zeroed buffer of the library's size, written at the library's offsets.

use std::fmt;

use crate::library::layout::{
    LOAD_BYTES, LOAD_REGIONS, LOAD_REGISTERS_OFFSET, REGISTERS, Refusal, check_load, reg,
};

/// Why a write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// The register offset is past the register image, or not a whole word.
    Register(usize),
    /// The region number is past the last load region, or is the register image (use
    /// [`LoadImage::set_word`] for it).
    Region(u8),
    /// The table's bytes are not the region's size.
    TableSize {
        region: u8,
        expected: usize,
        given: usize,
    },
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register(offset) => {
                write!(
                    f,
                    "register offset {offset:#x} is not a word in the register image"
                )
            }
            Self::Region(n) => write!(f, "load region {n} is not a table"),
            Self::TableSize {
                region,
                expected,
                given,
            } => write!(
                f,
                "load region {region} takes {expected} bytes, given {given}"
            ),
        }
    }
}

impl std::error::Error for WriteError {}

/// A load image under construction: [`LOAD_BYTES`] bytes, zero until written.
#[derive(Debug)]
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

    /// Writes `value` at word `offset` of the register image, little-endian. `offset` is from the
    /// start of the register image: a loaded register at [`reg`] offset `r` is
    /// `LOAD_REGISTERS_OFFSET + r`.
    pub fn set_word(&mut self, offset: usize, value: u32) -> Result<(), WriteError> {
        check_word(offset)?;
        self.bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Sets or clears [`module`] enables in the loaded `MODULE_BYPASS1` word. Bits outside
    /// `bits` keep their value.
    pub fn set_modules(&mut self, bits: u32, on: bool) -> Result<(), WriteError> {
        let offset = LOAD_REGISTERS_OFFSET + reg::MODULE_BYPASS1;
        check_word(offset)?;
        let mut word = [0u8; 4];
        word.copy_from_slice(&self.bytes[offset..offset + 4]);
        let mut value = u32::from_le_bytes(word);
        if on {
            value |= bits;
        } else {
            value &= !bits;
        }
        self.set_word(offset, value)
    }

    /// Copies `table` into table region `region` (1 to 4 in the library's numbering), which must
    /// be the region's size.
    pub fn set_table(&mut self, region: u8, table: &[u8]) -> Result<(), WriteError> {
        if region == 0 {
            return Err(WriteError::Region(region));
        }
        let r = *LOAD_REGIONS
            .get(usize::from(region))
            .ok_or(WriteError::Region(region))?;
        if table.len() != r.bytes {
            return Err(WriteError::TableSize {
                region,
                expected: r.bytes,
                given: table.len(),
            });
        }
        self.bytes[r.offset..r.end()].copy_from_slice(table);
        Ok(())
    }

    /// The image if it may go to the kernel: the library's `check_load`, which refuses any address
    /// word that is not zero.
    pub fn checked(&self) -> Result<crate::library::layout::Checked<'_>, Refusal> {
        check_load(&self.bytes[..])
    }
}

fn check_word(offset: usize) -> Result<(), WriteError> {
    if !offset.is_multiple_of(4) || offset + 4 > REGISTERS.bytes {
        return Err(WriteError::Register(offset));
    }
    Ok(())
}
