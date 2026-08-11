use crate::format::SegmentId;
use std::fmt;

/// Failures produced by the Hakutaku parser and random-access reader.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    InvalidFormat(&'static str),
    UnsupportedVersion { major: u16, minor: u16 },
    LimitExceeded(&'static str),
    Authentication(&'static str),
    Signature,
    ProjectMismatch,
    SegmentUnavailable(SegmentId),
    AssetNotFound,
    InvalidPath,
    InvalidRange,
    Compression(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::InvalidFormat(reason) => write!(f, "invalid Hakutaku format: {reason}"),
            Self::UnsupportedVersion { major, minor } => {
                write!(f, "unsupported Hakutaku format version {major}.{minor}")
            }
            Self::LimitExceeded(limit) => write!(f, "Hakutaku limit exceeded: {limit}"),
            Self::Authentication(scope) => write!(f, "authentication failed for {scope}"),
            Self::Signature => f.write_str("snapshot publisher signature is invalid"),
            Self::ProjectMismatch => f.write_str("package belongs to a different project"),
            Self::SegmentUnavailable(id) => write!(f, "segment is unavailable: {id}"),
            Self::AssetNotFound => f.write_str("asset was not found"),
            Self::InvalidPath => f.write_str("asset path is not canonical"),
            Self::InvalidRange => f.write_str("asset read range is invalid"),
            Self::Compression(error) => write!(f, "zstd error: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::Compression(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
