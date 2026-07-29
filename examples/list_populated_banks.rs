//! List the banks that are actually *populated* (≥ 1 row) in a HIPO file.
//!
//! A CLAS12 dictionary defines hundreds of bank schemas, but any given file
//! only fills a subset. This walks every event's structures and reports, per
//! bank, the number of events it appears in and the total row count —
//! revealing how many of the defined schemas carry real data.
//!
//! ```sh
//! cargo run --release --example list_populated_banks -- <file.hipo> [max_events]
//! ```

use std::collections::HashMap;
use std::env;

use oxihipo::{Chain, Result};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .expect("usage: list_populated_banks <file.hipo> [max_events]");
    let max: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);

    let chain = Chain::open(&path)?;
    let total_schemas = chain.schemas().len();
    let total_events = chain.event_count();

    // (group << 8 | item) -> (name, row_size). Dict::get_by_id is crate-private,
    // so build the reverse map from the public schema iterator.
    let mut by_id: HashMap<u32, (String, u32)> = HashMap::with_capacity(total_schemas);
    for s in chain.schemas().iter() {
        let key = (u32::from(s.group()) << 8) | u32::from(s.item());
        by_id.insert(key, (s.name().to_string(), s.row_size()));
    }

    let mut present_events: HashMap<u32, u64> = HashMap::new();
    let mut total_rows: HashMap<u32, u64> = HashMap::new();
    let mut scanned = 0u64;
    for ev in chain.events() {
        if scanned >= max {
            break;
        }
        let ev = ev?;
        for (h, data) in ev.structures() {
            let key = (u32::from(h.group) << 8) | u32::from(h.item);
            if let Some((_, row_size)) = by_id.get(&key) {
                let rows = if *row_size > 0 {
                    data.len() / *row_size as usize
                } else {
                    0
                };
                if rows > 0 {
                    *present_events.entry(key).or_insert(0) += 1;
                    *total_rows.entry(key).or_insert(0) += rows as u64;
                }
            }
        }
        scanned += 1;
    }

    let mut rows: Vec<(u32, u64, u64)> = present_events
        .iter()
        .map(|(&k, &evc)| (k, evc, *total_rows.get(&k).unwrap_or(&0)))
        .collect();
    rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

    eprintln!("file: {path}");
    eprintln!(
        "  {total_events} events in file, {total_schemas} schemas defined; scanned {scanned} events\n"
    );
    println!(
        "{} of {} schemas are populated (>=1 row); {} are empty/absent.\n",
        rows.len(),
        total_schemas,
        total_schemas - rows.len()
    );
    println!(
        "{:<26} {:>6} {:>11} {:>14} {:>7}",
        "bank", "g,i", "events", "total rows", "% evt"
    );
    println!("{}", "-".repeat(68));
    for (key, evc, nrows) in &rows {
        let (name, _) = &by_id[key];
        let (g, i) = (key >> 8, key & 0xff);
        let pct = 100.0 * (*evc as f64) / (scanned.max(1) as f64);
        println!(
            "{:<26} {:>6} {:>11} {:>14} {:>6.1}%",
            name,
            format!("{g},{i}"),
            evc,
            nrows,
            pct
        );
    }
    Ok(())
}
