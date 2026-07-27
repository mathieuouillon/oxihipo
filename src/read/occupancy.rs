//! Per-bank occupancy — which banks carry data, in how many events, and how
//! many rows — **without inflating a single bank or column**.
//!
//! This is the cheapest possible answer to "what is in this file". Every number
//! is a function of a bank's per-event *extent*, which both columnar layouts
//! record in their bank-offset tables, so on `Lz4PerBank` and `Lz4PerColumn`
//! files no bank stream is decompressed at all.
//!
//! It exists because the obvious way to compute it is dramatically dearer.
//! Walking [`Chain::events`](super::chain::Chain::events) hands out an
//! `OwnedEvent` — a copy of every event's bytes — and enumerating its structures
//! on a per-column file first *synthesises* a whole event out of separate column
//! streams. Measured in the `hipo-tools` `banks` command over 20,000 events of a
//! CLAS12 run: 19 µs/event on `Lz4PerBank` and 26 µs/event on `Lz4PerColumn`,
//! against roughly 1.5 µs/event for reading the presence tables.
//!
//! `EventCtx` is not an escape route: it avoids the copy but cannot enumerate a
//! per-column record's banks, because that requires exactly the synthesis it
//! exists to avoid. An attempt to use it for this reported 4 banks out of 71 —
//! fast and silently wrong, which is what motivated putting the operation here
//! instead of leaving each caller to improvise.
//!
//! The classic layouts (`None`/`Lz4`/`Lz4Best`/`Gzip`) have no per-bank table,
//! so their records are decompressed and their events walked — but still without
//! a per-event allocation, so they gain too.

use std::ops::Range;

use rayon::prelude::*;

use crate::error::{HipoError, Result};
use crate::event::Event;
use crate::wire::by_bank::ByBankRecord;
use crate::wire::per_column::PerColumnRecord;
use crate::wire::record::Record;

use super::chain::Chain;
use super::filter::Filter;
use super::inner::FileInner;

/// How much of the chain one bank actually occupies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankOccupancy {
    pub name: String,
    /// Events carrying **at least one row** of this bank. A bank present with
    /// zero rows is not carrying data, which is the question being asked.
    pub events: u64,
    pub total_rows: u64,
    /// Largest row count seen in a single event — the number that sizes a
    /// buffer, and which an average hides.
    pub max_rows: u32,
}

/// Occupancy across the chain, plus the denominator it is measured against.
///
/// `events_scanned` is here because every useful presentation of this data is a
/// *rate* — "this bank is in 85% of events" — and the caller cannot recover the
/// denominator from the bank counts. `Chain::event_count` is not it: that is
/// pre-filter, so under a filter every percentage computed from it is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occupancy {
    /// Events actually visited, after the filter and the range.
    pub events_scanned: u64,
    /// One entry per schema, in dictionary order.
    pub banks: Vec<BankOccupancy>,
}

/// Accumulator: events scanned, then one slot per schema in dictionary order.
type Tally = (u64, Vec<(u64, u64, u32)>);

fn merge(into: &mut Tally, from: &Tally) {
    into.0 += from.0;
    for (dst, src) in into.1.iter_mut().zip(&from.1) {
        dst.0 += src.0;
        dst.1 += src.1;
        dst.2 = dst.2.max(src.2);
    }
}

fn note(tally: &mut Tally, slot: usize, rows: u32) {
    if rows == 0 {
        return;
    }
    let t = &mut tally.1[slot];
    t.0 += 1;
    t.1 += u64::from(rows);
    t.2 = t.2.max(rows);
}

#[inline]
fn in_range(range: Option<&Range<u64>>, idx: u64) -> bool {
    range.is_none_or(|r| r.contains(&idx))
}

