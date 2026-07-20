//! `TagSet` — the ergonomic view of the per-event tag (`EH_TAG`, a `u32`) as a
//! set of up to 32 bit-flags, plus the [`tag_flags!`](crate::tag_flags) macro
//! for declaring *named* flags.
//!
//! Tags are just bits of the per-event `u32`, so this is entirely
//! wire-compatible: a `TagSet` converts to/from `u32` for free, and named flags
//! are compile-time constants — no format change.
//!
//! ```
//! use oxihipo::{tag_flags, TagSet};
//!
//! tag_flags! {
//!     /// CLAS12 physics event categories.
//!     pub EventTag {
//!         Dvcs        = 0,
//!         Sidis       = 1,
//!         OneElectron = 2,
//!     }
//! }
//!
//! let t: TagSet = EventTag::Dvcs | EventTag::OneElectron;
//! assert!(t.contains(EventTag::Dvcs));
//! assert!(!t.contains(EventTag::Sidis));
//! assert_eq!(t.bits(), 0b101);
//! assert_eq!(EventTag::name(1), Some("Sidis"));
//! ```

use core::fmt;
use core::ops::{BitAnd, BitOr, BitOrAssign};

/// A set of up to 32 event-tag bits — the ergonomic form of the per-event
/// `EH_TAG` (`u32`). Combine flags with `|`, test with
/// [`contains`](Self::contains) (superset) / [`intersects`](Self::intersects)
/// (any shared bit). Converts to/from `u32` for free.
#[derive(Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct TagSet(u32);

impl TagSet {
    /// The empty set (tag `0`).
    pub const EMPTY: TagSet = TagSet(0);

    /// A set from a raw bit pattern.
    #[inline]
    pub const fn from_bits(bits: u32) -> Self {
        TagSet(bits)
    }

    /// A single-bit set at position `bit` (`0..=31`) — i.e. `1 << bit`.
    #[inline]
    pub const fn from_index(bit: u32) -> Self {
        TagSet(1u32 << bit)
    }

    /// The raw bit pattern (what actually lands in the event's `EH_TAG`).
    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether no bit is set.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every bit of `other` is also set here (`self ⊇ other`).
    #[inline]
    pub const fn contains(self, other: TagSet) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether `self` and `other` share any bit.
    #[inline]
    pub const fn intersects(self, other: TagSet) -> bool {
        self.0 & other.0 != 0
    }

    /// This set with every bit of `other` added.
    #[inline]
    pub const fn with(self, other: TagSet) -> Self {
        TagSet(self.0 | other.0)
    }

    /// Add every bit of `other` in place.
    #[inline]
    pub fn insert(&mut self, other: TagSet) {
        self.0 |= other.0;
    }

    /// The set bit positions, ascending (`0..32`) — pair with a `tag_flags!`
    /// type's `name` to render a tag: `t.iter_bits().filter_map(EventTag::name)`.
    pub fn iter_bits(self) -> impl Iterator<Item = u32> {
        (0..32).filter(move |b| self.0 & (1u32 << b) != 0)
    }
}

impl fmt::Debug for TagSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TagSet(0x{:08x})", self.0)
    }
}

impl BitOr for TagSet {
    type Output = TagSet;
    #[inline]
    fn bitor(self, rhs: TagSet) -> TagSet {
        TagSet(self.0 | rhs.0)
    }
}
impl BitOrAssign for TagSet {
    #[inline]
    fn bitor_assign(&mut self, rhs: TagSet) {
        self.0 |= rhs.0;
    }
}
impl BitAnd for TagSet {
    type Output = TagSet;
    #[inline]
    fn bitand(self, rhs: TagSet) -> TagSet {
        TagSet(self.0 & rhs.0)
    }
}
impl From<u32> for TagSet {
    #[inline]
    fn from(bits: u32) -> Self {
        TagSet(bits)
    }
}
impl From<TagSet> for u32 {
    #[inline]
    fn from(t: TagSet) -> u32 {
        t.0
    }
}

