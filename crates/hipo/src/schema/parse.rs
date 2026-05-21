//! Parsing for the two on-disk schema formats.
//!
//! - **Compact text** `{Name/group/item}{col1/T,col2/T,...}` — what
//!   dictionary records carry.
//! - **JSON** — what the `(120, 2)` dictionary banks carry in newer files.

use serde::Deserialize;

use crate::error::{HipoError, Result};
use crate::schema::types::{DataType, Schema};

#[derive(Deserialize)]
struct JsonSchema {
    name: String,
    group: u16,
    item: u8,
    entries: Vec<JsonEntry>,
}

#[derive(Deserialize)]
struct JsonEntry {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

impl Schema {
    /// Parse a compact text form.
    pub fn parse_text(text: &str) -> Result<Self> {
        let (head, body) = split_text(text)?;
        let (name, group, item) = parse_head(head)?;
        let columns = parse_columns(body)?;
        Ok(Self::from_columns(name, group, item, columns))
    }

    /// Parse the JSON form.
    pub fn parse_json(json: &str) -> Result<Self> {
        let parsed: JsonSchema = serde_json::from_str(json)
            .map_err(|e| HipoError::SchemaParse(format!("invalid schema JSON: {e}")))?;
        let mut cols = Vec::with_capacity(parsed.entries.len());
        for e in parsed.entries {
            let ty = parse_type(&e.ty)?;
            cols.push((e.name, ty));
        }
        Ok(Self::from_columns(
            parsed.name,
            parsed.group,
            parsed.item,
            cols,
        ))
    }

    /// Round-trip the schema to its compact text form.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        s.push('{');
        s.push_str(self.name());
        s.push('/');
        s.push_str(&self.group().to_string());
        s.push('/');
        s.push_str(&self.item().to_string());
        s.push_str("}{");
        for (i, e) in self.entries().iter().enumerate() {
            if i != 0 {
                s.push(',');
            }
            s.push_str(&e.name);
            s.push('/');
            s.push(e.ty.letter());
        }
        s.push('}');
        s
    }
}

fn split_text(text: &str) -> Result<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::with_capacity(2);
    let mut i = 0;
    while i < bytes.len() && spans.len() < 2 {
        if bytes[i] == b'{' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'}' {
                j += 1;
            }
            if j == bytes.len() {
                return Err(HipoError::SchemaParse(
                    "unterminated `{` in schema text".into(),
                ));
            }
            spans.push(&text[start..j]);
            i = j + 1;
        } else {
            i += 1;
        }
    }
    if spans.len() < 2 {
        return Err(HipoError::SchemaParse(
            "schema text must have two {…} blocks".into(),
        ));
    }
    Ok((spans[0], spans[1]))
}

fn parse_head(head: &str) -> Result<(String, u16, u8)> {
    let mut parts = head.splitn(3, '/');
    let name = parts
        .next()
        .ok_or_else(|| HipoError::SchemaParse("missing schema name".into()))?
        .trim()
        .to_string();
    let group: u16 = parts
        .next()
        .ok_or_else(|| HipoError::SchemaParse("missing schema group".into()))?
        .trim()
        .parse()
        .map_err(|e| HipoError::SchemaParse(format!("bad schema group: {e}")))?;
    let item: u8 = parts
        .next()
        .ok_or_else(|| HipoError::SchemaParse("missing schema item".into()))?
        .trim()
        .parse()
        .map_err(|e| HipoError::SchemaParse(format!("bad schema item: {e}")))?;
    Ok((name, group, item))
}

fn parse_columns(body: &str) -> Result<Vec<(String, DataType)>> {
    body.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|raw| {
            let mut parts = raw.splitn(2, '/');
            let n = parts
                .next()
                .ok_or_else(|| HipoError::SchemaParse("empty column".into()))?
                .trim()
                .to_string();
            let ty = parts
                .next()
                .ok_or_else(|| HipoError::SchemaParse(format!("column {n:?} missing type")))?
                .trim();
            Ok((n, parse_type(ty)?))
        })
        .collect()
}

fn parse_type(ty: &str) -> Result<DataType> {
    let mut chars = ty.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => DataType::from_letter(c)
            .ok_or_else(|| HipoError::SchemaParse(format!("unknown type {ty:?}"))),
        _ => Err(HipoError::SchemaParse(format!(
            "type must be a single letter, got {ty:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_compact_text() {
        let s = Schema::parse_text("{REC::Particle/300/1}{pid/I,px/F,py/F,charge/B}").unwrap();
        assert_eq!(s.name(), "REC::Particle");
        assert_eq!(s.group(), 300);
        assert_eq!(s.item(), 1);
        assert_eq!(s.entries().len(), 4);
        assert_eq!(s.entries()[0].ty, DataType::Int);
        assert_eq!(s.entries()[3].ty, DataType::Byte);
    }

    #[test]
    fn parse_text_with_whitespace() {
        let s = Schema::parse_text("{ REC::Foo / 100 / 2 }{ a / I , b / F }").unwrap();
        assert_eq!(s.name(), "REC::Foo");
        assert_eq!(s.entries()[1].name, "b");
    }

    #[test]
    fn text_round_trip() {
        let original = "{REC::Particle/300/1}{pid/I,px/F,py/F,charge/B}";
        let s = Schema::parse_text(original).unwrap();
        assert_eq!(s.to_text(), original);
    }

    #[test]
    fn parse_json_form() {
        let json = r#"{
            "name": "REC::Calorimeter",
            "group": 332,
            "item": 11,
            "entries": [
                {"name": "energy", "type": "F"},
                {"name": "sector", "type": "B"}
            ]
        }"#;
        let s = Schema::parse_json(json).unwrap();
        assert_eq!(s.name(), "REC::Calorimeter");
        assert_eq!(s.entries()[0].ty, DataType::Float);
        assert_eq!(s.entries()[1].ty, DataType::Byte);
    }

    #[test]
    fn rejects_bad_type_letter() {
        let err = Schema::parse_text("{X/1/1}{a/Z}").unwrap_err();
        assert!(err.to_string().contains("unknown type"));
    }

    #[test]
    fn rejects_missing_brace() {
        let err = Schema::parse_text("{X/1/1}").unwrap_err();
        assert!(err.to_string().contains("two"));
    }
}
