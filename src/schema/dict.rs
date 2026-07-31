//! `Dict` — the schema directory parsed from a HIPO file's dictionary record.
//!
//! Cheap to clone (a few `Vec`s and a `HashMap`). Held in an `Arc` by
//! [`Chain`](crate::Chain) and by each file it opens, so a multi-threaded scan
//! shares one copy rather than one per worker.

use std::collections::HashMap;

use crate::error::{HipoError, Result};
use crate::schema::types::{Schema, SchemaIndex};

/// Schema directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dict {
    schemas: Vec<Schema>,
    by_name: HashMap<String, u16>,
    by_id: SchemaIndex,
}

impl Dict {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a schema with the same name. Returns a reference
    /// to the stored schema.
    ///
    /// # Panics
    ///
    /// Once the dictionary already holds 65,536 schemas and a *new* name
    /// arrives, because a schema id is a `u16` and there is none left to
    /// assign. Replacing an existing name never panics.
    ///
    /// That ceiling is unreachable for a caller assembling a dictionary it
    /// wrote itself — a real CLAS12 dictionary has a few hundred schemas —
    /// but it is reachable from hostile or corrupt input. Every path in this
    /// crate that parses a dictionary out of bytes uses [`Self::try_add`]
    /// instead, and callers doing the same should too.
    pub fn add(&mut self, schema: Schema) -> &Schema {
        self.try_add(schema)
            .expect("more than 65536 schemas — bug or hostile input")
    }

    /// [`Self::add`], returning an error rather than panicking when the `u16`
    /// schema-id space is exhausted.
    ///
    /// This exists because [`Self::parse_text`] is public, re-exported from
    /// both the crate root and the prelude, and accepts arbitrary text.
    /// Feeding it 65,537 schema blocks used to abort the process outright:
    /// release builds set `panic = "abort"`, so `catch_unwind` does not even
    /// return. Verified — the same probe exits 0 with the panic caught in a
    /// debug build and 134 (SIGABRT) in release.
    ///
    /// Prefer this wherever the schemas come from outside the program. `add`
    /// stays infallible for the common case of building a dictionary from
    /// literals, where a `Result` would be noise at a hundred call sites that
    /// cannot fail.
    pub fn try_add(&mut self, schema: Schema) -> Result<&Schema> {
        let name = schema.name().to_string();
        if let Some(&idx) = self.by_name.get(&name) {
            self.by_id.insert(schema.group(), schema.item(), idx);
            self.schemas[idx as usize] = schema;
            return Ok(&self.schemas[idx as usize]);
        }
        // Ids run `0..=u16::MAX`, so 65,536 schemas fit and the 65,537th does
        // not. The old panic text said "more than 65535", off by one about its
        // own limit.
        let Ok(idx) = u16::try_from(self.schemas.len()) else {
            return Err(HipoError::SchemaParse(format!(
                "dictionary exceeds {} schemas: a schema id is a u16, so there is none \
                 left to assign to {name:?}",
                u16::MAX as usize + 1,
            )));
        };
        self.by_id.insert(schema.group(), schema.item(), idx);
        self.by_name.insert(name, idx);
        self.schemas.push(schema);
        Ok(&self.schemas[idx as usize])
    }

    pub fn get(&self, name: &str) -> Option<&Schema> {
        self.by_name.get(name).map(|&i| &self.schemas[i as usize])
    }

    pub fn require(&self, name: &str) -> Result<&Schema> {
        self.get(name).ok_or_else(|| HipoError::UnknownSchema {
            name: name.to_string(),
        })
    }

    /// Look up by `(group, item)`. O(1) via the [`SchemaIndex`] sparse table.
    /// Crate-internal: backs the typed-row accessors and `EventCtx::bank`.
    #[inline]
    pub(crate) fn get_by_id(&self, group: u16, item: u8) -> Option<&Schema> {
        self.by_id
            .get(group, item)
            .map(|i| &self.schemas[i as usize])
    }

    pub fn len(&self) -> usize {
        self.schemas.len()
    }

    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Schema> {
        self.schemas.iter()
    }

    /// Concatenate every schema's compact text form, matching the C++
    /// writer's dictionary-record payload layout.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for sch in &self.schemas {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&sch.to_text());
        }
        s
    }

    /// Decode a text payload (sequence of `{head}{body}` blocks).
    pub fn parse_text(payload: &str) -> Result<Self> {
        let mut out = Self::new();
        let bytes = payload.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i == bytes.len() {
                break;
            }
            if bytes[i] != b'{' {
                return Err(HipoError::SchemaParse(format!(
                    "expected `{{` at offset {i}, found {:?}",
                    bytes[i] as char
                )));
            }
            let head_start = i + 1;
            let head_end = scan_to_close(bytes, head_start)?;
            let body_start = head_end + 1;
            if body_start >= bytes.len() || bytes[body_start] != b'{' {
                return Err(HipoError::SchemaParse(
                    "schema text missing body block".into(),
                ));
            }
            let body_end = scan_to_close(bytes, body_start + 1)?;
            let one = &payload[i..=body_end];
            // Hostile input can name more schemas than a u16 id space holds.
            out.try_add(Schema::parse_text(one)?)?;
            i = body_end + 1;
        }
        Ok(out)
    }
}

