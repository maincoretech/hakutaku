#![deny(missing_docs)]

//! Hakutaku's minimal game-runtime reader.

mod cache;
/// Cryptographic key derivation, authentication, and wire-domain helpers.
pub mod crypto;
mod error;
/// Normative Hakutaku v1 wire-format records and codecs.
pub mod format;
/// Random-access file and immutable segment-source abstractions.
pub mod io;
mod package;

pub use error::{Error, Result};
pub use format::{AccessClass, Availability, ProjectId, SegmentId};
pub use io::{
    DirectorySegmentSource, LocalFile, PositionedFile, SEGMENT_FILE_EXTENSION, SegmentSource,
    segment_file_name,
};
pub use package::{Asset, AssetCursor, AssetInfo, Package, ResourceBudget, SegmentInfo};
