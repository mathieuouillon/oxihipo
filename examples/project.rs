//! Bank projection: write a file keeping only the banks you name.
//! Usage: project <src> <dst> <pattern[,pattern...]> [codec]
use oxihipo::{Chain, Compression, Result};
use std::env;

fn main() -> Result<()> {
    let mut a = env::args().skip(1);
    let src = a
        .next()
        .expect("usage: project <src> <dst> <patterns> [codec]");
    let dst = a.next().expect("missing <dst>");
    let spec = a.next().expect("missing <patterns>");
    let codec = match a.next().as_deref() {
        Some("none") => Compression::None,
        Some("lz4-per-bank") => Compression::Lz4PerBank,
        Some("lz4-per-column") => Compression::Lz4PerColumn,
        Some("lz4best") | None => Compression::Lz4Best,
        Some(other) => panic!("unknown codec {other}"),
    };

    let src_bytes = std::fs::metadata(&src)?.len();
    let chain = Chain::open(&src)?;
    let pats: Vec<&str> = spec.split(',').map(str::trim).collect();
    let s = chain.skim_banks(&dst, codec, &pats)?;

    println!("kept: {}", s.kept.join(", "));
    println!("events        {}", s.write.events);
    println!("dropped banks {}", s.dropped_structures);
    println!(
        "bytes         {} -> {}  ({:.3}% of source, {:.1}x smaller)",
        src_bytes,
        s.write.bytes,
        100.0 * s.write.bytes as f64 / src_bytes as f64,
        src_bytes as f64 / s.write.bytes as f64
    );
    for (from, to) in &s.dangling_refs {
        println!("warning: {from} indexes into {to}, which was dropped");
    }
    Ok(())
}
