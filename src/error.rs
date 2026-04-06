use std::fmt;

/// Errors returned by wirecap operations.
#[derive(Debug)]
pub enum Error {
    /// An I/O error from the underlying filesystem or stream.
    Io(std::io::Error),
    /// A format violation: bad magic, unsupported version, invalid field, etc.
    Format(String),
    /// The capture writer has shut down (channel closed).
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o error: {e}"),
            Self::Format(msg) => write!(f, "format error: {msg}"),
            Self::Closed => write!(f, "capture writer is closed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn display_io() {
        let e = Error::Io(std::io::Error::other("oops"));
        assert_eq!(e.to_string(), "i/o error: oops");
    }

    #[test]
    fn display_format() {
        let e = Error::Format("bad magic".into());
        assert_eq!(e.to_string(), "format error: bad magic");
    }

    #[test]
    fn display_closed() {
        let e = Error::Closed;
        assert_eq!(e.to_string(), "capture writer is closed");
    }

    #[test]
    fn source_io_returns_some() {
        let e = Error::Io(std::io::Error::other("inner"));
        assert!(e.source().is_some());
    }

    #[test]
    fn source_format_and_closed_return_none() {
        assert!(Error::Format("x".into()).source().is_none());
        assert!(Error::Closed.source().is_none());
    }

    #[test]
    fn from_io_error() {
        let io = std::io::Error::other("from-io");
        let e: Error = io.into();
        assert!(matches!(e, Error::Io(_)));
    }
}
