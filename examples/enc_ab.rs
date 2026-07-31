//! Zstd x PerColumn with and without per-stream encodings, on a real file.
use oxihipo::{BankPatterns, Chain, Codec, Compression, Layout, Result, SkimOptions};
fn main() -> Result<()> {
    let src = std::env::args().nth(1).unwrap();
    let dir = std::env::temp_dir().join("oxi-encab");
    std::fs::create_dir_all(&dir)?;
    let all = BankPatterns::from_slice(&["*"])?;
    let base = Compression::new(Codec::Zstd, Layout::PerColumn);
    for (label, c) in [("plain", base), ("encoded", base.with_encodings())] {
        let out = dir.join(format!("{label}.hipo"));
        let t = std::time::Instant::now();
        let s =
            Chain::open(&src)?.skim_banks_with(&out, c, &all, SkimOptions::default(), |_| {})?;
        let w = t.elapsed().as_secs_f64();
        // read-back check
        let t = std::time::Instant::now();
        let mut rows = 0u64;
        for ev in Chain::open(&out)?.events() {
            let ev = ev?;
            let ctx = ev.ctx();
            for n in Chain::open(&src)?.schemas().iter().take(1) {
                if let Some(b) = ctx.bank(n.name()) {
                    rows += u64::from(b.rows());
                }
            }
        }
        println!(
            "  {label:<8} {:>12} bytes   write {w:.2}s   read {:.2}s   rows {rows}",
            s.write.bytes,
            t.elapsed().as_secs_f64()
        );
    }
    Ok(())
}
