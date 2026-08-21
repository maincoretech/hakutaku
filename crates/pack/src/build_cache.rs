use crate::source::{CompressionPolicy, SourceStamp};
use crate::{Error, Result};
use hakutaku_core::format::LayoutKind;
use hakutaku_core::{AccessClass, Availability, ProjectId};
use std::collections::HashMap;
use std::path::Path;

const MAGIC: &[u8; 8] = b"HAKBC002";
const HEADER_SIZE: usize = 48;
const ENTRY_HEADER_SIZE: usize = 51;
const CHECKSUM_SIZE: usize = 32;
const CHUNK_SIZE: usize = 44;
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_CHUNKS: usize = 10_000_000;

pub(crate) struct BuildCache {
    entries: HashMap<String, CachedEntry>,
}

#[derive(Clone)]
pub(crate) struct CachedEntry {
    pub(crate) stamp: SourceStamp,
    pub(crate) layout: LayoutKind,
    pub(crate) fixed_block_len: u32,
    pub(crate) access: AccessClass,
    pub(crate) availability: Availability,
    pub(crate) compression: CompressionPolicy,
    pub(crate) chunks: Vec<CachedChunk>,
}

#[derive(Clone, Copy)]
pub(crate) struct CachedChunk {
    pub(crate) logical_offset: u64,
    pub(crate) plain_len: u32,
    pub(crate) chunk_id: [u8; 32],
}

impl BuildCache {
    pub(crate) fn load(
        path: &Path,
        project_id: ProjectId,
        release_sequence: u64,
    ) -> Result<Option<Self>> {
        let metadata = match path.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if metadata.len() > MAX_CACHE_BYTES {
            return Ok(None);
        }
        let bytes = std::fs::read(path)?;
        Ok(Self::parse(&bytes, project_id, release_sequence))
    }

    pub(crate) fn get(&self, path: &str) -> Option<&CachedEntry> {
        self.entries.get(path)
    }

    fn parse(bytes: &[u8], project_id: ProjectId, release_sequence: u64) -> Option<Self> {
        if bytes.len() < HEADER_SIZE + CHECKSUM_SIZE
            || bytes.get(..8)? != MAGIC
            || read_u16(bytes, 8)? != 2
            || read_u16(bytes, 10)? as usize != HEADER_SIZE
            || bytes.get(12..28)? != project_id.0
            || read_u64(bytes, 28)? != release_sequence
            || bytes.get(40..48)?.iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let payload_len = bytes.len().checked_sub(CHECKSUM_SIZE)?;
        if blake3::hash(bytes.get(..payload_len)?).as_bytes() != bytes.get(payload_len..)? {
            return None;
        }
        let entry_count = read_u32(bytes, 36)? as usize;
        if entry_count > MAX_ENTRIES {
            return None;
        }
        let mut offset = HEADER_SIZE;
        let mut total_chunks = 0_usize;
        let mut entries = HashMap::with_capacity(entry_count);
        for _ in 0..entry_count {
            let path_len = read_u16(bytes, offset)? as usize;
            let layout = parse_layout(*bytes.get(offset + 2)?)?;
            let access = parse_access(*bytes.get(offset + 3)?)?;
            let availability = parse_availability(*bytes.get(offset + 4)?)?;
            let compression = parse_compression(*bytes.get(offset + 5)?)?;
            let modified_known = *bytes.get(offset + 6)?;
            if modified_known > 1 {
                return None;
            }
            let fixed_block_len = read_u32(bytes, offset + 7)?;
            let len = read_u64(bytes, offset + 11)?;
            let modified_secs = read_u64(bytes, offset + 19)?;
            let modified_nanos = read_u32(bytes, offset + 27)?;
            let device = read_u64(bytes, offset + 31)?;
            let inode = read_u64(bytes, offset + 39)?;
            let chunk_count = read_u32(bytes, offset + 47)? as usize;
            total_chunks = total_chunks.checked_add(chunk_count)?;
            if total_chunks > MAX_CHUNKS || modified_nanos >= 1_000_000_000 {
                return None;
            }
            offset = offset.checked_add(ENTRY_HEADER_SIZE)?;
            let path_end = offset.checked_add(path_len)?;
            let path = std::str::from_utf8(bytes.get(offset..path_end)?).ok()?;
            hakutaku_core::format::validate_canonical_path(path).ok()?;
            offset = path_end;
            let mut chunks = Vec::with_capacity(chunk_count);
            for _ in 0..chunk_count {
                let chunk_end = offset.checked_add(CHUNK_SIZE)?;
                chunks.push(CachedChunk {
                    logical_offset: read_u64(bytes, offset)?,
                    plain_len: read_u32(bytes, offset + 8)?,
                    chunk_id: bytes.get(offset + 12..chunk_end)?.try_into().ok()?,
                });
                offset = chunk_end;
            }
            let stamp = SourceStamp {
                len,
                modified: (modified_known == 1).then_some((modified_secs, modified_nanos)),
                #[cfg(unix)]
                device,
                #[cfg(unix)]
                inode,
            };
            #[cfg(not(unix))]
            let _ = (device, inode);
            if entries
                .insert(
                    path.to_owned(),
                    CachedEntry {
                        stamp,
                        layout,
                        fixed_block_len,
                        access,
                        availability,
                        compression,
                        chunks,
                    },
                )
                .is_some()
            {
                return None;
            }
        }
        (offset == payload_len).then_some(Self { entries })
    }
}

