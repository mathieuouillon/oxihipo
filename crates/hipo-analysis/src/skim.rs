//! [`skim`] — write the events passing a predicate to a new `.hipo` file.

use std::path::Path;

use hipo::{Chain, EventCtx, Writer};

/// Counts returned by [`skim`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SkimStats {
    /// Events read from the input chain.
    pub events_in: u64,
    /// Events written to the output file.
    pub events_kept: u64,
}

/// Stream every event of `chain` past the `keep` predicate, writing the
/// ones it accepts verbatim to a new `.hipo` file at `out`.
///
/// This is a *sequential, streaming* operation — it never holds more than
/// one event in memory, so skimming a multi-hundred-gigabyte input is
/// safe. Pair it with `Chain::with_filter` for cheap record-level
/// pushdown before the per-event `keep` check.
pub fn skim<P>(chain: &Chain, out: impl AsRef<Path>, mut keep: P) -> hipo::Result<SkimStats>
where
    P: FnMut(&EventCtx<'_>) -> bool,
{
    let mut writer = Writer::create(out.as_ref())
        .schemas(chain.schemas())
        .build()?;
    let mut stats = SkimStats::default();
    for event in chain.events() {
        stats.events_in += 1;
        if keep(&event.ctx()) {
            writer.append_raw(event.bytes())?;
            stats.events_kept += 1;
        }
    }
    writer.finish()?;
    Ok(stats)
}