impl Chain {
    /// Per-bank occupancy over the **filtered** chain, in dictionary order.
    ///
    /// `range` restricts to global event indices `[start, stop)` (`None` = the
    /// whole chain). `threads`: `0` = rayon's global pool, `1` = sequential, `n`
    /// = an `n`-thread pool. Honors the chain filter and record-tag pushdown.
    ///
    /// Banks that carry no data anywhere are included with zero counts, so a
    /// caller can tell "declared but never populated" from "not in the
    /// dictionary" — a distinction that matters when a bank's absence is the
    /// finding.
    ///
    /// On `Lz4PerBank` and `Lz4PerColumn` files nothing is decompressed beyond
    /// each record's header and offset tables. See the module docs for why this
    /// is not something a caller should build out of
    /// [`Chain::events`](Self::events).
    pub fn bank_occupancy(&self, range: Option<Range<u64>>, threads: usize) -> Result<Occupancy> {
        // Dictionary order, resolved once: `(group, item, row_size)` per slot.
        let schemas: Vec<(u16, u8, u32)> = self
            .schemas()
            .iter()
            .map(|s| (s.group(), s.item(), s.row_size()))
            .collect();
        let names: Vec<String> = self
            .schemas()
            .iter()
            .map(|s| s.name().to_string())
            .collect();
        if schemas.is_empty() {
            return Ok(Occupancy {
                events_scanned: 0,
                banks: Vec::new(),
            });
        }

        let tasks = self.build_tasks();
        let files = self.files_inner();
        let offsets = self.event_offsets();
        let filter = self.filter_ref();
        let filter_active = filter.is_some_and(Filter::is_active);
        let range = range.as_ref();

        let run_one = |record: &mut Record, read_buf: &mut Vec<u8>, fi: usize, ri: usize| {
            process_record(
                &files[fi],
                ri,
                offsets[fi],
                &schemas,
                filter,
                filter_active,
                range,
                record,
                read_buf,
            )
        };

        let tallies: Vec<Tally> = if threads == 1 {
            let mut record = Record::new();
            let mut read_buf = Vec::new();
            tasks
                .iter()
                .map(|&(fi, ri)| run_one(&mut record, &mut read_buf, fi, ri))
                .collect::<Result<_>>()?
        } else {
            let run = || {
                tasks
                    .par_iter()
                    .map_init(
                        || (Record::new(), Vec::new()),
                        |(record, read_buf), &(fi, ri)| run_one(record, read_buf, fi, ri),
                    )
                    .collect::<Result<Vec<_>>>()
            };
            if threads == 0 {
                run()?
            } else {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .map_err(|e| HipoError::ThreadPool(e.to_string()))?;
                pool.install(run)?
            }
        };

        let mut total: Tally = (0, vec![(0, 0, 0); schemas.len()]);
        for t in &tallies {
            merge(&mut total, t);
        }
        Ok(Occupancy {
            events_scanned: total.0,
            banks: names
                .into_iter()
                .zip(total.1)
                .map(|(name, (events, total_rows, max_rows))| BankOccupancy {
                    name,
                    events,
                    total_rows,
                    max_rows,
                })
                .collect(),
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn process_record(
    inner: &std::sync::Arc<FileInner>,
    ri: usize,
    file_base: u64,
    schemas: &[(u16, u8, u32)],
    filter: Option<&Filter>,
    filter_active: bool,
    range: Option<&Range<u64>>,
    record: &mut Record,
    read_buf: &mut Vec<u8>,
) -> Result<Tally> {
    let mut tally: Tally = (0, vec![(0, 0, 0); schemas.len()]);

    let span = &inner.index.records()[ri];
    let global_first = file_base + span.first_event;

    // Skip the whole record without any I/O when it cannot intersect the range,
    // the same early-out the columnar reader takes.
    if let Some(r) = range {
        let rec_end = global_first + u64::from(span.event_count);
        if rec_end <= r.start || global_first >= r.end {
            return Ok(tally);
        }
    }

    let header = inner.read_record_into(span.file_offset, read_buf)?;

    if header.compression.is_by_bank() {
        let rec = ByBankRecord::parse(read_buf)?;
        let idxs: Vec<Option<u32>> = schemas
            .iter()
            .map(|&(g, i, _)| rec.bank_index(g, i))
            .collect();
        for e in 0..rec.event_count() {
            if !in_range(range, global_first + u64::from(e)) {
                continue;
            }
            if filter_active && filter.is_some_and(|f| !f.check_by_bank(&rec, e)) {
                continue;
            }
            tally.0 += 1;
            for (slot, (&idx, &(_, _, row_size))) in idxs.iter().zip(schemas).enumerate() {
                let Some(b) = idx else { continue };
                // `has` is belt and braces: an absent bank has a zero byte
                // extent, so `note` would skip it anyway. Removing it passes
                // every test, which is stated rather than left to be discovered.
                if !rec.has(e, b) || row_size == 0 {
                    continue;
                }
                // Row count from the byte extent — no stream inflated.
                let len = rec.bank_byte_range(e, b).len() as u32;
                note(&mut tally, slot, len / row_size);
            }
        }
    } else if header.compression.is_per_column() {
        let rec = PerColumnRecord::parse(read_buf)?;
        let idxs: Vec<Option<u32>> = schemas
            .iter()
            .map(|&(g, i, _)| rec.bank_index(g, i))
            .collect();
        for e in 0..rec.event_count() {
            if !in_range(range, global_first + u64::from(e)) {
                continue;
            }
            if filter_active && filter.is_some_and(|f| !f.check_per_column(&rec, e)) {
                continue;
            }
            tally.0 += 1;
            for (slot, (&idx, &(_, _, row_size))) in idxs.iter().zip(schemas).enumerate() {
                let Some(b) = idx else { continue };
                if !rec.has(e, b) || row_size == 0 {
                    continue;
                }
                // `bank_size` is the bank's byte extent for this event; no
                // column stream is touched.
                note(&mut tally, slot, rec.bank_size(e, b) / row_size);
            }
        }
    } else {
        // Classic layouts keep no per-bank table, so the record is decompressed
        // and its events walked. Still no per-event allocation: the structure
        // list is read from borrowed bytes.
        record.load_with_header(read_buf, header, Some(&inner.dict))?;
        for e in 0..record.event_count() {
            if !in_range(range, global_first + u64::from(e)) {
                continue;
            }
            let Some(raw) = record.event(e) else { continue };
            let event = Event::new(raw);
            if filter_active && filter.is_some_and(|f| !f.check(&event)) {
                continue;
            }
            tally.0 += 1;
            for (h, data) in event.iter_structures() {
                // Linear over the dictionary rather than hashed: a CLAS12 event
                // carries far fewer structures than the dictionary has schemas,
                // and this avoids building a map per record.
                let Some(slot) = schemas
                    .iter()
                    .position(|&(g, i, _)| g == h.group && i == h.item)
                else {
                    continue;
                };
                let row_size = schemas[slot].2;
                if row_size == 0 {
                    continue;
                }
                note(&mut tally, slot, data.len() as u32 / row_size);
            }
        }
    }

    Ok(tally)
}
