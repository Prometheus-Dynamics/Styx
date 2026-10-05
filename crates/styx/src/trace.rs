//! Diagnostics: `tracing`'s macros with feature `tracing` (default), no-ops without it.
//!
//! Styx logs through `crate::trace::{debug, info, warn, error, trace, trace_span}`. Without the
//! feature the macros expand to `if false { .. }` blocks that only borrow their arguments (so
//! the call sites type-check and their bindings count as used) and compile to nothing; spans
//! are a zero-sized [`Span`] whose guard does nothing.

#[cfg(feature = "tracing")]
pub(crate) use tracing::{debug, error, info, trace, trace_span, warn};

#[cfg(not(feature = "tracing"))]
mod noop {
    /// The no-op span `trace_span!` gives without `tracing`.
    pub(crate) struct Span;

    /// The guard of [`Span::enter`].
    pub(crate) struct Entered;

    impl Span {
        #[inline(always)]
        pub(crate) fn enter(&self) -> Entered {
            Entered
        }
    }

    /// Borrows every field value and the message arguments of a `tracing` call, in a block
    /// that never runs.
    macro_rules! fields {
        () => {};
        ($fmt:literal $($args:tt)*) => {
            let _ = ::std::format_args!($fmt $($args)*);
        };
        (% $($k:ident).+ $(, $($rest:tt)*)?) => {
            let _ = &$($k).+;
            $crate::trace::fields!($($($rest)*)?);
        };
        (? $($k:ident).+ $(, $($rest:tt)*)?) => {
            let _ = &$($k).+;
            $crate::trace::fields!($($($rest)*)?);
        };
        ($($k:ident).+ = % $e:expr $(, $($rest:tt)*)?) => {
            let _ = &$e;
            $crate::trace::fields!($($($rest)*)?);
        };
        ($($k:ident).+ = ? $e:expr $(, $($rest:tt)*)?) => {
            let _ = &$e;
            $crate::trace::fields!($($($rest)*)?);
        };
        ($($k:ident).+ = $e:expr $(, $($rest:tt)*)?) => {
            let _ = &$e;
            $crate::trace::fields!($($($rest)*)?);
        };
        ($($k:ident).+ $(, $($rest:tt)*)?) => {
            let _ = &$($k).+;
            $crate::trace::fields!($($($rest)*)?);
        };
    }

    macro_rules! event {
        ($($t:tt)*) => {
            if false {
                $crate::trace::fields!($($t)*);
            }
        };
    }

    macro_rules! trace_span {
        ($name:expr $(, $($rest:tt)*)?) => {{
            if false {
                let _ = $name;
                $crate::trace::fields!($($($rest)*)?);
            }
            $crate::trace::Span
        }};
    }

    pub(crate) use {
        event as debug, event as error, event as info, event as trace, event as warn, fields,
        trace_span,
    };
}

#[cfg(not(feature = "tracing"))]
pub(crate) use noop::*;
