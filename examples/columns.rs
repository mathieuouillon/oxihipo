//! Columnar reads with [`Chain::read_columns`] — the bulk materializer that
//! backs the Python binding. One pass over the (optionally filtered) chain
//! gathers each requested `(bank, column)` into a flat values buffer plus one
//! shared `i64` offsets vector per bank (an Awkward `ListOffsetArray` layout).
//!
//! Run:
//! ```text
//! cargo run --example columns -- <file|dir|glob> [BANK] [COLUMN]
//! ```

use oxihipo::{Chain, ColumnData, Result};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(src) = args.next() else {
        eprintln!("usage: columns <file|dir|glob> [bank] [column]");
        std::process::exit(2);
    };
    let bank = args.next().unwrap_or_else(|| "REC::Particle".into());
    let column = args.next().unwrap_or_else(|| "px".into());

    let chain = Chain::open(src)?;
    println!(
        "{} events across {} file(s)",
        chain.event_count(),
        chain.file_count()
    );

    // Bulk columnar read: `threads = 0` uses all cores; `None` = the whole
    // chain (pass `Some(start..stop)` for a global event-index range).
    let cols = [column.as_str()];
    let selection: &[(&str, &[&str])] = &[(bank.as_str(), &cols)];
    let buffers = chain.read_columns(selection, None, 0)?;
    let bank_buf = &buffers[0];

    println!(
        "\n{}: {} events, {} rows total",
        bank_buf.bank,
        bank_buf.event_count(),
        bank_buf.total_rows(),
    );
    println!(
        "offsets[..6] = {:?}",
        &bank_buf.offsets[..bank_buf.offsets.len().min(6)]
    );

    let col = &bank_buf.columns[0];
    println!(
        "column {:?}: type {:?}, inner_len {}, {} flat values",
        col.name,
        col.data_type,
        col.inner_len,
        col.data.len(),
    );

    // `ColumnData` is byte-typed so one pass can carry columns of different
    // element types; match to get the concrete Rust slice.
    let head = 8;
    match &col.data {
        ColumnData::F32(v) => println!("first values: {:?}", &v[..v.len().min(head)]),
        ColumnData::F64(v) => println!("first values: {:?}", &v[..v.len().min(head)]),
        ColumnData::I32(v) => println!("first values: {:?}", &v[..v.len().min(head)]),
        ColumnData::I64(v) => println!("first values: {:?}", &v[..v.len().min(head)]),
        ColumnData::I16(v) => println!("first values: {:?}", &v[..v.len().min(head)]),
        ColumnData::I8(v) => println!("first values: {:?}", &v[..v.len().min(head)]),
    }

    Ok(())
}
