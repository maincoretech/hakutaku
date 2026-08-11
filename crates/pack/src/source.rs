use crate::{Error, Result};
use hakutaku_core::AccessClass;
use hakutaku_core::format::{LayoutKind, validate_canonical_path};
use std::path::{Path, PathBuf};

const STREAM_BLOCK: usize = 256 * 1024;
pub(crate) const BULK_BLOCK: usize = 1024 * 1024;
pub(crate) const HOT_FILE_LIMIT: u64 = 32 * 1024;
pub(crate) const CONTENT_DEFINED_LIMIT: u64 = 64 * 1024 * 1024;

pub(crate) struct SourceFile {
    pub(crate) host_path: PathBuf,
    pub(crate) logical_path: String,
    pub(crate) len: u64,
}

pub(crate) fn classify(source: &SourceFile) -> (LayoutKind, u32, AccessClass) {
    let extension = source
        .logical_path
        .rsplit_once('.')
        .map_or("", |(_, extension)| extension)
        .to_ascii_lowercase();
    let streaming = matches!(
        extension.as_str(),
        "mp4" | "webm" | "mkv" | "mov" | "m4v" | "mp3" | "ogg" | "opus" | "flac" | "wav"
    );
    if source.len <= HOT_FILE_LIMIT {
        let fixed = u32::try_from(source.len.max(1)).unwrap_or(1);
        return (LayoutKind::Fixed, fixed, AccessClass::Hot);
    }
    if streaming {
        return (
            LayoutKind::Fixed,
            STREAM_BLOCK as u32,
            AccessClass::Streaming,
        );
    }
    if source.len <= CONTENT_DEFINED_LIMIT {
        return (LayoutKind::ContentDefined, 0, AccessClass::Normal);
    }
    (LayoutKind::Fixed, BULK_BLOCK as u32, AccessClass::Normal)
}

pub(crate) fn collect_files(root: &Path) -> Result<Vec<SourceFile>> {
    let root = root.canonicalize()?;
    let mut files = Vec::new();
    collect_directory(&root, &root, &mut files)?;
    files.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    Ok(files)
}

fn collect_directory(root: &Path, directory: &Path, files: &mut Vec<SourceFile>) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = entry.file_type()?;
        if metadata.is_symlink() {
            return Err(Error::InvalidInput(format!(
                "symbolic links are not package resources: {}",
                entry.path().display()
            )));
        }
        if metadata.is_dir() {
            collect_directory(root, &entry.path(), files)?;
        } else if metadata.is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|_| Error::InvalidInput("resource escaped input root".into()))?
                .to_path_buf();
            let logical_path = relative
                .components()
                .map(|component| {
                    component.as_os_str().to_str().ok_or_else(|| {
                        Error::InvalidInput(format!("path is not UTF-8: {}", relative.display()))
                    })
                })
                .collect::<Result<Vec<_>>>()?
                .join("/");
            validate_canonical_path(&logical_path)?;
            files.push(SourceFile {
                host_path: entry.path(),
                logical_path,
                len: entry.metadata()?.len(),
            });
        }
    }
    Ok(())
}
