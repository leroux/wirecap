//! Internal macros for ferro-buggify integration.
//!
//! These wrappers let call sites in writer.rs and reader.rs invoke fault
//! injection without sprinkling `#[cfg(feature = "buggify")]` everywhere.
//! Both macros expand at the *call site*, so `file!()` / `line!()` inside
//! `ferro_buggify::buggify!()` resolve to the actual injection site — which
//! is essential for per-site activation and swarm testing.
//!
//! When the `buggify` feature is off, both macros expand to no-ops with no
//! runtime cost.
//!
//! ## Fire counter
//!
//! When the `buggify` feature is on, every macro expansion that fires
//! increments a global atomic counter (`fire_count`). Tests inspect this
//! counter via [`crate::buggify_fires`] to *prove* injection actually ran
//! (otherwise, "no panic" assertions could pass even if every site returned
//! false). The counter is process-global; tests should snapshot before/after.

#[cfg(feature = "buggify")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "buggify")]
static FIRE_COUNT: AtomicU64 = AtomicU64::new(0);

/// Total number of times any `buggify!()` site has fired since process start.
///
/// Always returns 0 when the `buggify` feature is disabled.
#[cfg(feature = "buggify")]
pub(crate) fn fire_count() -> u64 {
    FIRE_COUNT.load(Ordering::Relaxed)
}

/// Internal: increment the fire counter. Called by the macros, not by users.
#[cfg(feature = "buggify")]
#[doc(hidden)]
pub fn record_fire() {
    FIRE_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Returns `true` if buggify decides to fire at this call site.
///
/// Always returns `false` when the `buggify` feature is disabled.
#[cfg(feature = "buggify")]
macro_rules! buggify {
    () => {{
        let __fired = ::ferro_buggify::buggify!();
        if __fired {
            $crate::buggify::record_fire();
        }
        __fired
    }};
}

#[cfg(not(feature = "buggify"))]
macro_rules! buggify {
    () => {
        false
    };
}

/// Early-returns `Err(Error::Io(...))` if buggify fires at this call site.
///
/// Use at the entry of a fallible function to simulate an I/O failure.
/// No-op when the `buggify` feature is disabled.
#[cfg(feature = "buggify")]
macro_rules! buggify_io_err {
    ($label:literal) => {
        if ::ferro_buggify::buggify!() {
            $crate::buggify::record_fire();
            return Err($crate::error::Error::Io(::std::io::Error::other(concat!(
                "buggified ", $label
            ))));
        }
    };
}

#[cfg(not(feature = "buggify"))]
macro_rules! buggify_io_err {
    ($label:literal) => {};
}

pub(crate) use buggify;
pub(crate) use buggify_io_err;
