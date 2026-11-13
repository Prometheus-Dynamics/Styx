//! Little-endian writes and reads at byte offsets in a structure the library lays out. The
//! offsets come from `core::mem::offset_of!` on the library's structures, so a write lands on the
//! field it names. A buffer is always the generation's size, so an offset of a field is in bounds.

#[inline]
pub(crate) fn put_u8(b: &mut [u8], at: usize, v: u8) {
    b[at] = v;
}

#[inline]
pub(crate) fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub(crate) fn put_i16(b: &mut [u8], at: usize, v: i16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub(crate) fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub(crate) fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub(crate) fn get_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

#[inline]
pub(crate) fn get_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