pub(crate) fn save(
    path: &Path,
    project_id: ProjectId,
    release_sequence: u64,
    entries: &[(String, CachedEntry)],
) -> Result<()> {
    let entry_count = u32::try_from(entries.len())
        .map_err(|_| Error::InvalidInput("too many build-cache entries".into()))?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    bytes.extend_from_slice(&project_id.0);
    bytes.extend_from_slice(&release_sequence.to_le_bytes());
    bytes.extend_from_slice(&entry_count.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    for (logical_path, entry) in entries {
        let path_len = u16::try_from(logical_path.len())
            .map_err(|_| Error::InvalidInput("build-cache path is too long".into()))?;
        let chunk_count = u32::try_from(entry.chunks.len())
            .map_err(|_| Error::InvalidInput("too many cached chunks".into()))?;
        bytes.extend_from_slice(&path_len.to_le_bytes());
        bytes.push(entry.layout as u8);
        bytes.push(entry.access as u8);
        bytes.push(entry.availability as u8);
        bytes.push(entry.compression as u8);
        bytes.push(u8::from(entry.stamp.modified.is_some()));
        bytes.extend_from_slice(&entry.fixed_block_len.to_le_bytes());
        bytes.extend_from_slice(&entry.stamp.len.to_le_bytes());
        let (modified_secs, modified_nanos) = entry.stamp.modified.unwrap_or_default();
        bytes.extend_from_slice(&modified_secs.to_le_bytes());
        bytes.extend_from_slice(&modified_nanos.to_le_bytes());
        #[cfg(unix)]
        let (device, inode) = (entry.stamp.device, entry.stamp.inode);
        #[cfg(not(unix))]
        let (device, inode) = (0_u64, 0_u64);
        bytes.extend_from_slice(&device.to_le_bytes());
        bytes.extend_from_slice(&inode.to_le_bytes());
        bytes.extend_from_slice(&chunk_count.to_le_bytes());
        bytes.extend_from_slice(logical_path.as_bytes());
        for chunk in &entry.chunks {
            bytes.extend_from_slice(&chunk.logical_offset.to_le_bytes());
            bytes.extend_from_slice(&chunk.plain_len.to_le_bytes());
            bytes.extend_from_slice(&chunk.chunk_id);
        }
    }
    let checksum = blake3::hash(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    let temporary = path.with_extension(format!("cache.part-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn parse_layout(value: u8) -> Option<LayoutKind> {
    match value {
        0 => Some(LayoutKind::Fixed),
        1 => Some(LayoutKind::ContentDefined),
        _ => None,
    }
}

fn parse_access(value: u8) -> Option<AccessClass> {
    match value {
        0 => Some(AccessClass::Hot),
        1 => Some(AccessClass::Normal),
        2 => Some(AccessClass::Streaming),
        3 => Some(AccessClass::Transient),
        _ => None,
    }
}

fn parse_availability(value: u8) -> Option<Availability> {
    match value {
        0 => Some(Availability::Required),
        1 => Some(Availability::Deferred),
        _ => None,
    }
}

fn parse_compression(value: u8) -> Option<CompressionPolicy> {
    match value {
        0 => Some(CompressionPolicy::Auto),
        1 => Some(CompressionPolicy::Raw),
        _ => None,
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hakutaku-build-cache-{name}-{}",
            std::process::id()
        ))
    }

    fn stamp(len: u64) -> SourceStamp {
        SourceStamp {
            len,
            modified: Some((42, 7)),
            #[cfg(unix)]
            device: 11,
            #[cfg(unix)]
            inode: 13,
        }
    }

    fn entry(
        path_byte: u8,
        layout: LayoutKind,
        access: AccessClass,
        availability: Availability,
    ) -> CachedEntry {
        CachedEntry {
            stamp: stamp(4),
            layout,
            fixed_block_len: u32::from(layout == LayoutKind::Fixed) * 4,
            access,
            availability,
            compression: CompressionPolicy::Auto,
            chunks: vec![CachedChunk {
                logical_offset: 0,
                plain_len: 4,
                chunk_id: [path_byte; 32],
            }],
        }
    }

    fn rewrite_checksum(bytes: &mut [u8]) {
        let payload = bytes.len() - CHECKSUM_SIZE;
        let checksum = blake3::hash(&bytes[..payload]);
        bytes[payload..].copy_from_slice(checksum.as_bytes());
    }

    #[test]
    fn cache_roundtrip_and_corruption_limits_cover_every_wire_variant() {
        let root = scratch("wire");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("cache");
        let project = ProjectId([9; 16]);
        let mut entries = vec![
            (
                "a".into(),
                entry(
                    1,
                    LayoutKind::Fixed,
                    AccessClass::Hot,
                    Availability::Required,
                ),
            ),
            (
                "b".into(),
                entry(
                    2,
                    LayoutKind::ContentDefined,
                    AccessClass::Normal,
                    Availability::Deferred,
                ),
            ),
            (
                "c".into(),
                entry(
                    3,
                    LayoutKind::Fixed,
                    AccessClass::Streaming,
                    Availability::Required,
                ),
            ),
            (
                "d".into(),
                entry(
                    4,
                    LayoutKind::Fixed,
                    AccessClass::Transient,
                    Availability::Required,
                ),
            ),
        ];
        entries[2].1.compression = CompressionPolicy::Raw;
        save(&path, project, 7, &entries).unwrap();
        let loaded = BuildCache::load(&path, project, 7).unwrap().unwrap();
        assert_eq!(loaded.get("b").unwrap().chunks.len(), 1);
        assert_eq!(loaded.get("c").unwrap().compression, CompressionPolicy::Raw);
        assert!(
            BuildCache::load(&root.join("missing"), project, 7)
                .unwrap()
                .is_none()
        );
        assert!(BuildCache::load(&path, project, 8).unwrap().is_none());

        let valid = std::fs::read(&path).unwrap();
        let mut damaged = valid.clone();
        damaged[12] ^= 1;
        rewrite_checksum(&mut damaged);
        assert!(BuildCache::parse(&damaged, project, 7).is_none());
        let mut damaged = valid.clone();
        damaged[40] = 1;
        rewrite_checksum(&mut damaged);
        assert!(BuildCache::parse(&damaged, project, 7).is_none());
        let mut damaged = valid.clone();
        damaged[36..40].copy_from_slice(&((MAX_ENTRIES as u32) + 1).to_le_bytes());
        rewrite_checksum(&mut damaged);
        assert!(BuildCache::parse(&damaged, project, 7).is_none());
        let mut damaged = valid.clone();
        damaged[48 + 5] = 2;
        rewrite_checksum(&mut damaged);
        assert!(BuildCache::parse(&damaged, project, 7).is_none());
        let mut damaged = valid.clone();
        damaged[48 + 6] = 2;
        rewrite_checksum(&mut damaged);
        assert!(BuildCache::parse(&damaged, project, 7).is_none());
        let mut damaged = valid.clone();
        damaged[48 + 27..48 + 31].copy_from_slice(&1_000_000_000_u32.to_le_bytes());
        rewrite_checksum(&mut damaged);
        assert!(BuildCache::parse(&damaged, project, 7).is_none());
        let mut damaged = valid;
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        assert!(BuildCache::parse(&damaged, project, 7).is_none());

        let duplicate = vec![entries[0].clone(), entries[0].clone()];
        save(&root.join("duplicate"), project, 7, &duplicate).unwrap();
        assert!(
            BuildCache::load(&root.join("duplicate"), project, 7)
                .unwrap()
                .is_none()
        );

        let oversized = root.join("oversized");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_CACHE_BYTES + 1)
            .unwrap();
        assert!(BuildCache::load(&oversized, project, 7).unwrap().is_none());
        assert_eq!(parse_layout(2), None);
        assert_eq!(parse_access(4), None);
        assert_eq!(parse_availability(2), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
