//! Re-cook one or many HIPO files into the `Lz4ByBank` format for
//! benchmarking true partial decompression.
//!
//! Single-file mode (default):
//!
//! ```sh
//! cargo run --release --example recook_by_bank -- \
//!     /Users/.../in.hipo /tmp/out_by_bank.hipo
//! ```
//!
//! Batch mode — scans `<input_dir>` for `*.hipo` and recooks each in
//! parallel, mirroring filenames into `<output_dir>`:
//!
//! ```sh
//! cargo run --release --example recook_by_bank -- --batch \
//!     /volatile/.../skim_slices/hipo \
//!     /scratch/$USER/skim_by_bank/
//! ```
//!
//! Append `--merge <merged_file.hipo>` to also concatenate every
//! per-file output into a single combined `Lz4ByBank` file at the
//! end. Useful when you want one file per slice-set instead of N:
//!
//! ```sh
//! cargo run --release --example recook_by_bank -- --batch \
//!     /volatile/.../skim_slices/hipo \
//!     /scratch/$USER/skim_by_bank/ \
//!     --merge /scratch/$USER/skim_combined.hipo
//! ```
//!
//! The per-file outputs are kept after the merge (you can delete
//! `<output_dir>` yourself once the merged file is verified).
//!
//! Output is written in the `Lz4ByBankV2` format (LZ4-HC bank streams plus a
//! compressed directory). Files produced are not readable by the C++ `hipo4`
//! reader (Rust-only compression tag 6).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use kdam::{BarExt, tqdm};
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
        /// Optional `--merge <path>`: after recooking each input into
        /// `output_dir`, concatenate every per-file output into one
        /// combined `Lz4ByBankV2` file at `path`.
        merge: Option<PathBuf>,
    },
}

fn parse_args() -> (Mode, Compression) {
    const USAGE: &str = "usage: recook_by_bank [--batch] <in> <out> [--merge <merged_file>]";
    let compression = Compression::Lz4ByBankV2;
    let mut batch = false;
    let mut merge: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(tok) = args.next() {
        match tok.as_str() {
            "--batch" => batch = true,
            "--merge" => {
                merge = Some(PathBuf::from(
                    args.next()
                        .expect("--merge: needs a path (--merge <merged_file>)"),
                ));
            }
            other if other.starts_with("--") => panic!("unknown flag {other:?} — {USAGE}"),
            _ => positional.push(tok),
        }
    }
    let input = PathBuf::from(positional.first().cloned().expect(USAGE));
    let output = PathBuf::from(positional.get(1).cloned().expect(USAGE));
    let mode = if batch {
        Mode::Batch {
            input_dir: input,
            output_dir: output,
            merge,
        }
    } else {
        Mode::Single { input, output }
    };
    (mode, compression)
}

fn main() -> Result<()> {
    let (mode, compression) = parse_args();
    match mode {
        Mode::Single { input, output } => {
            recook_one(&input, &output, /*quiet=*/ false, compression)
        }
        Mode::Batch {
            input_dir,
            output_dir,
            merge,
        } => recook_batch(&input_dir, &output_dir, merge.as_deref(), compression),
    }
}

fn recook_one(input: &Path, output: &Path, quiet: bool, compression: Compression) -> Result<()> {
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
            .compression(compression)
            .build()?;
        // tqdm-style bar over the scan; `disable = quiet` silences it in
        // batch mode, where the caller shows a per-file bar instead.
        for ev in tqdm!(
            chain.events().map(Result::unwrap),
            total = total_events as usize,
            desc = "recook",
            unit = "ev",
            unit_scale = true,
            disable = quiet
        ) {
            // For Lz4ByBank source records, `ev.bytes()` triggers
            // synthetic-bytes synthesis (decompressing every bank).
            // For Bytes-backed source records (the typical case when
            // recook-ing a vanilla Lz4 file), it's zero-copy.
            w.append_raw(ev.bytes())?;
        }
        w.finish()?;
    }
    if !quiet {
        eprintln!();
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

fn recook_batch(
    input_dir: &Path,
    output_dir: &Path,
    merge: Option<&Path>,
    compression: Compression,
) -> Result<()> {
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
    let total_in_bytes = AtomicU64::new(0);
    let total_out_bytes = AtomicU64::new(0);
    let start = Instant::now();

    // One file per rayon worker — file-level parallelism, not within each
    // recook (each recook is its own tight sequential loop). A shared bar
    // advances as files complete; the per-recook bars are disabled (quiet).
    //
    // Errors per-file are reported but don't abort siblings.
    let pb = Mutex::new(tqdm!(total = total, desc = "recook batch", unit = "file"));
    entries.par_iter().for_each(|input| {
        let fname = input.file_name().expect("regular file has a name");
        let output = output_dir.join(fname);
        let r = recook_one(input, &output, /*quiet=*/ true, compression);
        match r {
            Ok(()) => {
                let in_bytes = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0);
                let out_bytes = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
                total_in_bytes.fetch_add(in_bytes, Ordering::Relaxed);
                total_out_bytes.fetch_add(out_bytes, Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!(
                    "  FAILED {}: {e}",
                    input.file_name().unwrap().to_string_lossy()
                );
            }
        }
        if let Ok(mut b) = pb.lock() {
            let _ = b.update(1);
        }
    });
    if let Ok(mut b) = pb.lock() {
        let _ = b.refresh();
    }
    eprintln!();

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

    // Optional merge phase: glue every successful per-file output into
    // one combined `Lz4ByBank` file. Skips files that failed (no output
    // on disk) so a partial batch still produces a usable merged file.
    if let Some(merged_path) = merge {
        let outputs: Vec<PathBuf> = entries
            .iter()
            .map(|input| output_dir.join(input.file_name().expect("regular file has a name")))
            .filter(|p| p.is_file())
            .collect();
        if outputs.is_empty() {
            eprintln!("merge: no per-file outputs to combine — skipping");
        } else {
            merge_into(&outputs, merged_path, compression)?;
        }
    }
    Ok(())
}

/// Open `inputs` as a single `Chain` and write every event to `merged`
/// as `Lz4ByBank`. All inputs must share the same dict (enforced by
/// `Chain::open`, which is the natural guarantee when they came
/// from the same upstream slice-set).
fn merge_into(inputs: &[PathBuf], merged: &Path, compression: Compression) -> Result<()> {
    eprintln!("merge: {} file(s) → {}", inputs.len(), merged.display());
    let chain = Chain::open(inputs)?;
    let dict = chain.schemas().clone();
    let total_events = chain.event_count();
    let start = Instant::now();
    {
        let mut w = Writer::create(merged)
            .schemas(&dict)
            .compression(compression)
            .build()?;
        for ev in tqdm!(
            chain.events().map(Result::unwrap),
            total = total_events as usize,
            desc = "merge",
            unit = "ev",
            unit_scale = true
        ) {
            // Same caveat as `recook_one`: when the source is already
            // `Lz4ByBank`, `ev.bytes()` synthesises (decompressing every
            // bank in the event); for `Bytes`-backed sources it's
            // zero-copy.
            w.append_raw(ev.bytes())?;
        }
        w.finish()?;
    }
    eprintln!();
    let elapsed = start.elapsed();
    let total_in: u64 = inputs
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();
    let merged_bytes = std::fs::metadata(merged).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "merge done in {:.1}s — {} → {} ({:+.1}%)",
        elapsed.as_secs_f64(),
        human_bytes(total_in),
        human_bytes(merged_bytes),
        100.0 * (merged_bytes as f64 - total_in as f64) / (total_in as f64).max(1.0),
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
