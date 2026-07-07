//! Head-to-head benchmark of LZ4 decoders on real CLAS12 record data.
//!
//! Pulls every compressed record body out of a HIPO file (just the
//! decompressed payload, not the index/event layer) and times three
//! decoders end-to-end:
//!
//! - `lz4_flex` (pure-Rust default)
//! - `lz4-sys` (the standard C `liblz4`)
//! - Apple's `libcompression` `COMPRESSION_LZ4_RAW` (hardware-accelerated
//!   on Apple Silicon)
//!
//! Build with `--features hipo/lz4-c` to compare lz4-sys; the apple path
//! is always linked when building on macOS.
//!
//! Usage:
//!   cargo run --release -p hipo --example bench_decoders --features hipo/lz4-c \
//!       -- <file.hipo> [iters]

use std::env;
use std::hint::black_box;
use std::time::Instant;

// Internal modules are private, so we re-parse record headers ourselves
// using the small public surface exposed by the crate. We can't reach
// `decode_record_into` from outside the crate, but we can read the file
// into memory and walk records by their headers.

// ----- Apple libcompression FFI ---------------------------------------------
#[cfg(target_os = "macos")]
mod apple {
    use std::os::raw::c_void;
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

// Wire-format constants we need to walk the file.
const RECORD_HEADER_SIZE: usize = 56;
const RH_RECORD_LENGTH: usize = 0;
const RH_HEADER_LENGTH: usize = 8;
const RH_BIT_INFO: usize = 20;
const RH_DATA_LENGTH: usize = 32;
const RH_COMP_WORD: usize = 36;
const FH_HEADER_LENGTH: usize = 8;
const FH_USER_HEADER_LEN: usize = 24;
const COMP_TYPE_SHIFT: u32 = 28;
const COMP_LENGTH_MASK: u32 = 0x0FFF_FFFF;
const BITINFO_PAD3_SHIFT: u32 = 24;
const BITINFO_PAD_MASK: u32 = 0x3;

fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

/// One record's compressed body + expected decompressed payload size.
struct RecordSample {
    compressed: Vec<u8>,
    expected: usize,
}

fn extract_records(path: &str, max: usize) -> Vec<RecordSample> {
    let data = std::fs::read(path).unwrap();

    // File header → header_length + user_header_length → first data record.
    let file_header_words = read_u32(&data, FH_HEADER_LENGTH);
    let user_header_bytes = read_u32(&data, FH_USER_HEADER_LEN);
    let mut off = (file_header_words as usize * 4) + user_header_bytes as usize;

    let mut out = Vec::new();
    while off + RECORD_HEADER_SIZE <= data.len() && out.len() < max {
        let header = &data[off..off + RECORD_HEADER_SIZE];
        let record_length_words = read_u32(header, RH_RECORD_LENGTH);
        let header_length_words = read_u32(header, RH_HEADER_LENGTH);
        let bit_info = read_u32(header, RH_BIT_INFO);
        let data_length = read_u32(header, RH_DATA_LENGTH);
        let comp_word = read_u32(header, RH_COMP_WORD);
        let comp_type = (comp_word >> COMP_TYPE_SHIFT) & 0xF;
        let compressed_words = comp_word & COMP_LENGTH_MASK;
        let compressed_data_padding =
            ((bit_info >> BITINFO_PAD3_SHIFT) & BITINFO_PAD_MASK) as usize;

        let record_bytes = (record_length_words as usize) * 4;
        let header_bytes = (header_length_words as usize) * 4;
        let compressed_bytes_padded = (compressed_words as usize) * 4;

        // Only sample LZ4-compressed records (type 1 or 2).
        if (comp_type == 1 || comp_type == 2) && data_length > 0 && compressed_bytes_padded > 0 {
            let lz4_start = off + header_bytes;
            // The compressed_words field includes trailing zero padding
            // to a 4-byte boundary. Strip it so we hand each decoder the
            // exact LZ4 stream — otherwise some implementations (notably
            // Apple's) will keep consuming the zeros as additional
            // literals and emit garbage.
            let lz4_end = lz4_start + compressed_bytes_padded - compressed_data_padding;
            if lz4_end <= data.len() && lz4_end > lz4_start {
                let expected = (data_length as usize) + 1024 * 64; // slack
                out.push(RecordSample {
                    compressed: data[lz4_start..lz4_end].to_vec(),
                    expected,
                });
            }
        }

        if record_bytes == 0 {
            break;
        }
        off += record_bytes;
    }
    out
}

#[derive(Clone, Copy, Debug)]
struct Stats {
    elapsed_secs: f64,
    bytes_out: u64,
    iters: u64,
}

impl Stats {
    fn gb_per_s(&self) -> f64 {
        self.bytes_out as f64 / 1e9 / self.elapsed_secs
    }
    fn ns_per_record(&self) -> f64 {
        self.elapsed_secs * 1e9 / self.iters as f64
    }
}

fn bench_lz4_flex(records: &[RecordSample], iters: usize) -> Stats {
    let mut dst = vec![0u8; 32 * 1024 * 1024];
    let start = Instant::now();
    let mut bytes_out: u64 = 0;
    let mut total_iters: u64 = 0;
    for _ in 0..iters {
        for r in records {
            let target = &mut dst[..r.expected];
            let n = lz4_flex::block::decompress_into(&r.compressed, target).unwrap_or(0);
            bytes_out += n as u64;
            total_iters += 1;
        }
    }
    let _ = black_box(dst);
    Stats {
        elapsed_secs: start.elapsed().as_secs_f64(),
        bytes_out,
        iters: total_iters,
    }
}

#[cfg(feature = "lz4-c")]
fn bench_lz4_sys(records: &[RecordSample], iters: usize) -> Stats {
    let mut dst = vec![0u8; 32 * 1024 * 1024];
    let start = Instant::now();
    let mut bytes_out: u64 = 0;
    let mut total_iters: u64 = 0;
    for _ in 0..iters {
        for r in records {
            let n = unsafe {
                lz4_sys::LZ4_decompress_safe(
                    r.compressed.as_ptr() as *const i8,
                    dst.as_mut_ptr() as *mut i8,
                    r.compressed.len() as i32,
                    r.expected as i32,
                )
            };
            if n > 0 {
                bytes_out += n as u64;
            }
            total_iters += 1;
        }
    }
    let _ = black_box(dst);
    Stats {
        elapsed_secs: start.elapsed().as_secs_f64(),
        bytes_out,
        iters: total_iters,
    }
}

#[cfg(target_os = "macos")]
fn bench_apple(records: &[RecordSample], iters: usize) -> Stats {
    let mut dst = vec![0u8; 32 * 1024 * 1024];
    let start = Instant::now();
    let mut bytes_out: u64 = 0;
    let mut total_iters: u64 = 0;
    for _ in 0..iters {
        for r in records {
            let n = unsafe {
                apple::compression_decode_buffer(
                    dst.as_mut_ptr(),
                    r.expected,
                    r.compressed.as_ptr(),
                    r.compressed.len(),
                    std::ptr::null_mut(),
                    apple::COMPRESSION_LZ4_RAW,
                )
            };
            bytes_out += n as u64;
            total_iters += 1;
        }
    }
    let _ = black_box(dst);
    Stats {
        elapsed_secs: start.elapsed().as_secs_f64(),
        bytes_out,
        iters: total_iters,
    }
}

fn print_row(name: &str, s: Stats) {
    println!(
        "  {:<24}  {:>8.3}s   {:>7.2} GB/s   {:>8.1} ns/record   {} MB output",
        name,
        s.elapsed_secs,
        s.gb_per_s(),
        s.ns_per_record(),
        s.bytes_out / 1024 / 1024,
    );
}

fn main() {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .expect("usage: bench_decoders <file.hipo> [iters]");
    let iters: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(50);

    println!("loading records from {path} …");
    let records = extract_records(&path, 1024);
    let total_compressed: usize = records.iter().map(|r| r.compressed.len()).sum();
    println!(
        "  extracted {} records, {:.1} MB compressed total",
        records.len(),
        total_compressed as f64 / 1024.0 / 1024.0,
    );
    println!("running each decoder for {iters} iterations\n");

    // Warmup each.
    bench_lz4_flex(&records, 1);
    #[cfg(feature = "lz4-c")]
    bench_lz4_sys(&records, 1);
    #[cfg(target_os = "macos")]
    bench_apple(&records, 1);

    let s_flex = bench_lz4_flex(&records, iters);
    #[cfg(feature = "lz4-c")]
    let s_sys = bench_lz4_sys(&records, iters);
    #[cfg(target_os = "macos")]
    let s_apple = bench_apple(&records, iters);

    println!("decoder                     elapsed   throughput   ns/record         output");
    print_row("lz4_flex (pure Rust)", s_flex);
    #[cfg(feature = "lz4-c")]
    print_row("lz4-sys (C liblz4)", s_sys);
    #[cfg(target_os = "macos")]
    print_row("apple libcompression", s_apple);
}

// Link Apple's libcompression on macOS. It's a system library in
// /usr/lib/system; passing `-lcompression` lets dyld resolve it via the
// shared cache (no actual .dylib file on disk in recent macOS).
#[cfg(target_os = "macos")]
#[link(name = "compression")]
unsafe extern "C" {}
