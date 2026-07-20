//! Tag-and-skim: classify each event, write the tag into a new DST, and reread
//! it *by name* — the select→label→write→reread loop (`Chain::skim_tagged`).
//!
//! Usage:
//!   cargo run --release --example tag_and_skim -- [scratch-path-prefix]
//!
//! Self-contained: it writes a small source file, retags it through
//! `skim_tagged` (recording a `tag_flags!` name registry so the output is
//! self-describing), then reopens the tagged file and filters by name — no
//! external input needed. With no argument it works under the temp dir.

use oxihipo::{
    Chain, Compression, DataType, Dict, EventCtx, Filter, Result, Schema, TagSet, Writer,
};

// Named event categories. The bit positions are stable, and `Cat::NAMES` is
// what gets written into the file — so a downstream reader resolves the names
// without ever seeing this declaration.
oxihipo::tag_flags! {
    pub Cat {
        HasElectron = 0,
        HasProton   = 1,
        Empty       = 2,
    }
}

/// Label one event by the particles it carries — a plain `fn`, so it doubles as
/// a reusable classifier and coerces straight into the `skim_tagged` closure.
fn classify(ev: &EventCtx<'_>) -> TagSet {
    let Some(p) = ev.bank("REC::Particle") else {
        return Cat::Empty; // no particle bank at all
    };
    let mut tag = TagSet::EMPTY;
    for row in 0..p.rows() {
        match p.get::<i32>("pid", row) {
            11 => tag |= Cat::HasElectron,
            2212 => tag |= Cat::HasProton,
            _ => {}
        }
    }
    if tag.is_empty() { Cat::Empty } else { tag }
}

fn main() -> Result<()> {
    let base = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("oxihipo_tagdemo")
            .to_string_lossy()
            .into_owned()
    });
    let src = format!("{base}_src.hipo");
    let tagged = format!("{base}_tagged.hipo");

    write_source(&src)?;

    // Label every event and record the name registry in the output.
    let summary =
        Chain::open(&src)?.skim_tagged(&tagged, Compression::Lz4PerColumn, Cat::NAMES, classify)?;
    eprintln!("tagged {} events: {src} -> {tagged}", summary.events);
    eprintln!(
        "registry written into the file: {:?}",
        Chain::open(&tagged)?.tag_registry()
    );

    // Reread by name — the `tag_flags!` decl isn't needed here; the registry is
    // in the file, so `mask(name)` resolves straight from it.
    let out = Chain::open(&tagged)?;
    for name in ["HasElectron", "HasProton", "Empty"] {
        let mask = out
            .tag_registry()
            .mask(name)
            .expect("name in the file's registry");
        let survivors = out
            .clone()
            .with_filter(Filter::new().event_tag_any(mask))?
            .for_each(1, |_| {})?
            .events_yielded;
        eprintln!("  {name:12} -> {survivors} events");
    }
    Ok(())
}

/// Write a small source file: 12 events, each with `REC::Event`; every event
/// except every 4th carries `REC::Particle` (always an electron, plus a proton
/// on every 3rd), so the three categories are all exercised.
fn write_source(path: &str) -> Result<()> {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
    ));
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        31,
        [("pid".into(), DataType::Int, 1)],
    ));

    let mut w = Writer::create(path)
        .schemas(&d)
        .compression(Compression::Lz4PerColumn)
        .build()?;
    for i in 0..12i64 {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", i)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            if i % 4 != 3 {
                ev.bank("REC::Particle", |b| {
                    b.row(|r| {
                        r.set("pid", 11)?; // always an electron
                        Ok(())
                    })?;
                    if i % 3 == 0 {
                        b.row(|r| {
                            r.set("pid", 2212)?; // + a proton on every 3rd
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })?;
    }
    w.finish()?;
    Ok(())
}