fn scan_to_close(bytes: &[u8], start: usize) -> Result<usize> {
    let mut j = start;
    while j < bytes.len() && bytes[j] != b'}' {
        j += 1;
    }
    if j == bytes.len() {
        return Err(HipoError::SchemaParse("unterminated `{`".into()));
    }
    Ok(j)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::types::DataType;

    fn sample() -> Dict {
        let mut f = Dict::new();
        f.add(Schema::from_columns(
            "REC::Particle",
            300,
            1,
            [
                ("pid".into(), DataType::Int, 1),
                ("px".into(), DataType::Float, 1),
            ],
        ));
        f.add(Schema::from_columns(
            "REC::Calorimeter",
            332,
            11,
            [("energy".into(), DataType::Float, 1)],
        ));
        f
    }

    #[test]
    fn add_and_lookup() {
        let f = sample();
        assert!(f.get("REC::Particle").is_some());
        assert!(f.get("REC::Cherenkov").is_none());
        assert_eq!(f.get_by_id(300, 1).map(|s| s.name()), Some("REC::Particle"));
        assert_eq!(
            f.get_by_id(332, 11).map(|s| s.name()),
            Some("REC::Calorimeter")
        );
    }

    /// `parse_text` is public, in the prelude, and takes arbitrary text.
    /// Overflowing the `u16` id space used to abort the process — release
    /// builds set `panic = "abort"`, so `catch_unwind` never returns.
    ///
    /// The boundary matters and the old message got it wrong: ids run
    /// `0..=u16::MAX`, so **65,536** schemas fit and the 65,537th is the one
    /// with nowhere to go. The old panic said "more than 65535".
    #[test]
    fn parse_text_errors_instead_of_panicking_past_the_id_space() {
        let text = |n: usize| {
            let mut t = String::with_capacity(n * 20);
            for i in 0..n {
                t.push_str(&format!("{{S{i}/300/1}}{{a/I}}\n"));
            }
            t
        };

        // Exactly full is fine — this is the control. If the generator were
        // malformed this would fail here rather than at the boundary, and the
        // test below would be passing for the wrong reason.
        let full = Dict::parse_text(&text(65_536)).expect("65536 schemas must fit");
        assert_eq!(full.len(), 65_536);

        // One more has no id left.
        let err = Dict::parse_text(&text(65_537))
            .expect_err("65537 schemas cannot fit in a u16 id space");
        let msg = err.to_string();
        assert!(
            msg.contains("65536"),
            "message should name the real limit: {msg}"
        );
    }

    #[test]
    fn try_add_reports_the_overflow_rather_than_panicking() {
        let mut d = Dict::new();
        for i in 0..65_536u32 {
            d.try_add(Schema::from_columns(
                format!("S{i}").as_str(),
                300,
                1,
                [("a".into(), DataType::Int, 1)],
            ))
            .expect("within the id space");
        }
        assert_eq!(d.len(), 65_536);

        // Replacing an existing name still works at the boundary — there is
        // no new id to assign, so the limit does not apply.
        assert!(
            d.try_add(Schema::from_columns(
                "S0",
                300,
                1,
                [("b".into(), DataType::Float, 1)],
            ))
            .is_ok(),
            "replacing an existing schema must not hit the id cap"
        );
        assert_eq!(d.len(), 65_536);

        // A genuinely new name does.
        let err = d
            .try_add(Schema::from_columns(
                "one_too_many",
                300,
                1,
                [("a".into(), DataType::Int, 1)],
            ))
            .expect_err("the id space is exhausted");
        assert!(err.to_string().contains("one_too_many"));
    }

    /// The counterpart: `add` keeps its documented `# Panics` contract, so
    /// the two are genuinely different functions and not an alias.
    #[test]
    #[should_panic(expected = "more than 65536 schemas")]
    fn add_still_panics_past_the_id_space() {
        let mut d = Dict::new();
        for i in 0..=65_536u32 {
            d.add(Schema::from_columns(
                format!("S{i}").as_str(),
                300,
                1,
                [("a".into(), DataType::Int, 1)],
            ));
        }
    }

    #[test]
    fn require_errors_with_name() {
        let f = sample();
        let err = f.require("XX").unwrap_err();
        assert!(err.to_string().contains("XX"));
    }

    #[test]
    fn text_roundtrip_dict() {
        let f = sample();
        let text = f.to_text();
        let f2 = Dict::parse_text(&text).unwrap();
        assert_eq!(f2.len(), 2);
        assert_eq!(f2.get("REC::Particle").unwrap().entries().len(), 2);
    }

    #[test]
    fn duplicate_name_replaces() {
        let mut f = sample();
        f.add(Schema::from_columns(
            "REC::Particle",
            300,
            1,
            [("pid".into(), DataType::Int, 1)],
        ));
        assert_eq!(f.len(), 2);
        assert_eq!(f.get("REC::Particle").unwrap().entries().len(), 1);
    }
}
