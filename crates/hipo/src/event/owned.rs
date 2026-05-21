//! `OwnedEvent` — a slice into a ref-counted decompressed record buffer.
//!
//! Each event keeps the underlying record buffer alive via `Arc`. Two
//! consequences:
//!
//! 1. **Zero copies on `next()`**: yielding an event is two `Arc::clone`s
//!    (the payload buffer and the dict), no `memcpy`.
//! 2. **Buffer recycling**: when the iterator advances and no events from
//!    the previous record are still alive, the iterator recovers the
//!    `Vec<u8>` via `Arc::try_unwrap` and reuses it. Steady-state
//!    allocations are zero.
//!
//! Use [`OwnedEvent`] when:
//! - The user wants to send an event across a thread boundary.
//! - The user wants to collect events into a `Vec` and process them later.
//! - The user wants to write events to disk via `Writer::append_owned`.

use std::sync::Arc;

use crate::event::bank::Bank;
use crate::event::composite::Composite;
use crate::event::ctx::EventCtx;
use crate::event::event::{Event, StructureIter};
use crate::schema::{Dict, Schema};

/// An event that owns its byte buffer (via `Arc`) and shares the schema
/// dictionary. Cloning is two atomic increments.
#[derive(Debug, Clone)]
pub struct OwnedEvent {
    payload: Arc<Vec<u8>>,
    start: u32,
    end: u32,
    dict: Arc<Dict>,
}

impl OwnedEvent {
    /// Construct from a stand-alone `Vec<u8>` (e.g. test fixtures, writer
    /// round-trips). Wraps in an `Arc` once; from then on cloning is free.
    pub fn new(bytes: Vec<u8>, dict: Arc<Dict>) -> Self {
        let len = bytes.len() as u32;
        Self {
            payload: Arc::new(bytes),
            start: 0,
            end: len,
            dict,
        }
    }

    /// Construct as a slice into a shared payload buffer. Used by
    /// [`EventIter`](crate::read::EventIter); the same `Arc<Vec<u8>>` is
    /// shared across every event in a single record.
    #[inline]
    pub(crate) fn slice(payload: Arc<Vec<u8>>, start: u32, end: u32, dict: Arc<Dict>) -> Self {
        Self {
            payload,
            start,
            end,
            dict,
        }
    }

    #[inline]
    pub fn bytes(&self) -> &[u8] {
        &self.payload[self.start as usize..self.end as usize]
    }

    pub fn dict(&self) -> &Arc<Dict> {
        &self.dict
    }

    pub fn tag(&self) -> u32 {
        self.as_event().tag()
    }

    pub fn size(&self) -> u32 {
        self.as_event().size()
    }

    /// Borrow as an `EventCtx<'_>` for the duration of `&self`.
    pub fn ctx(&self) -> EventCtx<'_> {
        EventCtx::new(self.as_event(), &self.dict)
    }

    pub fn bank(&self, name: &str) -> Option<Bank<'_>> {
        self.ctx().bank(name)
    }

    /// Decode the bank for an already-resolved schema reference.
    pub fn bank_for<'a>(&'a self, schema: &'a Schema) -> Option<Bank<'a>> {
        self.ctx().bank_for(schema)
    }

    /// Decode by `(group, item)` directly — useful when the dict doesn't
    /// list the bank but you know the wire IDs.
    pub fn bank_by_id(&self, group: u16, item: u8) -> Option<Bank<'_>> {
        self.ctx().bank_by_id(group, item)
    }

    pub fn has(&self, name: &str) -> bool {
        self.ctx().has(name)
    }

    /// Iterate structure headers + payloads, raw — for tools like `dump`
    /// and `stats` that want to see everything.
    pub fn structures(&self) -> StructureIter<'_> {
        self.as_event().iter_structures()
    }

    /// Decode a composite structure by name.
    pub fn composite(&self, name: &str) -> Option<Composite<'_>> {
        self.ctx().composite(name)
    }

    #[inline]
    fn as_event(&self) -> Event<'_> {
        Event::new(self.bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::build::EventBuilder;
    use crate::schema::{DataType, Schema};
    use crate::wire::bytes::write_u32_le;
    use crate::wire::constants::BANK_STRUCTURE_SIZE;

    fn build_event_bytes(schema: &Schema, pid: i32) -> Vec<u8> {
        // Structure: 8-byte header + column-major data (1 row, 1 col i32)
        let mut struct_bytes = vec![0u8; BANK_STRUCTURE_SIZE + 4];
        struct_bytes[0..2].copy_from_slice(&schema.group().to_le_bytes());
        struct_bytes[2] = schema.item();
        struct_bytes[3] = 11;
        write_u32_le(&mut struct_bytes, 4, 4);
        struct_bytes[BANK_STRUCTURE_SIZE..].copy_from_slice(&pid.to_le_bytes());

        let mut eb = EventBuilder::new();
        eb.add_bank_bytes(&struct_bytes);
        eb.finish()
    }

    #[test]
    fn owned_event_round_trip() {
        let mut dict = Dict::new();
        let schema = Schema::from_columns("X", 1, 1, [("pid".into(), DataType::Int)]);
        dict.add(schema.clone());
        let bytes = build_event_bytes(&schema, 42);
        let owned = OwnedEvent::new(bytes, Arc::new(dict));

        let bank = owned.bank("X").unwrap();
        assert_eq!(&*bank.col::<i32>("pid").unwrap(), &[42]);
        assert!(owned.has("X"));
    }

    #[test]
    fn owned_event_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<OwnedEvent>();
    }

    #[test]
    fn cross_thread_round_trip() {
        let mut dict = Dict::new();
        let schema = Schema::from_columns("X", 1, 1, [("pid".into(), DataType::Int)]);
        dict.add(schema.clone());
        let bytes = build_event_bytes(&schema, 99);
        let owned = OwnedEvent::new(bytes, Arc::new(dict));

        let h =
            std::thread::spawn(move || owned.bank("X").map(|b| b.col::<i32>("pid").map(|c| c[0])));
        let result = h.join().unwrap().unwrap().unwrap();
        assert_eq!(result, 99);
    }

    #[test]
    fn slice_constructor_shares_buffer() {
        let mut dict = Dict::new();
        let schema = Schema::from_columns("X", 1, 1, [("pid".into(), DataType::Int)]);
        dict.add(schema.clone());
        let bytes = build_event_bytes(&schema, 7);
        let len = bytes.len() as u32;
        let payload = Arc::new(bytes);
        let dict_arc = Arc::new(dict);

        let a = OwnedEvent::slice(Arc::clone(&payload), 0, len, Arc::clone(&dict_arc));
        let b = OwnedEvent::slice(Arc::clone(&payload), 0, len, Arc::clone(&dict_arc));
        assert_eq!(
            a.bank("X").unwrap().col::<i32>("pid").unwrap(),
            b.bank("X").unwrap().col::<i32>("pid").unwrap()
        );
        // Three Arc holders: a, b, and `payload` itself.
        assert_eq!(Arc::strong_count(&payload), 3);
    }
}
