//! Decompression / compression for HIPO records, plus the reusable scratch
//! buffer used everywhere records are read or written.
//!
//! HIPO uses raw LZ4 *block* format (not framed) and gzip. The block size is
//! known up front from the record header, which lets us write straight into
//! a caller-supplied buffer with no framing overhead.
//!
//! [`ScratchBuf`] is in the same module because its only consumers are
//! record decompression and record building. Records are 1–8 MB; events are
//! 1–10 KB. Allocating per record kills cache warmth and stresses the
//! allocator, so every reader and writer holds a single scratch and grows
//! it geometrically — after a few records, it stops growing and we never
//! allocate again on the hot path.

#![allow(dead_code)] // ScratchBuf helpers are wire-format scaffolding.

use std::io::{Read, Write};

use crate::error::{HipoError, Result};
use crate::wire::constants::CompressionType;

// Apple's `libcompression` framework — FFI surface used for the
// `lz4-apple` feature. macOS only; on other platforms the feature is a
// no-op and the cfg below resolves to false so the inline route is never
// compiled.
#[cfg(all(feature = "lz4-apple", target_os = "macos"))]
mod apple_compression {
    use std::os::raw::c_void;
    /// `COMPRESSION_LZ4_RAW` — block-format LZ4 with no leading header.
    /// Wire-format compatible with `LZ4_compress_default` output and
    /// therefore with HIPO records.
    pub const COMPRESSION_LZ4_RAW: u32 = 0x101;
    unsafe extern "C" {
        pub fn compression_decode_buffer(
            dst: *mut u8,
            dst_size: usize,
            src: *const u8,
            src_size: usize,
            scratch: *mut c_void,
            algorithm: u32,
        ) -> usize;
    }
}

// Link Apple's libcompression on macOS. It's a system library available
// via the dyld shared cache; no .dylib file needs to exist on disk on
// modern macOS — passing `-lcompression` resolves at link time and dyld
// looks it up at runtime.
#[cfg(all(feature = "lz4-apple", target_os = "macos"))]
#[link(name = "compression")]
unsafe extern "C" {}

/// Generous slack we allocate beyond the header-reported decompressed size.
///
/// The HIPO record header's `data_length` field doesn't always match what
/// the LZ4 stream actually produces — writers sometimes append small
/// amounts of padding inside the compressed payload. The C++ reader masks
/// the divergence by using `LZ4_decompress_safe`, which truncates silently.
/// We allocate enough headroom that lz4_flex's strict bounds checks don't
/// trip on real files. 64 KiB is well over any observed delta.
const DECOMPRESS_SLACK: usize = 64 * 1024;

