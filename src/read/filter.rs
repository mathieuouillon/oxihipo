//! `Filter` — event-level bank-name pushdown.
//!
//! ```
//! # use oxihipo::read::Filter;
//! let filter = Filter::require(["REC::Particle", "REC::Event"])
//!     .record_tag([0x42_u64]);
//! # let _ = filter;
//! ```
//!
//! `require` drops events that don't carry every named bank.
//! `record_tag` skips entire records whose `user_word_1` doesn't match.
//! `event_tag` / `event_tag_any` drop individual events whose per-event
//! `EH_TAG` doesn't match — read from the header (or directory) without
//! inflating any bank.
//!
//! Names not present in the file's dictionary are silently dropped at
//! bind time — they can't appear in events anyway. Callers who want
//! early validation can call [`Filter::validate`] explicitly.

use crate::error::{HipoError, Result};
use crate::event::Event;
use crate::schema::Dict;

#[derive(Debug, Clone, Default)]
pub struct Filter {
    require_names: Vec<String>,
    require_ids: Vec<(u16, u8)>,
    record_tags: Vec<u64>,
    event_tags: Vec<u32>,
    event_tag_mask: u32,
}

impl Filter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a filter that requires every named bank to be present.
    pub fn require<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut f = Self::new();
        for n in names {
            f.require_names.push(n.into());
        }
        f
    }

    pub fn and_require<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for n in names {
            self.require_names.push(n.into());
        }
        self
    }

    /// Add record-tag pushdown. Records whose `user_word_1` doesn't match
    /// any of `tags` are skipped without decompression.
    pub fn record_tag<I>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = u64>,
    {
        self.record_tags.extend(tags);
        self
    }

    /// Keep only events whose per-event tag (`EH_TAG` in the event header)
    /// is one of `tags`. Cheap: the tag is read from the event header (or,
    /// for the by-bank / per-column formats, the record directory) without
    /// inflating any bank stream. ANDs with the other clauses.
    pub fn event_tag<I>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = u32>,
    {
        self.event_tags.extend(tags);
        self
    }

    /// Keep only events with at least one of `mask`'s bits set in their tag
    /// (`tag & mask != 0`) — the bit-flag form, for tags used as a set of
    /// physics categories. Repeated calls OR into the mask. ANDs with the
    /// other clauses.
    pub fn event_tag_any(mut self, mask: u32) -> Self {
        self.event_tag_mask |= mask;
        self
    }

    pub fn is_active(&self) -> bool {
        !self.require_names.is_empty()
            || !self.record_tags.is_empty()
            || !self.require_ids.is_empty()
            || self.event_tag_active()
    }

    pub fn required_names(&self) -> &[String] {
        &self.require_names
    }

    pub fn record_tags(&self) -> &[u64] {
        &self.record_tags
    }

    pub fn event_tags(&self) -> &[u32] {
        &self.event_tags
    }

    pub fn event_tag_mask(&self) -> u32 {
        self.event_tag_mask
    }

    /// Whether any event-tag clause is set.
    #[inline]
    fn event_tag_active(&self) -> bool {
        !self.event_tags.is_empty() || self.event_tag_mask != 0
    }

    /// Whether `tag` satisfies the (possibly ANDed) event-tag clauses.
    /// Vacuously true when no event-tag clause is set.
    #[inline]
    fn tag_matches(&self, tag: u32) -> bool {
        if !self.event_tags.is_empty() && !self.event_tags.contains(&tag) {
            return false;
        }
        if self.event_tag_mask != 0 && (tag & self.event_tag_mask) == 0 {
            return false;
        }
        true
    }

    /// Verify every required-bank name appears in `dict`. Returns the
    /// first unknown name as an error. Use this when you want to fail
    /// fast on filter typos before iterating.
    pub fn validate(&self, dict: &Dict) -> Result<()> {
        for name in &self.require_names {
            if dict.get(name).is_none() {
                return Err(HipoError::UnknownSchema { name: name.clone() });
            }
        }
        Ok(())
    }

    /// Resolve bank names against `dict`. Names not in the dict are
    /// silently dropped — they can't appear in events anyway.
    pub(crate) fn bind(&mut self, dict: &Dict) {
        self.require_ids.clear();
        for name in &self.require_names {
            if let Some(s) = dict.get(name) {
                self.require_ids.push((s.group(), s.item()));
            }
        }
    }

    /// True if the event carries every required bank and its tag matches.
    #[inline]
    pub(crate) fn check(&self, event: &Event<'_>) -> bool {
        if self.event_tag_active() && !self.tag_matches(event.tag()) {
            return false;
        }
        for &(g, i) in &self.require_ids {
            if !event.has(g, i) {
                return false;
            }
        }
        true
    }

    /// `check` for by-bank records — peeks the presence matrix
    /// directly without inflating any bank stream.
    #[inline]
    pub(crate) fn check_by_bank(
        &self,
        record: &crate::wire::by_bank::ByBankRecord,
        event_idx: u32,
    ) -> bool {
        if self.event_tag_active() && !self.tag_matches(record.event_tag(event_idx)) {
            return false;
        }
        for &(g, i) in &self.require_ids {
            let Some(b) = record.bank_index(g, i) else {
                return false;
            };
            if !record.has(event_idx, b) {
                return false;
            }
        }
        true
    }

    /// Presence-only filter check against a per-column record — the
    /// analogue of [`Self::check_by_bank`].
    #[inline]
    pub(crate) fn check_per_column(
        &self,
        record: &crate::wire::per_column::PerColumnRecord,
        event_idx: u32,
    ) -> bool {
        if self.event_tag_active() && !self.tag_matches(record.event_tag(event_idx)) {
            return false;
        }
        for &(g, i) in &self.require_ids {
            let Some(b) = record.bank_index(g, i) else {
                return false;
            };
            if !record.has(event_idx, b) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DataType, Schema};

    fn dict() -> Dict {
        let mut d = Dict::new();
        d.add(Schema::from_columns(
            "REC::Particle",
            300,
            1,
            [("pid".into(), DataType::Int, 1)],
        ));
        d.add(Schema::from_columns(
            "REC::Calorimeter",
            332,
            11,
            [("e".into(), DataType::Float, 1)],
        ));
        d
    }

    #[test]
    fn binds_known_names() {
        let d = dict();
        let mut f = Filter::require(["REC::Particle", "UNKNOWN"]);
        f.bind(&d);
        assert_eq!(f.require_ids, vec![(300_u16, 1_u8)]);
    }

    #[test]
    fn validate_rejects_unknown() {
        let d = dict();
        let f = Filter::require(["UNKNOWN"]);
        let err = f.validate(&d).unwrap_err();
        assert!(matches!(err, HipoError::UnknownSchema { .. }));
    }

    #[test]
    fn validate_passes_for_known() {
        let d = dict();
        let f = Filter::require(["REC::Particle", "REC::Calorimeter"]);
        f.validate(&d).unwrap();
    }

    #[test]
    fn record_tag_chained() {
        let f = Filter::require(["REC::Particle"]).record_tag([1_u64, 2, 3]);
        assert_eq!(f.record_tags(), &[1, 2, 3]);
    }

    #[test]
    fn event_tag_set_membership() {
        let f = Filter::new().event_tag([2_u32, 5]);
        assert!(f.is_active());
        assert_eq!(f.event_tags(), &[2, 5]);
        assert!(f.tag_matches(2) && f.tag_matches(5));
        assert!(!f.tag_matches(3));
    }

    #[test]
    fn event_tag_any_is_a_bitmask() {
        let f = Filter::new().event_tag_any(0b0101);
        assert_eq!(f.event_tag_mask(), 0b0101);
        assert!(f.tag_matches(0b0001)); // bit 0 set
        assert!(f.tag_matches(0b0100)); // bit 2 set
        assert!(f.tag_matches(0b0111)); // both + extra
        assert!(!f.tag_matches(0b0010)); // no overlap
    }

    #[test]
    fn no_event_tag_clause_matches_any_tag() {
        // A filter with only a bank requirement never constrains the tag.
        let f = Filter::require(["REC::Particle"]);
        assert!(f.tag_matches(0) && f.tag_matches(42));
    }
}
