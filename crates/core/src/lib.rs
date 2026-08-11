//! Hakutaku's minimal game-runtime reader.

mod cache;
pub mod crypto;
mod error;
pub mod format;
pub mod io;
mod package;

pub use error::{Error, Result};
pub use format::{AccessClass, Availability, ProjectId, SegmentId};
pub use io::{
    DirectorySegmentSource, LocalFile, PositionedFile, SEGMENT_FILE_EXTENSION, SegmentSource,
    segment_file_name,
};
pub use package::{Asset, AssetCursor, AssetInfo, Package, ResourceBudget, SegmentInfo};