/// Decompress `src` into `dst`. `dst.capacity()` must be at least the
/// expected decompressed length plus the per-record slack. On success
/// `dst.len()` reflects the bytes produced.
pub fn decompress(
    kind: CompressionType,
    src: &[u8],
    dst: &mut Vec<u8>,
    expected: usize,
) -> Result<()> {
    dst.clear();
    // Reject an implausibly large decompressed size *before* reserving, so a
    // tiny record whose header claims a huge `data_length` can't force a
    // multi-GB allocation (a cheap amplification DoS on untrusted input). LZ4
    // expands at most ~255x and DEFLATE ~1032x; the generous 1056x ceiling
    // never rejects a valid stream. `None` can't produce more than its input.
    let max_plausible = match kind {
        CompressionType::None => src.len(),
        _ => src
            .len()
            .saturating_mul(1056)
            .saturating_add(DECOMPRESS_SLACK),
    };
    if expected > max_plausible {
        return Err(HipoError::CorruptRecord {
            offset: 0,
            reason: "decompressed size implausibly large for compressed input",
        });
    }
    let need = expected + DECOMPRESS_SLACK;
    if dst.capacity() < need {
        dst.reserve_exact(need - dst.capacity());
    }

    match kind {
        CompressionType::None => {
            if src.len() < expected {
                return Err(HipoError::DecompressUnderflow {
                    produced: src.len(),
                    expected,
                });
            }
            dst.extend_from_slice(&src[..expected]);
            Ok(())
        }
        CompressionType::Lz4
        | CompressionType::Lz4Best
        | CompressionType::Lz4PerBank
        | CompressionType::Lz4PerColumn => {
            // The by-bank / per-column formats reach this point only when their
            // record decoders hand us a single inner LZ4 block; their
            // record-level wrappers always pass `Lz4` explicitly. Routing
            // the tags through here keeps the match exhaustive.
            // Apple's `compression_decode_buffer` returns 0 for both
            // "empty output" and "failure", so we can't distinguish them.
            // Short-circuit the empty case so the proptest with `data
            // = []` doesn't false-positive as a decompression failure.
            if expected == 0 {
                return Ok(());
            }
            // Real CLAS12 LZ4 streams sometimes decompress to slightly more
            // than `header.data_length`; the C++ reader masks this with
            // `LZ4_decompress_safe`'s silent truncation. We instead size the
            // destination based on the input — a worst-case LZ4 decode is
            // bounded by `255 * input_size` but realistic compression ratios
            // are <= ~30x. We allocate based on `max(expected*2, input*32)`.
            let bound = expected.saturating_mul(2).max(src.len().saturating_mul(32));
            if dst.capacity() < bound {
                dst.reserve_exact(bound - dst.capacity());
            }
            // Reserved spare capacity to decode into. We keep it as
            // `&mut [MaybeUninit<u8>]` and hand the backends a raw pointer +
            // length, so no `&mut [u8]` is ever fabricated over uninitialized
            // memory for the FFI backends (the default builds). Only the
            // pure-Rust `lz4_flex` backend needs a slice, and it is confined to
            // that branch. `set_len` below commits only the bytes produced.
            let spare = dst.spare_capacity_mut();
            let target_len = std::cmp::min(spare.len(), bound);
            let dst_ptr = spare.as_mut_ptr() as *mut u8;

            // Decompression backend priority (best to worst):
            //   1. Apple `libcompression` (Apple Silicon NEON-tuned)  [macOS only]
            //   2. C `liblz4`                                          [lz4-c feature]
            //   3. pure-Rust `lz4_flex`                                 [default]
            //
            // The output is the same — HIPO uses raw LZ4 blocks — so any
            // of these can decode any of the others' output.
            #[cfg(all(feature = "lz4-apple", target_os = "macos"))]
            let produced = {
                // SAFETY: `dst_ptr` points at `target_len` bytes of reserved
                // capacity we own; the function only writes (≤ target_len) and
                // never reads the destination. No reference over uninit memory.
                let n = unsafe {
                    apple_compression::compression_decode_buffer(
                        dst_ptr,
                        target_len,
                        src.as_ptr(),
                        src.len(),
                        std::ptr::null_mut(),
                        apple_compression::COMPRESSION_LZ4_RAW,
                    )
                };
                if n == 0 {
                    return Err(HipoError::Compression(
                        "lz4 decompress failed (apple libcompression)",
                    ));
                }
                n
            };
            #[cfg(all(
                feature = "lz4-c",
                not(all(feature = "lz4-apple", target_os = "macos"))
            ))]
            let produced = {
                // SAFETY: `dst_ptr`/`target_len` describe reserved capacity we
                // own; LZ4_decompress_safe writes ≤ target_len bytes, returns
                // -1 on overflow, and never reads the destination.
                let n = unsafe {
                    lz4_sys::LZ4_decompress_safe(
                        src.as_ptr() as *const core::ffi::c_char,
                        dst_ptr as *mut core::ffi::c_char,
                        src.len() as i32,
                        target_len as i32,
                    )
                };
                if n < 0 {
                    return Err(HipoError::Compression("lz4 decompress failed (C)"));
                }
                n as usize
            };
            #[cfg(not(any(all(feature = "lz4-apple", target_os = "macos"), feature = "lz4-c")))]
            let produced = {
                // SAFETY: `dst_ptr`/`target_len` describe the reserved spare
                // capacity we own; the fabricated `&mut [u8]` is confined to
                // this call and never escapes. lz4_flex is built with the
                // `safe-decode` feature (see Cargo.toml), whose `decompress_into`
                // writes forward and reads back-references only within
                // `[0, capacity)`. Its wild-copy (`copy_within`) can memmove a
                // few not-yet-written bytes past the write cursor, but that is a
                // byte-wise `ptr::copy` over `u8`, which is defined over
                // uninitialized memory (it propagates, never *interprets*, the
                // bytes); every *typed* read is of an already-written byte. So
                // `[0, produced)` is fully initialized on return and no
                // uninitialized value is ever produced. (This reasoning is
                // specific to `safe-decode`; the unsafe-decode path differs.)
                let target: &mut [u8] =
                    unsafe { std::slice::from_raw_parts_mut(dst_ptr, target_len) };
                lz4_flex::block::decompress_into(src, target)
                    .map_err(|_| HipoError::Compression("lz4 decompress failed"))?
            };

            if produced + DECOMPRESS_SLACK < expected {
                return Err(HipoError::DecompressUnderflow { produced, expected });
            }
            // SAFETY: the backend wrote `produced` valid u8s; we reserved
            // enough capacity above.
            unsafe { dst.set_len(produced) };
            Ok(())
        }
        CompressionType::Gzip => {
            // `read_to_end` appends inflated bytes into the pre-reserved
            // capacity, initializing only the bytes it actually reads (std uses
            // a `BorrowedBuf` internally) — sound *and* single-write: no
            // `&mut [u8]` over uninitialized memory and no whole-payload
            // zero-fill. `take` bounds the output to `expected + slack` so a
            // gzip bomb can't grow `dst` without limit (the up-front
            // `max_plausible` check bounds `expected`; this bounds the stream).
            let limit = (expected as u64).saturating_add(DECOMPRESS_SLACK as u64);
            let produced = flate2::read::GzDecoder::new(src)
                .take(limit)
                .read_to_end(dst)
                .map_err(HipoError::Io)?;
            if produced + DECOMPRESS_SLACK < expected {
                return Err(HipoError::DecompressUnderflow { produced, expected });
            }
            Ok(())
        }
    }
}

