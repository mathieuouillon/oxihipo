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
}
