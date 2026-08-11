use crate::format::SegmentId;
use crate::{Error, Result};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const SEGMENT_FILE_EXTENSION: &str = "taku";

#[must_use]
pub fn segment_file_name(id: SegmentId) -> String {
    format!("{id}.{SEGMENT_FILE_EXTENSION}")
}

/// A thread-safe file-like object with cursor-independent reads.
pub trait PositionedFile: Send + Sync {
    fn len(&self) -> Result<u64>;
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
    fn read_exact_at(&self, offset: u64, destination: &mut [u8]) -> Result<()>;
}

/// Resolves immutable segment IDs to random-readable files.
pub trait SegmentSource: Send + Sync {
    fn open(&self, id: SegmentId) -> Result<Arc<dyn PositionedFile>>;
}

pub struct LocalFile {
    file: File,
    len: u64,
}

impl LocalFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

impl PositionedFile for LocalFile {
    fn len(&self) -> Result<u64> {
        Ok(self.len)
    }

    fn read_exact_at(&self, offset: u64, destination: &mut [u8]) -> Result<()> {
        read_exact_at(&self.file, offset, destination)
    }
}

#[derive(Clone, Debug)]
pub struct DirectorySegmentSource {
    root: PathBuf,
}

impl DirectorySegmentSource {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn segment_path(&self, id: SegmentId) -> PathBuf {
        self.root.join(segment_file_name(id))
    }
}

impl SegmentSource for DirectorySegmentSource {
    fn open(&self, id: SegmentId) -> Result<Arc<dyn PositionedFile>> {
        let path = self.segment_path(id);
        match LocalFile::open(path) {
            Ok(file) => Ok(Arc::new(file)),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::SegmentUnavailable(id))
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut offset: u64, mut destination: &mut [u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    while !destination.is_empty() {
        match file.read_at(destination, offset) {
            Ok(0) => return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into())),
            Ok(read) => {
                offset = offset.checked_add(read as u64).ok_or(Error::InvalidRange)?;
                destination = &mut destination[read..];
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut offset: u64, mut destination: &mut [u8]) -> Result<()> {
    use std::os::windows::fs::FileExt;
    while !destination.is_empty() {
        match file.seek_read(destination, offset) {
            Ok(0) => return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into())),
            Ok(read) => {
                offset = offset.checked_add(read as u64).ok_or(Error::InvalidRange)?;
                destination = &mut destination[read..];
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
compile_error!("Hakutaku requires positioned file I/O on this platform");
