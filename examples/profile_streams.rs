//! Stream-size profile for the by-bank / per-column formats.
//!
//! The open question behind "intra-stream parallel inflate" is whether a
//! *single* bank's (or column's) stream is ever big enough, and dominant
//! enough, to be worth splitting into independently-inflatable sub-blocks.
//!
//! Note what the reader does **not** do today: nothing inflates two streams at
//! once. `for_each` parallelises over whole *records*, and within a record a
//! worker inflates streams lazily and serially. So the shares below bound a
//! hypothetical — what intra-record stream parallelism could reach — not any
//! current behaviour.
//!
//! This walks a real file and models what the writer would emit: it fills
//! records up to `max_record_bytes` of payload (32 MB is the default for both
//! extension formats) and reports, per record, the largest bank stream
//! (`Lz4PerBank`) and largest column stream (`Lz4PerColumn`) against the
//! record total.
//!
//! ```sh
//! cargo run --release --example profile_streams -- file.hipo [max_record_mb]
//! ```

use std::collections::HashMap;

use oxihipo::Chain;

/// One modelled record: total payload bytes, plus bytes per bank and per
/// (bank, column) stream.
#[derive(Default)]
struct Record {
    total: u64,
    per_bank: HashMap<String, u64>,
    per_column: HashMap<String, u64>,
}

fn main() -> oxihipo::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: profile_streams <file.hipo> [max_record_mb]");
        std::process::exit(2);
    });
    let cap_mb: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(32);
    let cap = cap_mb * 1024 * 1024;

    let chain = Chain::open(&path)?;
    let dict = chain.schemas();

    // Column byte widths, resolved once: width = element size * array length.
    let mut widths: HashMap<String, Vec<(String, u64)>> = HashMap::new();
    for s in dict.iter() {
        let cols = s
            .entries()
            .iter()
            .map(|e| (e.name.clone(), e.ty.size() as u64 * e.length as u64))
            .collect();
        widths.insert(s.name().to_string(), cols);
    }
    let bank_names: Vec<String> = widths.keys().cloned().collect();

    let mut records: Vec<Record> = Vec::new();
    let mut cur = Record::default();
    let mut events: u64 = 0;

    for ev in chain.events() {
        let ev = ev?;
        for name in &bank_names {
            let Some(bank) = ev.bank(name) else { continue };
            let rows = bank.rows() as u64;
            if rows == 0 {
                continue;
            }
            let cols = &widths[name];
            let bank_bytes: u64 = cols.iter().map(|(_, w)| w * rows).sum();
            *cur.per_bank.entry(name.clone()).or_default() += bank_bytes;
            for (col, w) in cols {
                *cur.per_column.entry(format!("{name}/{col}")).or_default() += w * rows;
            }
            cur.total += bank_bytes;
        }
        events += 1;
        if cur.total >= cap {
            records.push(std::mem::take(&mut cur));
        }
    }
    if cur.total > 0 {
        records.push(cur);
    }

    println!("file           {path}");
    println!("events         {events}");
    println!(
        "modelled       {} record(s) at <= {cap_mb} MB payload",
        records.len()
    );
    println!();

    // Per-record: how concentrated is the payload?
    println!("per record — largest single stream as a share of the record");
    println!(
        "{:>4}  {:>9}  {:>26} {:>9} {:>6}  {:>26} {:>9} {:>6}",
        "rec", "total MB", "largest bank", "MB", "share", "largest column", "MB", "share"
    );
    let mut worst_bank_share = 0.0f64;
    let mut worst_col_share = 0.0f64;
    for (i, r) in records.iter().enumerate() {
        let top = |m: &HashMap<String, u64>| -> (String, u64) {
            m.iter()
                .max_by_key(|(_, v)| **v)
                .map(|(k, v)| (k.clone(), *v))
                .unwrap_or_default()
        };
        let (bname, bbytes) = top(&r.per_bank);
        let (cname, cbytes) = top(&r.per_column);
        let bshare = bbytes as f64 / r.total as f64;
        let cshare = cbytes as f64 / r.total as f64;
        worst_bank_share = worst_bank_share.max(bshare);
        worst_col_share = worst_col_share.max(cshare);
        println!(
            "{:>4}  {:>9.1}  {:>26} {:>9.2} {:>5.1}%  {:>26} {:>9.2} {:>5.1}%",
            i,
            r.total as f64 / 1e6,
            trunc(&bname, 26),
            bbytes as f64 / 1e6,
            bshare * 100.0,
            trunc(&cname, 26),
            cbytes as f64 / 1e6,
            cshare * 100.0,
        );
    }

    // Amdahl bound on the hypothetical: if a record's streams *were* inflated
    // across cores, the record still could not finish sooner than its longest
    // single stream. That is the ceiling splitting one stream would have to
    // lift to matter.
    println!();
    println!(
        "worst-case largest bank stream:   {:.1}% of its record",
        worst_bank_share * 100.0
    );
    println!(
        "worst-case largest column stream: {:.1}% of its record",
        worst_col_share * 100.0
    );
    println!();
    println!(
        "If a record's streams were inflated across unlimited cores, the longest\n\
         stream would still cap it at:\n  \
         Lz4PerBank   <= {:.1}x\n  \
         Lz4PerColumn <= {:.1}x\n\
         (today they inflate serially within a record; for_each parallelises\n\
         over records instead — see examples/record_size_scaling.rs)",
        1.0 / worst_bank_share,
        1.0 / worst_col_share
    );

    // Aggregate concentration across the whole file.
    let mut all_banks: HashMap<String, u64> = HashMap::new();
    let mut all_cols: HashMap<String, u64> = HashMap::new();
    let mut grand = 0u64;
    for r in &records {
        for (k, v) in &r.per_bank {
            *all_banks.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &r.per_column {
            *all_cols.entry(k.clone()).or_default() += v;
        }
        grand += r.total;
    }
    println!();
    println!(
        "top 10 banks by payload share (whole file, {} banks total)",
        all_banks.len()
    );
    let mut v: Vec<_> = all_banks.into_iter().collect();
    v.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
    for (name, bytes) in v.iter().take(10) {
        println!(
            "  {:>30} {:>8.1} MB  {:>5.1}%",
            trunc(name, 30),
            *bytes as f64 / 1e6,
            *bytes as f64 / grand as f64 * 100.0
        );
    }
    println!();
    println!(
        "top 10 columns by payload share ({} streams total)",
        all_cols.len()
    );
    let mut v: Vec<_> = all_cols.into_iter().collect();
    v.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
    for (name, bytes) in v.iter().take(10) {
        println!(
            "  {:>30} {:>8.1} MB  {:>5.1}%",
            trunc(name, 30),
            *bytes as f64 / 1e6,
            *bytes as f64 / grand as f64 * 100.0
        );
    }
    Ok(())
}

fn trunc(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("…{}", &s[s.len() - n + 1..])
    }
}
