//! `EventCtx<'a>` — borrowed event bytes paired with the schema dictionary.
//!
//! This is what scans hand to user closures. It exposes the
//! "find a bank by name" ergonomics: the user no longer needs to thread a
//! separate `&Schema` clone through their code.

use std::sync::Arc;

use crate::event::bank::Bank;
use crate::event::composite::Composite;
use crate::event::event::Event;
use crate::event::owned::OwnedEvent;
use crate::schema::{Dict, Schema};

/// Borrowed event view + reference to the file's schema dictionary.
///
/// `Copy + Clone` and cheap (two references), so passing by value is fine.
#[derive(Debug, Copy, Clone)]
pub struct EventCtx<'a> {
    event: Event<'a>,
    dict: &'a Dict,
}

impl<'a> EventCtx<'a> {
    pub fn new(event: Event<'a>, dict: &'a Dict) -> Self {
        Self { event, dict }
    }

    /// The underlying borrowed event.
    pub fn event(&self) -> Event<'a> {
        self.event
    }

    /// The file's schema dictionary.
    pub fn dict(&self) -> &'a Dict {
        self.dict
    }

    pub fn raw(&self) -> &'a [u8] {
        self.event.raw()
    }

    pub fn tag(&self) -> u32 {
        self.event.tag()
    }

    pub fn size(&self) -> u32 {
        self.event.size()
    }

    /// Decode bank `name`. `None` if the schema isn't in the dict, the
    /// structure isn't in the event, or the bank's data is mis-sized.
    pub fn bank(&self, name: &str) -> Option<Bank<'a>> {
        let schema = self.dict.get(name)?;
        self.bank_for(schema)
    }

    /// Decode the bank for an already-resolved schema reference.
    pub fn bank_for(&self, schema: &'a Schema) -> Option<Bank<'a>> {
        let (_, data) = self.event.find(schema.group(), schema.item())?;
        Bank::new(schema, data).ok()
    }

    /// Decode by `(group, item)` directly — useful when the dict doesn't
    /// list the bank but you know the wire IDs.
    pub fn bank_by_id(&self, group: u16, item: u8) -> Option<Bank<'a>> {
        let schema = self.dict.get_by_id(group, item)?;
        let (_, data) = self.event.find(group, item)?;
        Bank::new(schema, data).ok()
    }

    /// True if the event contains a structure for the named schema.
    pub fn has(&self, name: &str) -> bool {
        let Some(schema) = self.dict.get(name) else {
            return false;
        };
        self.event.has(schema.group(), schema.item())
    }

    /// Iterate structure headers + payloads, raw — for tools like `dump`
    /// and `stats` that want to see everything.
    pub fn structures(&self) -> crate::event::event::StructureIter<'a> {
        self.event.iter_structures()
    }

    /// Decode a composite structure by name (looks up the wire IDs via the
    /// dict, then re-parses the inline format string).
    pub fn composite(&self, name: &str) -> Option<Composite<'a>> {
        let schema = self.dict.get(name)?;
        self.composite_by_id(schema.group(), schema.item())
    }

    pub fn composite_by_id(&self, group: u16, item: u8) -> Option<Composite<'a>> {
        for (pos, hdr, data) in self.event.iter_structures_with_offset() {
            if hdr.group == group && hdr.item == item && hdr.header_size > 0 {
                let end = pos + crate::wire::constants::BANK_STRUCTURE_SIZE + data.len();
                let bytes = &self.event.raw()[pos..end];
                return Composite::from_structure(bytes).ok();
            }
        }
        None
    }

    /// Detach into an [`OwnedEvent`] (copies event bytes, shares the dict
    /// via `Arc`). Used to send events across thread boundaries or store
    /// them in collections.
    ///
    /// The dict must already be wrapped in an `Arc` — provided so callers
    /// don't have to clone the entire dict per event.
    pub fn to_owned_with(&self, dict: Arc<Dict>) -> OwnedEvent {
        OwnedEvent::new(self.event.raw().to_vec(), dict)
    }
}
