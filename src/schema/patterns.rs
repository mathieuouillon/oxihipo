//! Glob patterns over bank names.
//!
//! One matcher, shared by [`Chain::skim_banks`](crate::Chain::skim_banks) and
//! anything else that needs to name a *set* of banks. Inventing a second
//! syntax elsewhere is how two tools end up disagreeing about what `REC::*`
//! means.
//!
//! # Syntax
//!
//! Ordinary globbing over the **whole** bank name. `::` is two literal
//! characters, not a separator, so a `*` crosses it freely:
//!
//! | pattern | matches |
//! |---|---|
//! | `REC::Particle` | exactly that bank |
//! | `REC::*` | `REC::Particle`, `REC::Traj`, … but **not** `RECHB::Particle` |
//! | `*::Particle` | `REC::Particle`, `RECHB::Particle`, `RECAI::Particle` |
//! | `*` | every bank in the dictionary |
//!
//! `*::Particle` matching all three reconstruction families is the point of
//! supporting suffixes: CLAS12 dictionaries carry `REC::`, `RECHB::` and
//! `RECAI::` variants of the same banks.
//!
//! # Typos are errors
//!
//! A token with no metacharacter that is absent from the dictionary is
//! [`HipoError::UnknownSchema`] — the same contract as
//! [`Filter::require`](crate::Filter::require), and for the same reason: a
//! misspelled bank name should not silently produce an empty output. A token
//! *with* metacharacters that matches nothing is an error too, since a pattern
//! selecting no banks is nearly always a mistake.

use crate::error::{HipoError, Result};
use crate::schema::{Dict, Schema};

/// A compiled set of bank-name patterns.
///
/// Ordinary globbing over the **whole** bank name — `::` is two literal
/// characters, not a separator, so a `*` crosses it freely:
///
/// | pattern | matches |
/// |---|---|
/// | `REC::Particle` | exactly that bank |
/// | `REC::*` | `REC::Particle`, `REC::Traj`, … but **not** `RECHB::Particle` |
/// | `*::Particle` | `REC::Particle`, `RECHB::Particle`, `RECAI::Particle` |
/// | `*` | every bank in the dictionary |
///
/// `*::Particle` spanning every reconstruction family is the point of
/// supporting suffixes: CLAS12 dictionaries carry `REC::`, `RECHB::` and
/// `RECAI::` variants of the same banks.
///
/// A literal token absent from the dictionary is an error, not an empty
/// result — see [`resolve`](Self::resolve).
#[derive(Debug, Clone)]
pub struct BankPatterns {
    globs: Vec<glob::Pattern>,
    /// The original token for each glob, for error messages.
    sources: Vec<String>,
    /// Whether each token contained a metacharacter. Literal tokens that match
    /// nothing are reported as an unknown *schema*; wildcard tokens that match
    /// nothing are reported as an empty *pattern*.
    literal: Vec<bool>,
}

impl BankPatterns {
    /// Compile a comma-separated pattern list: `"REC::Particle,REC::Calorimeter"`
    /// or `"REC::*"`. Whitespace around tokens is trimmed; empty tokens are
    /// skipped, so a trailing comma is harmless.
    pub fn parse(spec: &str) -> Result<Self> {
        let tokens: Vec<&str> = spec
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();
        Self::from_slice(&tokens)
    }

    /// Compile an already-split pattern list.
    pub fn from_slice(patterns: &[&str]) -> Result<Self> {
        if patterns.is_empty() {
            return Err(HipoError::SchemaParse(
                "no bank patterns given: a projection that keeps nothing would write empty events"
                    .into(),
            ));
        }
        let mut globs = Vec::with_capacity(patterns.len());
        let mut sources = Vec::with_capacity(patterns.len());
        let mut literal = Vec::with_capacity(patterns.len());
        for p in patterns {
            let g = glob::Pattern::new(p)
                .map_err(|e| HipoError::SchemaParse(format!("bad bank pattern {p:?}: {e}")))?;
            globs.push(g);
            sources.push((*p).to_string());
            literal.push(!p.contains(['*', '?', '[']));
        }
        Ok(Self {
            globs,
            sources,
            literal,
        })
    }

