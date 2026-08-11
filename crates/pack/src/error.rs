use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Core(hakutaku_core::Error),
    InvalidInput(String),
    Identity(&'static str),
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

pub type Result<T> = std::result::Result<T, Error>;
