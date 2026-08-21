use crate::identity::{is_hakutaku_key_file, is_hakutaku_key_magic};
use crate::{Error, Result};
use hakutaku_core::AccessClass;
use hakutaku_core::format::{Codec, LayoutKind, validate_canonical_path};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const STREAM_BLOCK: usize = 256 * 1024;
const SHORT_AUDIO_LIMIT: u64 = 1024 * 1024;
pub(crate) const BULK_BLOCK: usize = 1024 * 1024;
pub(crate) const HOT_FILE_LIMIT: u64 = 32 * 1024;
pub(crate) const CONTENT_DEFINED_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CompressionPolicy {
    Auto = 0,
    Raw = 1,
}

impl CompressionPolicy {
    pub(crate) const fn accepts(self, codec: Codec) -> bool {
        matches!(self, Self::Auto) || matches!(codec, Codec::Raw)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceClass {
    pub(crate) layout: LayoutKind,
    pub(crate) fixed_block_len: u32,
    pub(crate) access: AccessClass,
    pub(crate) compression: CompressionPolicy,
}

pub(crate) struct SourceFile {
    pub(crate) host_path: PathBuf,
    pub(crate) logical_path: String,
    pub(crate) len: u64,
    stamp: SourceStamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceStamp {
    pub(crate) len: u64,
    pub(crate) modified: Option<(u64, u32)>,
    #[cfg(unix)]
    pub(crate) device: u64,
    #[cfg(unix)]
    pub(crate) inode: u64,
}

impl SourceStamp {
    fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: metadata.len(),
            modified: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| (duration.as_secs(), duration.subsec_nanos())),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }

    fn matches(&self, metadata: &Metadata) -> bool {
        metadata.is_file() && Self::from_metadata(metadata) == *self
    }
}

impl SourceFile {
    pub(crate) fn open_verified(&self) -> Result<File> {
        let file = File::open(&self.host_path)?;
        self.validate_open_file(&file)?;
        let mut magic = [0_u8; 8];
        let mut reader = &file;
        let read = reader.read(&mut magic)?;
        reader.seek(SeekFrom::Start(0))?;
        if read == magic.len() && is_hakutaku_key_magic(&magic) {
            return Err(Error::InvalidInput(format!(
                "Hakutaku key material cannot be packaged as a resource: {}",
                self.host_path.display()
            )));
        }
        Ok(file)
    }

    pub(crate) fn validate_open_file(&self, file: &File) -> Result<()> {
        if !self.stamp.matches(&file.metadata()?) {
            return Err(Error::InvalidInput(format!(
                "source changed while packing: {}",
                self.host_path.display()
            )));
        }
        Ok(())
    }

    pub(crate) const fn stamp(&self) -> SourceStamp {
        self.stamp
    }

    #[cfg(test)]
    pub(crate) fn test(logical_path: impl Into<String>, len: u64) -> Self {
        Self {
            host_path: PathBuf::new(),
            logical_path: logical_path.into(),
            len,
            stamp: SourceStamp {
                len,
                modified: None,
                #[cfg(unix)]
                device: 0,
                #[cfg(unix)]
                inode: 0,
            },
        }
    }
}

pub(crate) fn classify(source: &SourceFile) -> SourceClass {
    let extension = source
        .logical_path
        .rsplit_once('.')
        .map_or("", |(_, extension)| extension)
        .to_ascii_lowercase();
    let audio = matches!(extension.as_str(), "mp3" | "ogg" | "opus" | "flac" | "wav");
    let video = matches!(extension.as_str(), "mp4" | "webm" | "mkv" | "mov" | "m4v");
    let already_compressed = matches!(
        extension.as_str(),
        "webp"
            | "png"
            | "jpg"
            | "jpeg"
            | "avif"
            | "opus"
            | "mp3"
            | "ogg"
            | "flac"
            | "mp4"
            | "webm"
            | "mkv"
            | "mov"
            | "m4v"
    );
    let compression = if already_compressed {
        CompressionPolicy::Raw
    } else {
        CompressionPolicy::Auto
    };
    if video || audio {
        let long_lived_audio = source.logical_path.split('/').any(|component| {
            component.eq_ignore_ascii_case("bgm") || component.eq_ignore_ascii_case("music")
        });
        let access = if video || long_lived_audio || source.len > SHORT_AUDIO_LIMIT {
            AccessClass::Streaming
        } else {
            AccessClass::Transient
        };
        return SourceClass {
            layout: LayoutKind::Fixed,
            fixed_block_len: STREAM_BLOCK as u32,
            access,
            compression,
        };
    }
    if source.len <= HOT_FILE_LIMIT {
        let fixed = u32::try_from(source.len.max(1)).unwrap_or(1);
        return SourceClass {
            layout: LayoutKind::Fixed,
            fixed_block_len: fixed,
            access: AccessClass::Hot,
            compression,
        };
    }
    if source.len <= CONTENT_DEFINED_LIMIT {
        return SourceClass {
            layout: LayoutKind::ContentDefined,
            fixed_block_len: 0,
            access: AccessClass::Normal,
            compression,
        };
    }
    SourceClass {
        layout: LayoutKind::Fixed,
        fixed_block_len: BULK_BLOCK as u32,
        access: AccessClass::Normal,
        compression,
    }
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
            if is_hakutaku_key_file(entry.path())? {
                return Err(Error::InvalidInput(format!(
                    "Hakutaku key material cannot be packaged as a resource: {}",
                    entry.path().display()
                )));
            }
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
            let metadata = entry.metadata()?;
            files.push(SourceFile {
                host_path: entry.path(),
                logical_path,
                len: metadata.len(),
                stamp: SourceStamp::from_metadata(&metadata),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hakutaku-source-{name}-{}", std::process::id()))
    }

    #[test]
    fn verified_open_rechecks_key_magic_on_the_open_handle() {
        let root = scratch("magic");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("renamed.bin");
        std::fs::write(&path, b"HAKID001not-a-valid-identity").unwrap();
        let metadata = path.metadata().unwrap();
        let source = SourceFile {
            host_path: path,
            logical_path: "renamed.bin".into(),
            len: metadata.len(),
            stamp: SourceStamp::from_metadata(&metadata),
        };
        assert!(source.open_verified().is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn source_scan_rejects_non_utf8_names() {
        use std::os::unix::ffi::OsStringExt;

        let root = scratch("non-utf8");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        if std::fs::write(
            root.join(std::ffi::OsString::from_vec(vec![0xff])),
            b"asset",
        )
        .is_err()
        {
            // Some macOS volumes reject non-UTF-8 names before Hakutaku sees them.
            std::fs::remove_dir_all(root).unwrap();
            return;
        }
        assert!(collect_files(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