/// Decompress `src` into a caller-provided slice `dst`. Writes exactly
/// `dst.len()` bytes and returns the count produced (which must equal
/// `dst.len()` for LZ4 streams produced from inputs of that size).
///
/// This is the slice-only variant used by the chunked-record decoder:
/// the destination is a `split_at_mut` view into a single record-wide
/// buffer, so per-chunk inflate can run in parallel without owning a
/// separate `Vec` per chunk.
pub fn decompress_into_slice(kind: CompressionType, src: &[u8], dst: &mut [u8]) -> Result<usize> {
    let expected = dst.len();
    match kind {
        CompressionType::None => {
            if src.len() < expected {
                return Err(HipoError::DecompressUnderflow {
                    produced: src.len(),
                    expected,
                });
            }
            dst.copy_from_slice(&src[..expected]);
            Ok(expected)
        }
        CompressionType::Lz4
        | CompressionType::Lz4Best
        | CompressionType::Lz4PerBank
        | CompressionType::Lz4PerColumn => {
            if expected == 0 {
                return Ok(0);
            }
            #[cfg(all(feature = "lz4-apple", target_os = "macos"))]
            let produced = {
                // SAFETY: src/dst are valid; the function never reads
                // `dst` and writes ≤ `dst.len()` bytes.
                let n = unsafe {
                    apple_compression::compression_decode_buffer(
                        dst.as_mut_ptr(),
                        dst.len(),
                        src.as_ptr(),
                        src.len(),
                        std::ptr::null_mut(),
                        apple_compression::COMPRESSION_LZ4_RAW,
                    )
                };
                if n == 0 {
                    return Err(HipoError::Compression(
                        "lz4 decompress failed (apple libcompression)",
                    ));
                }
                n
            };
            #[cfg(all(
                feature = "lz4-c",
                not(all(feature = "lz4-apple", target_os = "macos"))
            ))]
            let produced = {
                // SAFETY: src/dst are `&[u8]` / `&mut [u8]`; C signature
                // takes raw pointers + sizes.
                let n = unsafe {
                    lz4_sys::LZ4_decompress_safe(
                        src.as_ptr() as *const core::ffi::c_char,
                        dst.as_mut_ptr() as *mut core::ffi::c_char,
                        src.len() as i32,
                        dst.len() as i32,
                    )
                };
                if n < 0 {
                    return Err(HipoError::Compression("lz4 decompress failed (C)"));
                }
                n as usize
            };
            #[cfg(not(any(all(feature = "lz4-apple", target_os = "macos"), feature = "lz4-c")))]
            let produced = lz4_flex::block::decompress_into(src, dst)
                .map_err(|_| HipoError::Compression("lz4 decompress failed"))?;

            if produced < expected {
                return Err(HipoError::DecompressUnderflow { produced, expected });
            }
            Ok(produced)
        }
        CompressionType::Gzip => {
            let mut decoder = flate2::read::GzDecoder::new(src);
            let mut filled = 0;
            while filled < expected {
                let n = decoder.read(&mut dst[filled..]).map_err(HipoError::Io)?;
                if n == 0 {
                    return Err(HipoError::DecompressUnderflow {
                        produced: filled,
                        expected,
                    });
                }
                filled += n;
            }
            Ok(filled)
        }
    }
}

