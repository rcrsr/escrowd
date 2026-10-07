//! The daemon's errors. Each variant is one row of the protocol's status table
//! (`docs/protocol.md`), so a client can tell "no such scope" from "try again" from
//! a failure of the host. An OS error is `Io`, whatever its kind: an ENOENT met
//! while committing is not "no scope".

use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    /// No scope with that id (NOT_FOUND).
    NoScope(String),
    /// The scope is not in the state the call needs: open, closed, held, or the tier
    /// is not pending (FAILED_PRECONDITION).
    State(String),
    /// A missing or wrong token, or a verdict the caller may not give
    /// (PERMISSION_DENIED).
    Denied(String),
    /// A malformed request (INVALID_ARGUMENT).
    Invalid(String),
    /// A commit rolled back: nothing was applied and the scope is still closed;
    /// decide again (ABORTED).
    Aborted(String),
    /// Anything else, every OS error included (INTERNAL).
    Io(io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn no_scope(id: &str) -> Self {
        Error::NoScope(format!("no scope {id}"))
    }

    pub fn is_no_scope(&self) -> bool {
        matches!(self, Error::NoScope(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoScope(m) | Error::State(m) | Error::Denied(m) | Error::Invalid(m) | Error::Aborted(m) => {
                f.write_str(m)
            }
            Error::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// For callers that speak `io::Result` (the exec socket, start-up).
impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        let kind = match &e {
            Error::Io(_) => {
                let Error::Io(e) = e else { unreachable!() };
                return e;
            }
            Error::NoScope(_) => io::ErrorKind::NotFound,
            Error::State(_) | Error::Invalid(_) => io::ErrorKind::InvalidInput,
            Error::Denied(_) => io::ErrorKind::PermissionDenied,
            Error::Aborted(_) => io::ErrorKind::Interrupted,
        };
        io::Error::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_os_error_stays_an_os_error() {
        let e: Error = io::Error::from_raw_os_error(libc::ENOENT).into();
        assert!(matches!(e, Error::Io(_)));
        assert!(!e.is_no_scope());
        let back: io::Error = e.into();
        assert_eq!(back.raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn domain_errors_keep_their_message() {
        let e = Error::no_scope("s1");
        assert_eq!(e.to_string(), "no scope s1");
        assert_eq!(io::Error::from(e).kind(), io::ErrorKind::NotFound);
    }
}
