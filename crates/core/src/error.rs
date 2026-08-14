use crate::format::SegmentId;
use std::fmt;

/// Failures produced by the Hakutaku parser and random-access reader.
#[derive(Debug)]
pub enum Error {
    /// A filesystem or positioned-read operation failed.
    Io(std::io::Error),
    /// Bytes violate a structural or canonical format invariant.
    InvalidFormat(&'static str),
    /// The package uses an unsupported major or minor format version.
    UnsupportedVersion {
        /// Encoded major version.
        major: u16,
        /// Encoded minor version.
        minor: u16,
    },
    /// An encoded allocation or record count exceeds the reader's hard limit.
    LimitExceeded(&'static str),
    /// Authenticated encryption failed for the named scope.
    Authentication(&'static str),
    /// The publisher signature or its key identifier is invalid.
    Signature,
    /// A validly signed snapshot is older than the caller's accepted floor.
    ReleaseRollback {
        /// Lowest release sequence accepted by the caller.
        minimum: u64,
        /// Sequence presented by the authenticated snapshot.
        actual: u64,
    },
    /// A segment belongs to another project.
    ProjectMismatch,
    /// A required immutable segment cannot be opened.
    SegmentUnavailable(SegmentId),
    /// No asset has the requested canonical path.
    AssetNotFound,
    /// An asset path is not canonical.
    InvalidPath,
    /// A requested byte range cannot be represented or lies outside the asset.
    InvalidRange,
    /// Zstandard decompression failed.
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
            Self::ReleaseRollback { minimum, actual } => write!(
                f,
                "snapshot release {actual} is older than required release {minimum}"
            ),
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

/// Result type returned by Hakutaku runtime operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_stable_display_and_source_semantics() {
        let cases = [
            Error::Io(std::io::ErrorKind::NotFound.into()),
            Error::InvalidFormat("record"),
            Error::UnsupportedVersion { major: 2, minor: 3 },
            Error::LimitExceeded("files"),
            Error::Authentication("catalog"),
            Error::Signature,
            Error::ReleaseRollback {
                minimum: 4,
                actual: 3,
            },
            Error::ProjectMismatch,
            Error::SegmentUnavailable(SegmentId([1; 32])),
            Error::AssetNotFound,
            Error::InvalidPath,
            Error::InvalidRange,
            Error::Compression(std::io::ErrorKind::InvalidData.into()),
        ];
        let expected = [
            "I/O error:",
            "invalid Hakutaku format: record",
            "unsupported Hakutaku format version 2.3",
            "Hakutaku limit exceeded: files",
            "authentication failed for catalog",
            "snapshot publisher signature is invalid",
            "snapshot release 3 is older than required release 4",
            "package belongs to a different project",
            "segment is unavailable:",
            "asset was not found",
            "asset path is not canonical",
            "asset read range is invalid",
            "zstd error:",
        ];
        for (error, expected) in cases.iter().zip(expected) {
            assert!(error.to_string().contains(expected));
        }
        assert!(std::error::Error::source(&cases[0]).is_some());
        assert!(std::error::Error::source(&cases[12]).is_some());
        for error in &cases[1..12] {
            assert!(std::error::Error::source(error).is_none());
        }
        assert!(matches!(
            Error::from(std::io::Error::from(std::io::ErrorKind::Other)),
            Error::Io(_)
        ));
    }
}
