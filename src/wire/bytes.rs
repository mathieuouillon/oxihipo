//! Low-level byte access utilities.
//!
//! HIPO files are little-endian on disk on every machine produced this
//! century. We keep a big-endian fallback for forensic reading (a record's
//! magic word tells us which is which).
//!
//! Every function here is `#[inline(always)]`. None of these helpers ever
//! allocate. They are designed to be the only path through which header
//! parsing reads bytes — that way, hot-path inlining is uniform.

#![allow(clippy::inline_always)]

use crate::wire::constants::{HEADER_MAGIC, HEADER_MAGIC_BE};

/// Read a little-endian `u32` from `buf[off..off+4]`.
///
/// Bounds-checked: an out-of-range `off` panics on the slice index rather
/// than reading out of bounds. `from_le_bytes` over a length-4 slice lowers
/// to a single (possibly unaligned) load in release, so this is zero-cost
/// versus the previous hand-rolled `read_unaligned` while removing the UB
/// hole the `debug_assert!`-only guard left open on corrupt input.
#[inline(always)]
pub fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().expect("4-byte window"))
}

#[inline(always)]
pub fn read_i32_le(buf: &[u8], off: usize) -> i32 {
    read_u32_le(buf, off) as i32
}

#[inline(always)]
pub fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().expect("8-byte window"))
}

#[inline(always)]
pub fn read_i16_le(buf: &[u8], off: usize) -> i16 {
    i16::from_le_bytes(buf[off..off + 2].try_into().expect("2-byte window"))
}

#[inline(always)]
pub fn read_f32_le(buf: &[u8], off: usize) -> f32 {
    f32::from_bits(read_u32_le(buf, off))
}

#[inline(always)]
pub fn read_f64_le(buf: &[u8], off: usize) -> f64 {
    f64::from_bits(read_u64_le(buf, off))
}

#[inline(always)]
pub fn write_u32_le(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline(always)]
pub fn write_u64_le(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Endianness {
    Little,
    Big,
}

impl Endianness {
    #[inline]
    pub const fn from_magic(magic: u32) -> Option<Self> {
        match magic {
            HEADER_MAGIC => Some(Self::Little),
            HEADER_MAGIC_BE => Some(Self::Big),
            _ => None,
        }
    }
}

/// Reinterpret `bytes` as `&[T]` of `count` elements without copying.
///
/// # Safety
/// - `bytes.len()` must be at least `count * size_of::<T>()`.
/// - `T` must be `Copy` and have no internal padding.
/// - On big-endian machines the slice is wrong-endian; supported targets
///   (x86_64, aarch64) are little-endian and this is zero-cost.
#[inline(always)]
pub unsafe fn cast_slice<T: Copy>(bytes: &[u8], count: usize) -> &[T] {
    debug_assert!(bytes.len() >= count * std::mem::size_of::<T>());
    debug_assert_eq!(
        bytes.as_ptr() as usize % std::mem::align_of::<T>(),
        0,
        "cast_slice requires aligned source"
    );
    unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const T, count) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_u32() {
        let mut buf = [0u8; 8];
        write_u32_le(&mut buf, 0, 0xDEAD_BEEF);
        write_u32_le(&mut buf, 4, 0x1234_5678);
        assert_eq!(read_u32_le(&buf, 0), 0xDEAD_BEEF);
        assert_eq!(read_u32_le(&buf, 4), 0x1234_5678);
    }

    #[test]
    fn round_trip_u64() {
        let mut buf = [0u8; 8];
        write_u64_le(&mut buf, 0, 0x0123_4567_89AB_CDEF);
        assert_eq!(read_u64_le(&buf, 0), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn endian_from_magic() {
        assert_eq!(
            Endianness::from_magic(HEADER_MAGIC),
            Some(Endianness::Little)
        );
        assert_eq!(
            Endianness::from_magic(HEADER_MAGIC_BE),
            Some(Endianness::Big)
        );
        assert_eq!(Endianness::from_magic(0xDEADBEEF), None);
    }

    #[test]
    fn unaligned_reads() {
        let buf = [0u8, 0xEF, 0xBE, 0xAD, 0xDE, 0u8, 0u8, 0u8];
        assert_eq!(read_u32_le(&buf, 1), 0xDEAD_BEEF);
    }
}