/// Compress `src` into `dst`. Appends to `dst`; returns bytes written.
pub fn compress(kind: CompressionType, src: &[u8], dst: &mut Vec<u8>) -> Result<usize> {
    let start = dst.len();
    match kind {
        CompressionType::None => {
            dst.extend_from_slice(src);
        }
        CompressionType::Lz4
        | CompressionType::Lz4Best
        | CompressionType::Lz4PerBank
        | CompressionType::Lz4PerColumn => {
            // The by-bank / per-column formats are record-level extensions;
            // their inner compression units still flow through this same
            // code path with `Lz4`/`Lz4Best`. The tags route here to keep the
            // match exhaustive.
            //
            // Pure-Rust `lz4_flex` doesn't expose an HC (high-compression)
            // mode, so both Lz4 and Lz4Best produce the same output there.
            // With `lz4-c` enabled, `Lz4Best` routes to `LZ4_compress_HC`
            // for ≈ 10–15% smaller output (≈ 4× slower compression speed
            // but the writer thread isn't the bottleneck for parallel
            // copy).
            #[cfg(feature = "lz4-c")]
            let bound = unsafe { lz4_sys::LZ4_compressBound(src.len() as i32) } as usize;
            #[cfg(not(feature = "lz4-c"))]
            let bound = lz4_flex::block::get_maximum_output_size(src.len());

            dst.reserve(bound);
            let spare = dst.spare_capacity_mut();
            let spare: &mut [u8] = unsafe {
                std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, spare.len())
            };

            #[cfg(feature = "lz4-c")]
            let n = {
                // SAFETY: `src` / `spare` are `&[u8]` / `&mut [u8]` we own;
                // bound was queried from LZ4_compressBound. Compression
                // never reads `spare`.
                let n = unsafe {
                    if matches!(kind, CompressionType::Lz4Best) {
                        // Level 9 ≈ `LZ4HC_CLEVEL_OPT_MIN`, matching what
                        // CLAS12 / `hipo4` use.
                        lz4_sys::LZ4_compress_HC(
                            src.as_ptr() as *const core::ffi::c_char,
                            spare.as_mut_ptr() as *mut core::ffi::c_char,
                            src.len() as i32,
                            spare.len() as i32,
                            9,
                        )
                    } else {
                        lz4_sys::LZ4_compress_default(
                            src.as_ptr() as *const core::ffi::c_char,
                            spare.as_mut_ptr() as *mut core::ffi::c_char,
                            src.len() as i32,
                            spare.len() as i32,
                        )
                    }
                };
                if n <= 0 {
                    return Err(HipoError::Compression("lz4 compress failed (C)"));
                }
                n as usize
            };
            #[cfg(not(feature = "lz4-c"))]
            let n = lz4_flex::block::compress_into(src, spare)
                .map_err(|_| HipoError::Compression("lz4 compress failed"))?;

            unsafe { dst.set_len(start + n) };
        }
        CompressionType::Gzip => {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(src).map_err(HipoError::Io)?;
            let buf = enc.finish().map_err(HipoError::Io)?;
            dst.extend_from_slice(&buf);
        }
    }
    Ok(dst.len() - start)
}

// ---- ScratchBuf -----------------------------------------------------------

/// A `Vec<u8>` with a single helper that grows but never shrinks.
#[derive(Debug, Default)]
pub struct ScratchBuf {
    inner: Vec<u8>,
}

impl ScratchBuf {
    pub const fn new() -> Self {
        Self { inner: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: Vec::with_capacity(cap),
        }
    }

