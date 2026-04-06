mod buggify;
mod error;
mod format;
mod reader;
mod writer;

pub use error::Error;
pub use format::{Dir, ReadEntry, WriteEntry};
pub use reader::{discover_files, find_active_file, WcapReader, WcapTailer};
pub use writer::{Capture, CaptureConfig, WcapWriter};

/// Total number of times any `buggify!()` injection site has fired since
/// process start. Always returns 0 unless the crate was built with
/// `--features buggify` *and* buggify was enabled at runtime
/// (`FERRO_BUGGIFY=1` env var or `ferro_buggify::enable()`).
///
/// Tests inspect this to prove fault injection actually ran.
#[cfg(feature = "buggify")]
pub fn buggify_fires() -> u64 {
    buggify::fire_count()
}
