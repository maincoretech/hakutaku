use crate::format::SegmentId;
use crate::{Error, Result};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Canonical filename extension for immutable segment files.
pub const SEGMENT_FILE_EXTENSION: &str = "taku";

#[must_use]
/// Returns the canonical lowercase digest filename for a segment.
pub fn segment_file_name(id: SegmentId) -> String {
    format!("{id}.{SEGMENT_FILE_EXTENSION}")
}

/// A thread-safe file-like object with cursor-independent reads.
pub trait PositionedFile: Send + Sync {
    /// Returns the immutable file length in bytes.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the backend cannot determine its length.
    fn len(&self) -> Result<u64>;
    /// Reports whether the immutable file has zero bytes.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`Self::len`].
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
    /// Fills `destination` from an absolute byte offset without shared cursor state.
    ///
    /// # Errors
    ///
    /// Returns an I/O or range error if the complete destination cannot be read.
    fn read_exact_at(&self, offset: u64, destination: &mut [u8]) -> Result<()>;
}

/// Resolves immutable segment IDs to random-readable files.
pub trait SegmentSource: Send + Sync {
    /// Opens the immutable segment identified by its signed content digest.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SegmentUnavailable`] when the segment is not installed.
    fn open(&self, id: SegmentId) -> Result<Arc<dyn PositionedFile>>;
}

/// Native filesystem implementation of [`PositionedFile`].
pub struct LocalFile {
    file: File,
    len: u64,
}

impl LocalFile {
    /// Opens a file and snapshots its current length.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the file or metadata cannot be read.
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
/// Resolves segment IDs beneath one local `data` directory.
pub struct DirectorySegmentSource {
    root: PathBuf,
}

impl DirectorySegmentSource {
    #[must_use]
    /// Creates a source rooted at a segment directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    /// Returns the canonical local path for `id` without opening it.
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
fn read_exact_at(file: &File, offset: u64, destination: &mut [u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    read_exact_with(offset, destination, |destination, offset| {
        file.read_at(destination, offset)
    })
}

fn read_exact_with(
    mut offset: u64,
    mut destination: &mut [u8],
    mut read: impl FnMut(&mut [u8], u64) -> std::io::Result<usize>,
) -> Result<()> {
    while !destination.is_empty() {
        match read(destination, offset) {
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
fn read_exact_at(file: &File, offset: u64, destination: &mut [u8]) -> Result<()> {
    use std::os::windows::fs::FileExt;
    read_exact_with(offset, destination, |destination, offset| {
        file.seek_read(destination, offset)
    })
}

#[cfg(not(any(unix, windows)))]
compile_error!("Hakutaku requires positioned file I/O on this platform");

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn local_and_directory_sources_preserve_positioned_io_semantics() {
        let root = std::env::temp_dir().join(format!(
            "hakutaku-io-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let id = SegmentId([0xabu8; 32]);
        let source = DirectorySegmentSource::new(&root);
        assert!(source.open(id).is_err());
        let path = source.segment_path(id);
        std::fs::write(&path, b"abcdef").unwrap();
        let file = source.open(id).unwrap();
        assert!(!file.is_empty().unwrap());
        assert_eq!(file.len().unwrap(), 6);
        let mut bytes = [0; 3];
        file.read_exact_at(2, &mut bytes).unwrap();
        assert_eq!(&bytes, b"cde");
        assert!(file.read_exact_at(5, &mut bytes).is_err());
        std::fs::remove_dir_all(&root).unwrap();

        let root_file = root.with_extension("file");
        std::fs::write(&root_file, b"not a directory").unwrap();
        assert!(DirectorySegmentSource::new(&root_file).open(id).is_err());
        std::fs::remove_file(root_file).unwrap();
    }

    #[test]
    fn positioned_read_retries_interrupts_and_reports_backend_errors() {
        let mut outcomes =
            VecDeque::from([Err(std::io::ErrorKind::Interrupted.into()), Ok(2), Ok(2)]);
        let mut bytes = [0; 4];
        read_exact_with(7, &mut bytes, |destination, offset| {
            let result = outcomes.pop_front().unwrap();
            if let Ok(read) = result {
                destination[..read].fill(offset as u8);
            }
            result
        })
        .unwrap();
        assert_eq!(bytes, [7, 7, 9, 9]);

        let mut byte = [0];
        assert!(read_exact_with(0, &mut byte, |_, _| Ok(0)).is_err());
        assert!(
            read_exact_with(0, &mut byte, |_, _| {
                Err(std::io::ErrorKind::PermissionDenied.into())
            })
            .is_err()
        );
        assert!(read_exact_with(u64::MAX, &mut byte, |_, _| Ok(1)).is_err());
    }
}