    /// Reserve at least `min` bytes of capacity, growing geometrically.
    /// After the call, `len() == 0` and `capacity() >= min`.
    #[inline]
    pub fn reset_with_capacity(&mut self, min: usize) {
        self.inner.clear();
        if self.inner.capacity() < min {
            let target = std::cmp::max(min, self.inner.capacity().saturating_mul(2));
            self.inner.reserve_exact(target);
        }
    }

    /// Borrow the underlying `Vec` so callers can pass it to compression.
    #[inline]
    pub fn vec_mut(&mut self) -> &mut Vec<u8> {
        &mut self.inner
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.inner
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn round_trip(kind: CompressionType, data: &[u8]) {
        let mut compressed = Vec::new();
        compress(kind, data, &mut compressed).unwrap();
        let mut out = Vec::with_capacity(data.len());
        decompress(kind, &compressed, &mut out, data.len()).unwrap();
        assert_eq!(&out[..], data);
    }

    #[test]
    fn roundtrip_none_empty() {
        round_trip(CompressionType::None, &[]);
    }

    // A tiny compressed input claiming a huge decompressed size must be
    // rejected before any large allocation (amplification-DoS guard). The
    // capacity of `dst` must stay small.
    #[test]
    fn rejects_implausible_decompressed_size() {
        let src = vec![0u8; 64]; // 64 compressed bytes
        for kind in [
            CompressionType::Lz4,
            CompressionType::Gzip,
            CompressionType::None,
        ] {
            let mut dst = Vec::new();
            let huge = 3_000_000_000usize; // ~3 GB
            let err = decompress(kind, &src, &mut dst, huge);
            assert!(err.is_err(), "{kind:?}: huge expected must Err");
            assert!(
                dst.capacity() < 16 * 1024 * 1024,
                "{kind:?}: must not allocate for an implausible size (cap = {})",
                dst.capacity()
            );
        }
    }

    #[test]
    fn roundtrip_lz4_small() {
        round_trip(
            CompressionType::Lz4,
            b"hello, world. hello, world. hello, world.",
        );
    }

    #[test]
    fn roundtrip_gzip_small() {
        round_trip(
            CompressionType::Gzip,
            b"hello, world. hello, world. hello, world.",
        );
    }

    #[test]
    fn scratch_grows_geometrically() {
        let mut s = ScratchBuf::new();
        s.reset_with_capacity(100);
        assert!(s.capacity() >= 100);
        s.reset_with_capacity(1000);
        assert!(s.capacity() >= 1000);
    }

    #[test]
    fn scratch_does_not_shrink() {
        let mut s = ScratchBuf::with_capacity(4096);
        s.reset_with_capacity(10);
        assert!(s.capacity() >= 4096);
    }

    #[test]
    fn slice_decompress_lz4_round_trip() {
        let src_data = b"hello, world. hello, world. hello, world.".to_vec();
        let mut compressed = Vec::new();
        compress(CompressionType::Lz4, &src_data, &mut compressed).unwrap();

        let mut out = vec![0u8; src_data.len()];
        let produced = decompress_into_slice(CompressionType::Lz4, &compressed, &mut out).unwrap();
        assert_eq!(produced, src_data.len());
        assert_eq!(out, src_data);
    }

    #[test]
    fn slice_decompress_none_round_trip() {
        let src_data = b"abc".to_vec();
        let mut out = vec![0u8; 3];
        let produced = decompress_into_slice(CompressionType::None, &src_data, &mut out).unwrap();
        assert_eq!(produced, 3);
        assert_eq!(out, src_data);
    }

    #[test]
    fn slice_decompress_lz4_empty() {
        // The chunked path should accept zero-byte chunks gracefully.
        let out: [u8; 0] = [];
        let mut buf: [u8; 0] = [];
        let produced = decompress_into_slice(CompressionType::Lz4, &out, &mut buf).unwrap();
        assert_eq!(produced, 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn proptest_lz4_roundtrip(data in proptest::collection::vec(any::<u8>(), 0..16384)) {
            round_trip(CompressionType::Lz4, &data);
        }

        #[test]
        fn proptest_gzip_roundtrip(data in proptest::collection::vec(any::<u8>(), 0..16384)) {
            round_trip(CompressionType::Gzip, &data);
        }
    }
}
