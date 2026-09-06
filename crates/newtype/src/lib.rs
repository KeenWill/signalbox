//! Positive integer newtypes with checked successors.

/// Defines a positive ordinal backed by `NonZeroU64`, or `NonZeroU32` with `, u32`.
#[macro_export]
macro_rules! positive_ordinal {
    ($(#[$doc:meta])* $name:ident) => {
        $crate::positive_ordinal!(@define $(#[$doc])* $name, NonZeroU64, u64, try_from_u64);
    };
    ($(#[$doc:meta])* $name:ident, u32) => {
        $crate::positive_ordinal!(@define $(#[$doc])* $name, NonZeroU32, u32, try_from_u32);
    };
    (@define $(#[$doc:meta])* $name:ident, $nonzero:ident, $integer:ident, $try_from:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(::std::num::$nonzero);

        impl $name {
            /// The first positive ordinal.
            pub const MIN: Self = Self(::std::num::$nonzero::MIN);

            /// Wraps a nonzero integer.
            pub const fn new(value: ::std::num::$nonzero) -> Self {
                Self(value)
            }

            /// Admits one nonzero integer.
            pub const fn $try_from(value: $integer) -> Option<Self> {
                match ::std::num::$nonzero::new(value) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            /// Returns the positive integer.
            pub const fn get(self) -> $integer {
                self.0.get()
            }

            /// Returns the next ordinal when representable.
            pub const fn checked_next(self) -> Option<Self> {
                match self.get().checked_add(1) {
                    Some(value) => Self::$try_from(value),
                    None => None,
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    positive_ordinal!(Ordinal);
    positive_ordinal!(SmallOrdinal, u32);

    #[test]
    fn u64_ordinals_reject_zero_and_stop_at_the_integer_limit() {
        const FIRST: Ordinal = Ordinal::new(NonZeroU64::MIN);
        const LAST: Ordinal = Ordinal::new(NonZeroU64::MAX);
        assert_eq!(FIRST, Ordinal::MIN);
        assert_eq!(FIRST.get(), 1);
        assert_eq!(Ordinal::try_from_u64(0), None);
        assert_eq!(FIRST.checked_next(), Ordinal::try_from_u64(2));
        assert_eq!(Ordinal::try_from_u64(u64::MAX), Some(LAST));
        assert_eq!(LAST.checked_next(), None);
    }

    #[test]
    fn u32_ordinals_reject_zero_and_stop_at_the_integer_limit() {
        const FIRST: SmallOrdinal = SmallOrdinal::new(NonZeroU32::MIN);
        const LAST: SmallOrdinal = SmallOrdinal::new(NonZeroU32::MAX);
        assert_eq!(FIRST, SmallOrdinal::MIN);
        assert_eq!(FIRST.get(), 1);
        assert_eq!(SmallOrdinal::try_from_u32(0), None);
        assert_eq!(FIRST.checked_next(), SmallOrdinal::try_from_u32(2));
        assert_eq!(SmallOrdinal::try_from_u32(u32::MAX), Some(LAST));
        assert_eq!(LAST.checked_next(), None);
    }
}