/// A persisted name↔bit registry — the `(name, bit-position)` table that a
/// [`tag_flags!`](crate::tag_flags) block declares at compile time, serialised
/// into a file so a reader can resolve tag names **without** the original
/// declaration.
///
/// Bit positions are `0..=31` (as in `tag_flags!`'s `NAMES`), so a name maps to
/// the single-bit [`TagSet`] `1 << position`. A writer records one with
/// [`WriterBuilder::tag_names`]; a reader reads it back via
/// [`Chain::tag_registry`]. It rides in the file's dictionary record (a small
/// extra text bank), so it is additive and wire-compatible — a file carrying a
/// registry stays readable by tools that don't know about it. The Python
/// binding uses it for `filtered(event_tag="dvcs")`.
///
/// [`WriterBuilder::tag_names`]: crate::write::WriterBuilder::tag_names
/// [`Chain::tag_registry`]: crate::read::Chain::tag_registry
///
/// ```
/// use oxihipo::{tag_flags, TagRegistry};
///
/// tag_flags! { pub EventTag { Dvcs = 0, Sidis = 1 } }
///
/// let reg = TagRegistry::from_names(EventTag::NAMES.iter().copied());
/// assert_eq!(reg.position("Dvcs"), Some(0));
/// assert_eq!(reg.name(1), Some("Sidis"));
/// assert_eq!(reg.mask("Sidis"), Some(0b10)); // 1 << 1
/// assert_eq!(reg.mask_of(["Dvcs", "Sidis"]), Some(0b11));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagRegistry {
    /// `(name, bit position)`, in declaration / insertion order.
    entries: Vec<(String, u32)>,
}

impl TagRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a `(name, bit)` list — pass a `tag_flags!` type's `NAMES`
    /// (via `.iter().copied()`) or any iterator of pairs. Later duplicate
    /// names replace earlier ones.
    pub fn from_names<'a, I>(names: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, u32)>,
    {
        let mut reg = Self::new();
        for (name, bit) in names {
            reg.insert(name, bit);
        }
        reg
    }

    /// Record (or replace, by name) one `name → bit position` mapping.
    pub fn insert(&mut self, name: impl Into<String>, bit: u32) {
        let name = name.into();
        match self.entries.iter_mut().find(|(n, _)| *n == name) {
            Some(e) => e.1 = bit,
            None => self.entries.push((name, bit)),
        }
    }

    /// Whether the registry has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of named flags.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The bit position (`0..=31`) declared for `name`, if any.
    pub fn position(&self, name: &str) -> Option<u32> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| *b)
    }

    /// The name declared at bit position `bit`, if any.
    pub fn name(&self, bit: u32) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, b)| *b == bit)
            .map(|(n, _)| n.as_str())
    }

    /// The single-bit mask `1 << position` for `name` — what an
    /// [`event_tag_any`](crate::read::Filter::event_tag_any) clause wants.
    /// `None` if the name is unknown or its position is out of range.
    pub fn mask(&self, name: &str) -> Option<u32> {
        self.position(name).and_then(|p| 1u32.checked_shl(p))
    }

    /// The OR of every named flag's [`mask`](Self::mask). `None` (and no
    /// partial mask) if **any** name is unknown — so a typo fails loudly
    /// rather than silently narrowing the selection.
    pub fn mask_of<'a, I>(&self, names: I) -> Option<u32>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut mask = 0u32;
        for name in names {
            mask |= self.mask(name)?;
        }
        Some(mask)
    }

    /// The `(name, bit)` pairs, in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u32)> {
        self.entries.iter().map(|(n, b)| (n.as_str(), *b))
    }

    /// Serialise to the on-disk text form: one `name=bit` per line.
    pub(crate) fn to_text(&self) -> String {
        let mut s = String::new();
        for (name, bit) in &self.entries {
            s.push_str(name);
            s.push('=');
            s.push_str(&bit.to_string());
            s.push('\n');
        }
        s
    }

    /// Parse the on-disk text form — tolerant: blank and malformed lines are
    /// skipped, so a future writer can add fields without breaking old readers.
    pub(crate) fn parse_text(text: &str) -> Self {
        let mut reg = Self::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((name, bit)) = line.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if let Ok(bit) = bit.trim().parse::<u32>() {
                reg.insert(name, bit);
            }
        }
        reg
    }
}

