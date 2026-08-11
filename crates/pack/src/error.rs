use std::fmt;

/// Failures produced by publisher identity and package construction operations.
#[derive(Debug)]
pub enum Error {
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// The runtime parser rejected generated or incremental package data.
    Core(hakutaku_core::Error),
    /// Publisher-supplied options or paths are invalid.
    InvalidInput(String),
    /// A publisher identity file is corrupt or inconsistent.
    Identity(&'static str),
    /// A cryptographic provider operation failed.
    Crypto(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Core(error) => write!(f, "package error: {error}"),
            Self::InvalidInput(reason) => write!(f, "invalid input: {reason}"),
            Self::Identity(reason) => write!(f, "invalid publisher identity: {reason}"),
            Self::Crypto(scope) => write!(f, "cryptographic operation failed: {scope}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Core(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<hakutaku_core::Error> for Error {
    fn from(value: hakutaku_core::Error) -> Self {
        Self::Core(value)
    }
}

/// Result type returned by Hakutaku publisher operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_stable_display_source_and_conversion_semantics() {
        let cases = [
            Error::Io(std::io::ErrorKind::NotFound.into()),
            Error::Core(hakutaku_core::Error::InvalidPath),
            Error::InvalidInput("option".into()),
            Error::Identity("checksum"),
            Error::Crypto("randomness"),
        ];
        let expected = [
            "I/O error:",
            "package error: asset path is not canonical",
            "invalid input: option",
            "invalid publisher identity: checksum",
            "cryptographic operation failed: randomness",
        ];
        for (error, expected) in cases.iter().zip(expected) {
            assert!(error.to_string().contains(expected));
        }
        assert!(std::error::Error::source(&cases[0]).is_some());
        assert!(std::error::Error::source(&cases[1]).is_some());
        for error in &cases[2..] {
            assert!(std::error::Error::source(error).is_none());
        }
        assert!(matches!(
            Error::from(std::io::Error::from(std::io::ErrorKind::Other)),
            Error::Io(_)
        ));
        assert!(matches!(
            Error::from(hakutaku_core::Error::InvalidRange),
            Error::Core(_)
        ));
    }
}