    /// Whether `name` matches any pattern in the set.
    pub fn matches(&self, name: &str) -> bool {
        self.globs.iter().any(|g| g.matches(name))
    }

    /// The schemas in `dict` that match, in dictionary order.
    ///
    /// # Errors
    ///
    /// [`HipoError::UnknownSchema`] if a literal token names no bank —
    /// catching the typo rather than silently writing an empty projection —
    /// and [`HipoError::SchemaParse`] if a wildcard token matches nothing.
    pub fn resolve<'d>(&self, dict: &'d Dict) -> Result<Vec<&'d Schema>> {
        for (i, g) in self.globs.iter().enumerate() {
            if dict.iter().any(|s| g.matches(s.name())) {
                continue;
            }
            return Err(if self.literal[i] {
                HipoError::UnknownSchema {
                    name: self.sources[i].clone(),
                }
            } else {
                HipoError::SchemaParse(format!(
                    "bank pattern {:?} matches no bank in the dictionary",
                    self.sources[i]
                ))
            });
        }
        Ok(dict.iter().filter(|s| self.matches(s.name())).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::DataType;

    fn dict() -> Dict {
        let mut d = Dict::new();
        for (name, g, i) in [
            ("REC::Particle", 300u16, 1u8),
            ("REC::Traj", 300, 2),
            ("RECHB::Particle", 310, 1),
            ("RECAI::Particle", 320, 1),
            ("MC::Lund", 40, 3),
        ] {
            d.add(Schema::from_columns(
                name,
                g,
                i,
                [("v".into(), DataType::Int, 1)],
            ));
        }
        d
    }

    fn names(p: &str) -> Vec<String> {
        let d = dict();
        BankPatterns::parse(p)
            .unwrap()
            .resolve(&d)
            .unwrap()
            .iter()
            .map(|s| s.name().to_string())
            .collect()
    }

    #[test]
    fn exact_prefix_and_suffix() {
        assert_eq!(names("REC::Particle"), ["REC::Particle"]);
        // A prefix glob does not leak into a longer family name.
        assert_eq!(names("REC::*"), ["REC::Particle", "REC::Traj"]);
        // A suffix glob spans every reconstruction family — the point of it.
        assert_eq!(
            names("*::Particle"),
            ["REC::Particle", "RECHB::Particle", "RECAI::Particle"]
        );
        assert_eq!(names("*").len(), 5);
    }

    #[test]
    fn comma_list_unions_and_tolerates_a_trailing_comma() {
        // Dictionary order, i.e. insertion order — not the order the
        // patterns were written in, and not sorted.
        assert_eq!(
            names("MC::Lund, REC::Particle,"),
            ["REC::Particle", "MC::Lund"]
        );
    }

    #[test]
    fn a_misspelled_literal_is_an_unknown_schema_not_an_empty_result() {
        let d = dict();
        let err = BankPatterns::parse("REC::Partical")
            .unwrap()
            .resolve(&d)
            .unwrap_err();
        assert!(
            matches!(err, HipoError::UnknownSchema { ref name } if name == "REC::Partical"),
            "{err}"
        );
    }

    #[test]
    fn a_wildcard_matching_nothing_is_an_error_too() {
        let d = dict();
        let err = BankPatterns::parse("XYZ::*")
            .unwrap()
            .resolve(&d)
            .unwrap_err();
        assert!(err.to_string().contains("matches no bank"), "{err}");
    }

    #[test]
    fn an_empty_pattern_list_is_rejected() {
        assert!(BankPatterns::parse("").is_err());
        assert!(BankPatterns::from_slice(&[]).is_err());
    }
}
