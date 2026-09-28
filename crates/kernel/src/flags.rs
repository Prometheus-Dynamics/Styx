//! A small bit-flag newtype generator (no external crates).

/// Declares a `u32`/`u64` flag newtype with named constants, set operations and a `Debug`
/// that lists the set flag names (and any unknown bits).
macro_rules! flags {
    (
        $(#[$meta:meta])*
        pub struct $name:ident: $ty:ty {
            $( $(#[$fmeta:meta])* const $flag:ident = $value:expr; )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
        pub struct $name(pub $ty);

        #[allow(dead_code)]
        impl $name {
            $( $(#[$fmeta])* pub const $flag: Self = Self($value); )*

            /// No flags.
            pub const fn empty() -> Self {
                Self(0)
            }

            /// The raw bits.
            pub const fn bits(self) -> $ty {
                self.0
            }

            /// True when every bit of `other` is set.
            pub const fn contains(self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }

            /// True when any bit of `other` is set.
            pub const fn intersects(self, other: Self) -> bool {
                self.0 & other.0 != 0
            }

            /// True when no bits are set.
            pub const fn is_empty(self) -> bool {
                self.0 == 0
            }

            /// The names of the set flags this crate knows.
            pub fn names(self) -> Vec<&'static str> {
                let mut out = Vec::new();
                $(
                    let v: $ty = Self::$flag.0;
                    if v != 0 && self.0 & v == v {
                        out.push(stringify!($flag));
                    }
                )*
                out
            }

            fn known_bits() -> $ty {
                let bits: $ty = 0;
                bits $( | Self::$flag.0 )*
            }
        }

        impl std::ops::BitOr for $name {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self {
                Self(self.0 | rhs.0)
            }
        }

        impl std::ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }

        impl std::ops::BitAnd for $name {
            type Output = Self;
            fn bitand(self, rhs: Self) -> Self {
                Self(self.0 & rhs.0)
            }
        }

        impl std::ops::Not for $name {
            type Output = Self;
            fn not(self) -> Self {
                Self(!self.0)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let names = self.names();
                let unknown = self.0 & !Self::known_bits();
                if names.is_empty() && unknown == 0 {
                    return f.write_str("(empty)");
                }
                let mut first = true;
                for n in names {
                    if !first {
                        f.write_str(" | ")?;
                    }
                    f.write_str(n)?;
                    first = false;
                }
                if unknown != 0 {
                    if !first {
                        f.write_str(" | ")?;
                    }
                    write!(f, "{unknown:#x}")?;
                }
                Ok(())
            }
        }
    };
}
pub(crate) use flags;

#[cfg(test)]
mod tests {
    flags! {
        /// Test flags.
        pub struct TestFlags: u32 {
            const A = 1;
            const B = 2;
        }
    }

    #[test]
    fn debug_lists_names_and_unknown_bits() {
        assert_eq!(format!("{:?}", TestFlags::A | TestFlags::B), "A | B");
        assert_eq!(format!("{:?}", TestFlags(0x11)), "A | 0x10");
        assert_eq!(format!("{:?}", TestFlags::empty()), "(empty)");
        assert!((TestFlags::A | TestFlags::B).contains(TestFlags::B));
    }
}
