//! File-level event index — maps a global event number to (record, local).
//!
//! Built once at file-open time from the footer's index array; lookups are
//! O(log records) via binary search.

use crate::error::{HipoError, Result};

#[derive(Debug, Clone, Copy)]
pub struct RecordSpan {
    pub file_offset: u64,
    pub record_length: u64,
    pub event_count: u32,
    /// Cumulative event count *before* this record.
    pub first_event: u64,
}

#[derive(Debug, Clone, Default)]
pub struct FileEventIndex {
    records: Vec<RecordSpan>,
    total_events: u64,
}

impl FileEventIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, file_offset: u64, record_length: u64, event_count: u32) {
        let first = self.total_events;
        self.records.push(RecordSpan {
            file_offset,
            record_length,
            event_count,
            first_event: first,
        });
        self.total_events += u64::from(event_count);
    }

    pub fn total_events(&self) -> u64 {
        self.total_events
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    pub fn records(&self) -> &[RecordSpan] {
        &self.records
    }

    /// Locate the `(record index, local event index)` for a global event
    /// number.
    pub fn locate(&self, global: u64) -> Option<(usize, u32)> {
        if global >= self.total_events {
            return None;
        }
        let idx = self
            .records
            .partition_point(|r| r.first_event <= global)
            .saturating_sub(1);
        let span = &self.records[idx];
        let local = (global - span.first_event) as u32;
        Some((idx, local))
    }

    pub fn record(&self, idx: usize) -> Option<&RecordSpan> {
        self.records.get(idx)
    }

    pub fn from_columns(positions: &[i64], lengths: &[i32], entries: &[i32]) -> Result<Self> {
        if positions.len() != lengths.len() || positions.len() != entries.len() {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "trailer index columns have mismatched lengths",
            });
        }
        let mut idx = Self::new();
        for ((&p, &l), &e) in positions.iter().zip(lengths).zip(entries) {
            idx.push(p as u64, l as u64, e as u32);
        }
        Ok(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_record_index() -> FileEventIndex {
        let mut idx = FileEventIndex::new();
        idx.push(56, 4096, 100);
        idx.push(56 + 4096, 4096, 50);
        idx.push(56 + 8192, 8192, 200);
        idx
    }

    #[test]
    fn totals() {
        let idx = three_record_index();
        assert_eq!(idx.total_events(), 350);
        assert_eq!(idx.record_count(), 3);
    }

    #[test]
    fn locate_first() {
        let idx = three_record_index();
        assert_eq!(idx.locate(0), Some((0, 0)));
        assert_eq!(idx.locate(99), Some((0, 99)));
    }

    #[test]
    fn locate_boundary() {
        let idx = three_record_index();
        assert_eq!(idx.locate(100), Some((1, 0)));
        assert_eq!(idx.locate(149), Some((1, 49)));
        assert_eq!(idx.locate(150), Some((2, 0)));
    }

    #[test]
    fn locate_last() {
        let idx = three_record_index();
        assert_eq!(idx.locate(349), Some((2, 199)));
    }

    #[test]
    fn locate_out_of_range() {
        let idx = three_record_index();
        assert_eq!(idx.locate(350), None);
        assert_eq!(idx.locate(u64::MAX), None);
    }

    #[test]
    fn from_columns_builds_index() {
        let positions = [56_i64, 56 + 1024];
        let lengths = [1024_i32, 2048];
        let entries = [10_i32, 20];
        let idx = FileEventIndex::from_columns(&positions, &lengths, &entries).unwrap();
        assert_eq!(idx.record_count(), 2);
        assert_eq!(idx.records()[1].file_offset, 1080);
        assert_eq!(idx.total_events(), 30);
    }

    #[test]
    fn from_columns_rejects_mismatch() {
        let err = FileEventIndex::from_columns(&[1_i64], &[1_i32, 2], &[1_i32]).unwrap_err();
        assert!(matches!(err, HipoError::CorruptRecord { .. }));
    }
}
