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
    ///
    /// Column-type tokens are either a single letter (`pid/I`) or a
    /// letter followed by `#N` for a fixed-length array (`px/F#32`,
    /// `pid/S#2`). `N` must be a positive integer.
    pub fn parse_text(text: &str) -> Result<Self> {
        let (head, body) = split_text(text)?;
        let (name, group, item) = parse_head(head)?;
        let columns = parse_columns(body)?;
        Ok(Self::from_columns_ext(name, group, item, columns))
    }

    /// Parse the JSON form.
    pub fn parse_json(json: &str) -> Result<Self> {
        let parsed: JsonSchema = serde_json::from_str(json)
            .map_err(|e| HipoError::SchemaParse(format!("invalid schema JSON: {e}")))?;
        let mut cols = Vec::with_capacity(parsed.entries.len());
        for e in parsed.entries {
            let (ty, length) = parse_type(&e.ty)?;
            cols.push((e.name, ty, length));
        }
        Ok(Self::from_columns_ext(
            parsed.name,
            parsed.group,
            parsed.item,
            cols,
        ))
    }

    /// Round-trip the schema to its compact text form. Array columns
    /// are emitted as `name/T#N`; scalar columns (length == 1) are
    /// emitted as `name/T` to preserve byte-identical round-trips with
    /// historical files and the C++ writer.
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
            if e.length > 1 {
                s.push('#');
                s.push_str(&e.length.to_string());
            }
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

fn parse_columns(body: &str) -> Result<Vec<(String, DataType, u32)>> {
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
            let (dt, length) = parse_type(ty)?;
            Ok((n, dt, length))
        })
        .collect()
}

/// Parse a column-type token like `I`, `F`, or `F#32`. Returns the
/// underlying `DataType` and the per-row element count (`1` for
/// scalars; `N` for `T#N` arrays).
fn parse_type(ty: &str) -> Result<(DataType, u32)> {
    let (letter_part, length_part) = match ty.split_once('#') {
        Some((a, b)) => (a.trim(), Some(b.trim())),
        None => (ty.trim(), None),
    };
    let mut chars = letter_part.chars();
    let dt = match (chars.next(), chars.next()) {
        (Some(c), None) => DataType::from_letter(c).ok_or_else(|| {
            HipoError::SchemaParse(format!("unknown type letter {letter_part:?}"))
        })?,
        _ => {
            return Err(HipoError::SchemaParse(format!(
                "type letter must be a single character, got {letter_part:?}"
            )));
        }
    };
    let length = match length_part {
        None => 1u32,
        Some(s) => {
            let n: u32 = s
                .parse()
                .map_err(|e| HipoError::SchemaParse(format!("bad array length {s:?}: {e}")))?;
            if n == 0 {
                return Err(HipoError::SchemaParse(format!(
                    "array length must be > 0 (got {s:?})"
                )));
            }
            n
        }
    };
    Ok((dt, length))
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

    #[test]
    fn parse_array_columns() {
        let s = Schema::parse_text("{REC::Foo/300/1}{pid/S#2,px/F#32,e/F}").unwrap();
        assert_eq!(s.entries().len(), 3);
        assert_eq!(s.entries()[0].ty, DataType::Short);
        assert_eq!(s.entries()[0].length, 2);
        assert_eq!(s.entries()[1].ty, DataType::Float);
        assert_eq!(s.entries()[1].length, 32);
        assert_eq!(s.entries()[2].ty, DataType::Float);
        assert_eq!(s.entries()[2].length, 1);
        // Row size = 2*2 + 32*4 + 1*4 = 4 + 128 + 4 = 136 bytes
        assert_eq!(s.row_size(), 136);
    }

    #[test]
    fn array_columns_round_trip() {
        let original = "{REC::Foo/300/1}{pid/S#2,px/F#32,e/F}";
        let s = Schema::parse_text(original).unwrap();
        assert_eq!(s.to_text(), original);
    }

    #[test]
    fn rejects_zero_length_array() {
        let err = Schema::parse_text("{X/1/1}{a/F#0}").unwrap_err();
        assert!(err.to_string().contains("length"));
    }

    #[test]
    fn rejects_negative_length_array() {
        let err = Schema::parse_text("{X/1/1}{a/F#-3}").unwrap_err();
        assert!(err.to_string().contains("array length"));
    }

    #[test]
    fn array_columns_with_whitespace() {
        let s = Schema::parse_text("{X/1/1}{ a / F # 16 , b / I }").unwrap();
        assert_eq!(s.entries()[0].length, 16);
        assert_eq!(s.entries()[1].length, 1);
    }
}
