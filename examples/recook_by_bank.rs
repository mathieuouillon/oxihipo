//! Re-cook one or many HIPO files into the `Lz4ByBank` format for
//! benchmarking true partial decompression.
//!
//! Single-file mode (default):
//!
//! ```sh
//! cargo run -p hipo --release --example recook_by_bank -- \
//!     /Users/.../in.hipo /tmp/out_by_bank.hipo
//! ```
//!
//! Batch mode — scans `<input_dir>` for `*.hipo` and recooks each in
//! parallel, mirroring filenames into `<output_dir>`:
//!
//! ```sh
//! cargo run -p hipo --release --example recook_by_bank -- --batch \
//!     /volatile/.../skim_slices/hipo \
//!     /scratch/$USER/skim_by_bank/
//! ```
//!
//! Files produced are not readable by the C++ `hipo4` reader (new
//! compression tag = 5).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rayon::prelude::*;

use oxihipo::{Chain, Compression, HipoError, Result, Writer};

enum Mode {
    Single {
        input: PathBuf,
        output: PathBuf,
    },
    Batch {
        input_dir: PathBuf,
        output_dir: PathBuf,
    },
}

fn parse_args() -> Mode {
    let mut args = std::env::args().skip(1);
    let first = args
        .next()
        .expect("usage: recook_by_bank [--batch] <in> <out>");
    if first == "--batch" {
        let input_dir = PathBuf::from(args.next().expect("batch mode: <input_dir> <output_dir>"));
        let output_dir = PathBuf::from(args.next().expect("batch mode: <input_dir> <output_dir>"));
        Mode::Batch {
            input_dir,
            output_dir,
        }
    } else {
        let output = PathBuf::from(args.next().expect("usage: recook_by_bank <in> <out>"));
        Mode::Single {
            input: PathBuf::from(first),
            output,
        }
    }
}

fn main() -> Result<()> {
    match parse_args() {
        Mode::Single { input, output } => recook_one(&input, &output, /*quiet=*/ false),
        Mode::Batch {
            input_dir,
            output_dir,
        } => recook_batch(&input_dir, &output_dir),
    }
}

fn recook_one(input: &Path, output: &Path, quiet: bool) -> Result<()> {
    let chain = Chain::open(input)?;
    let dict = chain.schemas().clone();
    let total_events = chain.event_count();
    if !quiet {
        eprintln!(
            "recook_by_bank: {} -> {} ({total_events} events)",
            input.display(),
            output.display(),
        );
    }

    let start = Instant::now();
    {
        let mut w = Writer::create(output)
            .schemas(&dict)
            .compression(Compression::Lz4ByBank)
            .build()?;
        let mut written: u64 = 0;
        let mut last_pct = -1i64;
        for ev in chain.events() {
            // For Lz4ByBank source records, `ev.bytes()` triggers
            // synthetic-bytes synthesis (decompressing every bank).
            // For Bytes-backed source records (the typical case when
            // recook-ing a vanilla Lz4 file), it's zero-copy.
            w.append_raw(ev.bytes())?;
            written += 1;
            if !quiet && let Some(pct) = (written * 100).checked_div(total_events) {
                let pct = pct as i64;
                if pct != last_pct && pct % 10 == 0 {
                    eprintln!("  {pct:3}%  ({written}/{total_events})");
                    last_pct = pct;
                }
            }
        }
        w.finish()?;
    }
    let elapsed = start.elapsed();

    let in_bytes = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0);
    let out_bytes = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    if !quiet {
        eprintln!(
            "done in {:.2}s — {} bytes → {} bytes ({:+.1}%)",
            elapsed.as_secs_f64(),
            in_bytes,
            out_bytes,
            100.0 * (out_bytes as f64 - in_bytes as f64) / (in_bytes as f64).max(1.0),
        );
    }
    Ok(())
}

fn recook_batch(input_dir: &Path, output_dir: &Path) -> Result<()> {
    if !input_dir.is_dir() {
        return Err(HipoError::Io(std::io::Error::other(format!(
            "batch mode: {} is not a directory",
            input_dir.display()
        ))));
    }
    std::fs::create_dir_all(output_dir).map_err(HipoError::Io)?;

    // Collect every *.hipo in the directory (non-recursive — matches the
    // usual JLab slice-set layout).
    let mut entries: Vec<PathBuf> = std::fs::read_dir(input_dir)
        .map_err(HipoError::Io)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension() == Some(OsStr::new("hipo")))
        .collect();
    entries.sort(); // deterministic ordering for the progress log

    let total = entries.len();
    if total == 0 {
        eprintln!("batch: no *.hipo files in {}", input_dir.display());
        return Ok(());
    }

    eprintln!(
        "batch: {total} file(s) in {} → {}",
        input_dir.display(),
        output_dir.display()
    );
    let done = AtomicU64::new(0);
    let total_in_bytes = AtomicU64::new(0);
    let total_out_bytes = AtomicU64::new(0);
    let start = Instant::now();

    // One file per rayon worker — file-level parallelism, not within
    // each recook. Each recook is itself a tight sequential loop that
    // already uses the lz4-c backend; parallelism here is the win.
    //
    // Errors per-file are reported but don't abort siblings.
    entries.par_iter().for_each(|input| {
        let fname = input.file_name().expect("regular file has a name");
        let output = output_dir.join(fname);
        let r = recook_one(input, &output, /*quiet=*/ true);
        let i = done.fetch_add(1, Ordering::Relaxed) + 1;
        match r {
            Ok(()) => {
                let in_bytes = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0);
                let out_bytes = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
                total_in_bytes.fetch_add(in_bytes, Ordering::Relaxed);
                total_out_bytes.fetch_add(out_bytes, Ordering::Relaxed);
                eprintln!(
                    "  [{i}/{total}] {} → {} ({:+.1}%)",
                    input.file_name().unwrap().to_string_lossy(),
                    human_bytes(out_bytes),
                    100.0 * (out_bytes as f64 - in_bytes as f64) / (in_bytes as f64).max(1.0),
                );
            }
            Err(e) => {
                eprintln!(
                    "  [{i}/{total}] FAILED {}: {e}",
                    input.file_name().unwrap().to_string_lossy()
                );
            }
        }
    });

    let elapsed = start.elapsed();
    let in_total = total_in_bytes.load(Ordering::Relaxed);
    let out_total = total_out_bytes.load(Ordering::Relaxed);
    eprintln!(
        "batch done in {:.1}s — {} → {} ({:+.1}%)",
        elapsed.as_secs_f64(),
        human_bytes(in_total),
        human_bytes(out_total),
        100.0 * (out_total as f64 - in_total as f64) / (in_total as f64).max(1.0),
    );
    Ok(())
}

fn human_bytes(b: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = b as f64;
    if b >= KIB * KIB * KIB {
        format!("{:.2} GiB", b / (KIB * KIB * KIB))
    } else if b >= KIB * KIB {
        format!("{:.1} MiB", b / (KIB * KIB))
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{b:.0} B")
    }
}
