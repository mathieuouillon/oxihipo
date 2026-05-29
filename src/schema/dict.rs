//! `Dict` — the schema directory parsed from a HIPO file's dictionary record.
//!
//! Cheap to clone (a few `Vec`s and a `HashMap`). Wrapped in an `Arc` inside
//! [`File`](crate::read::File) so multi-threaded scans share a single copy.

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
    pub fn add(&mut self, schema: Schema) -> &Schema {
        let name = schema.name().to_string();
        if let Some(&idx) = self.by_name.get(&name) {
            self.by_id.insert(schema.group(), schema.item(), idx);
            self.schemas[idx as usize] = schema;
            return &self.schemas[idx as usize];
        }
        let idx = u16::try_from(self.schemas.len())
            .expect("more than 65535 schemas — bug or hostile input");
        self.by_id.insert(schema.group(), schema.item(), idx);
        self.by_name.insert(name, idx);
        self.schemas.push(schema);
        &self.schemas[idx as usize]
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
    #[inline]
    pub fn get_by_id(&self, group: u16, item: u8) -> Option<&Schema> {
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

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.schemas.iter().map(|s| s.name())
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
            out.add(Schema::parse_text(one)?);
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
                ("pid".into(), DataType::Int),
                ("px".into(), DataType::Float),
            ],
        ));
        f.add(Schema::from_columns(
            "REC::Calorimeter",
            332,
            11,
            [("energy".into(), DataType::Float)],
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
            [("pid".into(), DataType::Int)],
        ));
        assert_eq!(f.len(), 2);
        assert_eq!(f.get("REC::Particle").unwrap().entries().len(), 1);
    }
}