/// Declare a group of named single-bit tag flags.
///
/// Each `Name = bit` (where `bit` is a **literal** position `0..=31`) becomes an
/// associated [`TagSet`] constant `1 << bit`; the generated type also carries
/// `name(bit)` and `NAMES` for logging and display. Combine flags with `|` and
/// pass them anywhere a tag is accepted — [`EventBuilder::with_tag`], the
/// writer's `with_tag`, and [`Filter::event_tag_any`] all take `impl Into<u32>`.
///
/// [`EventBuilder::with_tag`]: crate::event::EventBuilder::with_tag
/// [`Filter::event_tag_any`]: crate::read::Filter::event_tag_any
///
/// ```
/// # use oxihipo::{tag_flags, TagSet};
/// tag_flags! {
///     pub Trigger { Fcup = 0, Random = 1 }
/// }
/// assert_eq!((Trigger::Fcup | Trigger::Random).bits(), 0b11);
/// assert_eq!(Trigger::NAMES, &[("Fcup", 0), ("Random", 1)]);
/// ```
#[macro_export]
macro_rules! tag_flags {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident {
            $( $(#[$fmeta:meta])* $flag:ident = $bit:literal ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug)]
        $vis struct $name;

        #[allow(non_upper_case_globals)]
        impl $name {
            $( $(#[$fmeta])* $vis const $flag: $crate::TagSet = $crate::TagSet::from_index($bit); )*

            /// The declared flag name at bit position `bit`, if any.
            $vis const fn name(bit: u32) -> ::core::option::Option<&'static str> {
                match bit {
                    $( $bit => ::core::option::Option::Some(::core::stringify!($flag)), )*
                    _ => ::core::option::Option::None,
                }
            }

            /// Every declared flag as a `(name, bit)` pair, in declaration order.
            $vis const NAMES: &'static [(&'static str, u32)] =
                &[ $( (::core::stringify!($flag), $bit) ),* ];
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    tag_flags! {
        /// Test flags.
        pub Cat {
            Dvcs = 0,
            Sidis = 1,
            Elastic = 2,
        }
    }

    #[test]
    fn flags_are_single_bits() {
        assert_eq!(Cat::Dvcs.bits(), 1);
        assert_eq!(Cat::Sidis.bits(), 2);
        assert_eq!(Cat::Elastic.bits(), 4);
    }

    #[test]
    fn combine_and_test() {
        let t = Cat::Dvcs | Cat::Elastic;
        assert_eq!(t.bits(), 0b101);
        assert!(t.contains(Cat::Dvcs) && t.contains(Cat::Elastic));
        assert!(!t.contains(Cat::Sidis));
        assert!(t.intersects(Cat::Dvcs | Cat::Sidis)); // shares Dvcs
        assert!(!t.intersects(Cat::Sidis));
    }

    #[test]
    fn conversions_and_iter() {
        let t = Cat::Sidis | Cat::Elastic;
        assert_eq!(u32::from(t), 0b110);
        assert_eq!(TagSet::from(0b110_u32), t);
        assert_eq!(t.iter_bits().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(
            t.iter_bits().filter_map(Cat::name).collect::<Vec<_>>(),
            vec!["Sidis", "Elastic"]
        );
    }

    #[test]
    fn name_registry() {
        assert_eq!(Cat::name(0), Some("Dvcs"));
        assert_eq!(Cat::name(2), Some("Elastic"));
        assert_eq!(Cat::name(9), None);
        assert_eq!(Cat::NAMES, &[("Dvcs", 0), ("Sidis", 1), ("Elastic", 2)]);
    }

    #[test]
    fn empty_and_insert() {
        let mut t = TagSet::EMPTY;
        assert!(t.is_empty());
        t.insert(Cat::Dvcs);
        t |= Cat::Sidis;
        assert_eq!(t, Cat::Dvcs | Cat::Sidis);
    }

    #[test]
    fn registry_from_names_and_lookups() {
        let reg = TagRegistry::from_names(Cat::NAMES.iter().copied());
        assert_eq!(reg.len(), 3);
        assert!(!reg.is_empty());
        assert_eq!(reg.position("Sidis"), Some(1));
        assert_eq!(reg.position("Nope"), None);
        assert_eq!(reg.name(2), Some("Elastic"));
        assert_eq!(reg.name(9), None);
        // A name resolves to the single-bit mask `1 << position` — the form an
        // `event_tag_any` clause consumes.
        assert_eq!(reg.mask("Dvcs"), Some(0b001));
        assert_eq!(reg.mask("Elastic"), Some(0b100));
        assert_eq!(reg.mask("Nope"), None);
        assert_eq!(reg.mask_of(["Dvcs", "Elastic"]), Some(0b101));
        // Any unknown name → the whole `mask_of` fails (no partial mask).
        assert_eq!(reg.mask_of(["Dvcs", "Nope"]), None);
    }

    #[test]
    fn registry_insert_replaces_by_name() {
        let mut reg = TagRegistry::new();
        reg.insert("dvcs", 0);
        reg.insert("dvcs", 5); // same name → replace, no duplicate
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.position("dvcs"), Some(5));
    }

    #[test]
    fn registry_text_round_trip() {
        let reg = TagRegistry::from_names([("Dvcs", 0), ("Sidis", 1), ("Elastic", 2)]);
        let text = reg.to_text();
        assert_eq!(text, "Dvcs=0\nSidis=1\nElastic=2\n");
        assert_eq!(TagRegistry::parse_text(&text), reg);
    }

    #[test]
    fn registry_parse_is_tolerant() {
        // Blank lines, stray whitespace, and a malformed line are all skipped.
        let reg = TagRegistry::parse_text("  Dvcs = 0 \n\ngarbage-no-eq\nSidis=1\n=3\nBad=xyz\n");
        assert_eq!(
            reg.iter().collect::<Vec<_>>(),
            vec![("Dvcs", 0), ("Sidis", 1)]
        );
    }
}
