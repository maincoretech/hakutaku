//! Normative Hakutaku v1 wire records.
//!
//! Every record is encoded explicitly. Rust struct layout and serde are never
//! part of the format contract.

use crate::{Error, Result};
use std::fmt;
use std::sync::Arc;

pub const SNAPSHOT_HEADER_SIZE: usize = 4096;
pub const SEGMENT_HEADER_SIZE: usize = 4096;
pub const CATALOG_HEADER_SIZE: usize = 64;
pub const SEGMENT_RECORD_SIZE: usize = 96;
pub const FILE_RECORD_SIZE: usize = 32;
pub const PATH_SLOT_SIZE: usize = 16;
pub const PAGE_RECORD_SIZE: usize = 64;
pub const PAGE_HEADER_SIZE: usize = 16;
pub const BLOCK_REF_SIZE: usize = 48;
pub const REUSE_RECORD_SIZE: usize = 80;
pub const BLOCKS_PER_MAP_PAGE: usize = (16 * 1024 - PAGE_HEADER_SIZE) / BLOCK_REF_SIZE;
pub const REUSE_PER_PAGE: usize = (16 * 1024 - PAGE_HEADER_SIZE) / REUSE_RECORD_SIZE;

pub const MAX_CATALOG_PLAIN_LEN: usize = 64 * 1024 * 1024;
pub const MAX_CATALOG_STORED_LEN: usize = MAX_CATALOG_PLAIN_LEN + 1024 * 1024 + 16;
pub const MAX_PAGE_PLAIN_LEN: usize = 1024 * 1024;
pub const MAX_BLOCK_PLAIN_LEN: usize = 1024 * 1024;
pub const MAX_SEGMENTS: usize = 128;
pub const MAX_FILES: usize = 1_000_000;
pub const MAX_BLOCKS: usize = 10_000_000;
pub const MAX_PAGES: usize = 100_000;
pub const MAX_PATH_POOL_LEN: usize = 32 * 1024 * 1024;
pub const EMPTY_PATH_SLOT: u32 = u32::MAX;

const SNAPSHOT_MAGIC: &[u8; 8] = b"HAKU0001";
const CATALOG_MAGIC: &[u8; 8] = b"HAKCAT01";
const SEGMENT_MAGIC: &[u8; 8] = b"HAKSEG01";
const MAP_PAGE_MAGIC: &[u8; 8] = b"HAKMAP01";
const REUSE_PAGE_MAGIC: &[u8; 8] = b"HAKREU01";

pub const FORMAT_MAJOR: u16 = 1;
pub const FORMAT_MINOR: u16 = 0;
pub const SIGNATURE_OFFSET: usize = 152;
pub const SIGNATURE_LEN: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProjectId(pub [u8; 16]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(pub [u8; 32]);

impl fmt::Display for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    Raw = 0,
    Zstd = 1,
}

impl Codec {
    fn parse(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Zstd),
            _ => Err(Error::InvalidFormat("unknown codec")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Availability {
    Required = 0,
    Deferred = 1,
}

impl Availability {
    fn parse(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Required),
            1 => Ok(Self::Deferred),
            _ => Err(Error::InvalidFormat("unknown segment availability")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LayoutKind {
    Fixed = 0,
    ContentDefined = 1,
}

impl LayoutKind {
    fn parse(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Fixed),
            1 => Ok(Self::ContentDefined),
            _ => Err(Error::InvalidFormat("unknown file layout")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AccessClass {
    Hot = 0,
    Normal = 1,
    Streaming = 2,
    Transient = 3,
}

impl AccessClass {
    fn parse(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Hot),
            1 => Ok(Self::Normal),
            2 => Ok(Self::Streaming),
            3 => Ok(Self::Transient),
            _ => Err(Error::InvalidFormat("unknown access class")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PageKind {
    BlockMap = 1,
    Reuse = 2,
}

impl PageKind {
    fn parse(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::BlockMap),
            2 => Ok(Self::Reuse),
            _ => Err(Error::InvalidFormat("unknown page kind")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotHeader {
    pub project_id: ProjectId,
    pub release_sequence: u64,
    pub catalog_stored_len: u64,
    pub catalog_plain_len: u64,
    pub page_region_offset: u64,
    pub page_count: u32,
    pub snapshot_salt: [u8; 16],
    pub nonce_prefix: [u8; 8],
    pub signing_key_id: [u8; 16],
    pub source_fingerprint: [u8; 32],
    pub signature: [u8; SIGNATURE_LEN],
}

impl SnapshotHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, SNAPSHOT_HEADER_SIZE, "snapshot header")?;
        require_magic(bytes, SNAPSHOT_MAGIC, "snapshot magic")?;
        let major = read_u16(bytes, 8)?;
        let minor = read_u16(bytes, 10)?;
        if major != FORMAT_MAJOR || minor != FORMAT_MINOR {
            return Err(Error::UnsupportedVersion { major, minor });
        }
        if read_u32(bytes, 12)? as usize != SNAPSHOT_HEADER_SIZE {
            return Err(Error::InvalidFormat("snapshot header size"));
        }
        if read_u64(bytes, 40)? != SNAPSHOT_HEADER_SIZE as u64 {
            return Err(Error::InvalidFormat("catalog offset"));
        }
        let catalog_stored_len = read_u64(bytes, 48)?;
        let catalog_plain_len = read_u64(bytes, 56)?;
        if !(16..=MAX_CATALOG_STORED_LEN as u64).contains(&catalog_stored_len) {
            return Err(Error::LimitExceeded("catalog stored length"));
        }
        if catalog_plain_len == 0 || catalog_plain_len > MAX_CATALOG_PLAIN_LEN as u64 {
            return Err(Error::LimitExceeded("catalog plaintext length"));
        }
        let expected_page_offset = (SNAPSHOT_HEADER_SIZE as u64)
            .checked_add(catalog_stored_len)
            .ok_or(Error::InvalidFormat("snapshot length overflow"))?;
        let page_region_offset = read_u64(bytes, 64)?;
        if page_region_offset != expected_page_offset {
            return Err(Error::InvalidFormat("page region offset"));
        }
        let page_count = read_u32(bytes, 72)?;
        if page_count as usize > MAX_PAGES {
            return Err(Error::LimitExceeded("page count"));
        }
        require_zero(bytes, 76..80, "snapshot reserved fields")?;
        require_zero(
            bytes,
            SIGNATURE_OFFSET + SIGNATURE_LEN..SNAPSHOT_HEADER_SIZE,
            "snapshot reserved tail",
        )?;

        Ok(Self {
            project_id: ProjectId(read_array(bytes, 16)?),
            release_sequence: read_u64(bytes, 32)?,
            catalog_stored_len,
            catalog_plain_len,
            page_region_offset,
            page_count,
            snapshot_salt: read_array(bytes, 80)?,
            nonce_prefix: read_array(bytes, 96)?,
            signing_key_id: read_array(bytes, 104)?,
            source_fingerprint: read_array(bytes, 120)?,
            signature: read_array(bytes, SIGNATURE_OFFSET)?,
        })
    }

    #[must_use]
    pub fn encode(&self, zero_signature: bool) -> [u8; SNAPSHOT_HEADER_SIZE] {
        let mut out = [0_u8; SNAPSHOT_HEADER_SIZE];
        out[..8].copy_from_slice(SNAPSHOT_MAGIC);
        put_u16(&mut out, 8, FORMAT_MAJOR);
        put_u16(&mut out, 10, FORMAT_MINOR);
        put_u32(&mut out, 12, SNAPSHOT_HEADER_SIZE as u32);
        out[16..32].copy_from_slice(&self.project_id.0);
        put_u64(&mut out, 32, self.release_sequence);
        put_u64(&mut out, 40, SNAPSHOT_HEADER_SIZE as u64);
        put_u64(&mut out, 48, self.catalog_stored_len);
        put_u64(&mut out, 56, self.catalog_plain_len);
        put_u64(&mut out, 64, self.page_region_offset);
        put_u32(&mut out, 72, self.page_count);
        out[80..96].copy_from_slice(&self.snapshot_salt);
        out[96..104].copy_from_slice(&self.nonce_prefix);
        out[104..120].copy_from_slice(&self.signing_key_id);
        out[120..152].copy_from_slice(&self.source_fingerprint);
        if !zero_signature {
            out[SIGNATURE_OFFSET..SIGNATURE_OFFSET + SIGNATURE_LEN]
                .copy_from_slice(&self.signature);
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentHeader {
    pub project_id: ProjectId,
    pub segment_uid: [u8; 16],
    pub salt: [u8; 16],
    pub nonce_prefix: [u8; 8],
    pub block_count: u32,
    pub payload_len: u64,
    pub file_len: u64,
}

impl SegmentHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, SEGMENT_HEADER_SIZE, "segment header")?;
        require_magic(bytes, SEGMENT_MAGIC, "segment magic")?;
        let major = read_u16(bytes, 8)?;
        let minor = read_u16(bytes, 10)?;
        if major != FORMAT_MAJOR || minor != FORMAT_MINOR {
            return Err(Error::UnsupportedVersion { major, minor });
        }
        if read_u32(bytes, 12)? as usize != SEGMENT_HEADER_SIZE {
            return Err(Error::InvalidFormat("segment header size"));
        }
        require_zero(bytes, 76..80, "segment reserved fields")?;
        require_zero(bytes, 96..SEGMENT_HEADER_SIZE, "segment reserved tail")?;
        let payload_len = read_u64(bytes, 80)?;
        let file_len = read_u64(bytes, 88)?;
        if file_len != (SEGMENT_HEADER_SIZE as u64).saturating_add(payload_len) {
            return Err(Error::InvalidFormat("segment file length"));
        }
        Ok(Self {
            project_id: ProjectId(read_array(bytes, 16)?),
            segment_uid: read_array(bytes, 32)?,
            salt: read_array(bytes, 48)?,
            nonce_prefix: read_array(bytes, 64)?,
            block_count: read_u32(bytes, 72)?,
            payload_len,
            file_len,
        })
    }

    #[must_use]
    pub fn encode(&self) -> [u8; SEGMENT_HEADER_SIZE] {
        let mut out = [0_u8; SEGMENT_HEADER_SIZE];
        out[..8].copy_from_slice(SEGMENT_MAGIC);
        put_u16(&mut out, 8, FORMAT_MAJOR);
        put_u16(&mut out, 10, FORMAT_MINOR);
        put_u32(&mut out, 12, SEGMENT_HEADER_SIZE as u32);
        out[16..32].copy_from_slice(&self.project_id.0);
        out[32..48].copy_from_slice(&self.segment_uid);
        out[48..64].copy_from_slice(&self.salt);
        out[64..72].copy_from_slice(&self.nonce_prefix);
        put_u32(&mut out, 72, self.block_count);
        put_u64(&mut out, 80, self.payload_len);
        put_u64(&mut out, 88, self.file_len);
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentRecord {
    pub id: SegmentId,
    pub uid: [u8; 16],
    pub salt: [u8; 16],
    pub nonce_prefix: [u8; 8],
    pub file_len: u64,
    pub payload_len: u64,
    pub block_count: u32,
    pub availability: Availability,
}

impl SegmentRecord {
    fn parse(bytes: &[u8], offset: usize) -> Result<Self> {
        let record = checked_slice(bytes, offset, SEGMENT_RECORD_SIZE)?;
        require_zero(record, 93..96, "segment record reserved fields")?;
        Ok(Self {
            id: SegmentId(read_array(record, 0)?),
            uid: read_array(record, 32)?,
            salt: read_array(record, 48)?,
            nonce_prefix: read_array(record, 64)?,
            file_len: read_u64(record, 72)?,
            payload_len: read_u64(record, 80)?,
            block_count: read_u32(record, 88)?,
            availability: Availability::parse(record[92])?,
        })
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.resize(start + SEGMENT_RECORD_SIZE, 0);
        let record = &mut out[start..];
        record[..32].copy_from_slice(&self.id.0);
        record[32..48].copy_from_slice(&self.uid);
        record[48..64].copy_from_slice(&self.salt);
        record[64..72].copy_from_slice(&self.nonce_prefix);
        put_u64(record, 72, self.file_len);
        put_u64(record, 80, self.payload_len);
        put_u32(record, 88, self.block_count);
        record[92] = self.availability as u8;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileRecord {
    pub path_offset: u32,
    pub path_len: u16,
    pub layout: LayoutKind,
    pub access: AccessClass,
    pub logical_len: u64,
    pub first_block: u32,
    pub block_count: u32,
    pub fixed_block_len: u32,
}

impl FileRecord {
    fn parse(bytes: &[u8], offset: usize) -> Result<Self> {
        let record = checked_slice(bytes, offset, FILE_RECORD_SIZE)?;
        require_zero(record, 28..32, "file record reserved fields")?;
        let result = Self {
            path_offset: read_u32(record, 0)?,
            path_len: read_u16(record, 4)?,
            layout: LayoutKind::parse(record[6])?,
            access: AccessClass::parse(record[7])?,
            logical_len: read_u64(record, 8)?,
            first_block: read_u32(record, 16)?,
            block_count: read_u32(record, 20)?,
            fixed_block_len: read_u32(record, 24)?,
        };
        if result.layout == LayoutKind::Fixed
            && result.logical_len > 0
            && result.fixed_block_len == 0
        {
            return Err(Error::InvalidFormat("fixed layout has zero block length"));
        }
        if result.layout == LayoutKind::ContentDefined && result.fixed_block_len != 0 {
            return Err(Error::InvalidFormat(
                "content-defined layout has fixed length",
            ));
        }
        Ok(result)
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.resize(start + FILE_RECORD_SIZE, 0);
        let record = &mut out[start..];
        put_u32(record, 0, self.path_offset);
        put_u16(record, 4, self.path_len);
        record[6] = self.layout as u8;
        record[7] = self.access as u8;
        put_u64(record, 8, self.logical_len);
        put_u32(record, 16, self.first_block);
        put_u32(record, 20, self.block_count);
        put_u32(record, 24, self.fixed_block_len);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathSlot {
    pub hash: u64,
    pub file_index: u32,
}

impl PathSlot {
    fn parse(bytes: &[u8], offset: usize) -> Result<Self> {
        let record = checked_slice(bytes, offset, PATH_SLOT_SIZE)?;
        require_zero(record, 12..16, "path slot reserved fields")?;
        Ok(Self {
            hash: read_u64(record, 0)?,
            file_index: read_u32(record, 8)?,
        })
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.resize(start + PATH_SLOT_SIZE, 0);
        put_u64(&mut out[start..], 0, self.hash);
        put_u32(&mut out[start..], 8, self.file_index);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRecord {
    pub kind: PageKind,
    pub codec: Codec,
    pub nonce_ordinal: u32,
    pub relative_offset: u64,
    pub stored_len: u32,
    pub plain_len: u32,
    pub digest: [u8; 32],
}

impl PageRecord {
    fn parse(bytes: &[u8], offset: usize) -> Result<Self> {
        let record = checked_slice(bytes, offset, PAGE_RECORD_SIZE)?;
        require_zero(record, 2..4, "page record reserved fields")?;
        require_zero(record, 56..64, "page record reserved tail")?;
        let codec = Codec::parse(record[1])?;
        let stored_len = read_u32(record, 16)?;
        let plain_len = read_u32(record, 20)?;
        if stored_len < 16 {
            return Err(Error::InvalidFormat("page is shorter than its tag"));
        }
        if plain_len == 0 || plain_len as usize > MAX_PAGE_PLAIN_LEN {
            return Err(Error::LimitExceeded("page plaintext length"));
        }
        validate_encoded_length(codec, stored_len, plain_len, "page encoded length")?;
        Ok(Self {
            kind: PageKind::parse(record[0])?,
            codec,
            nonce_ordinal: read_u32(record, 4)?,
            relative_offset: read_u64(record, 8)?,
            stored_len,
            plain_len,
            digest: read_array(record, 24)?,
        })
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.resize(start + PAGE_RECORD_SIZE, 0);
        let record = &mut out[start..];
        record[0] = self.kind as u8;
        record[1] = self.codec as u8;
        put_u32(record, 4, self.nonce_ordinal);
        put_u64(record, 8, self.relative_offset);
        put_u32(record, 16, self.stored_len);
        put_u32(record, 20, self.plain_len);
        record[24..56].copy_from_slice(&self.digest);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub logical_offset: u64,
    pub segment_ordinal: u16,
    pub segment_block_ordinal: u32,
    pub physical_offset: u64,
    pub stored_len: u32,
    pub plain_len: u32,
    pub codec: Codec,
    pub cipher_digest: [u8; 16],
}

impl BlockRef {
    pub fn parse(bytes: &[u8], offset: usize) -> Result<Self> {
        let record = checked_slice(bytes, offset, BLOCK_REF_SIZE)?;
        if record[31] != 0 {
            return Err(Error::InvalidFormat("block reference reserved field"));
        }
        let codec = Codec::parse(record[30])?;
        let stored_len = read_u32(record, 22)?;
        let plain_len = read_u32(record, 26)?;
        if plain_len == 0 || plain_len as usize > MAX_BLOCK_PLAIN_LEN {
            return Err(Error::LimitExceeded("block plaintext length"));
        }
        validate_encoded_length(codec, stored_len, plain_len, "block encoded length")?;
        Ok(Self {
            logical_offset: read_u64(record, 0)?,
            segment_ordinal: read_u16(record, 8)?,
            segment_block_ordinal: read_u32(record, 10)?,
            physical_offset: read_u64(record, 14)?,
            stored_len,
            plain_len,
            codec,
            cipher_digest: read_array(record, 32)?,
        })
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.resize(start + BLOCK_REF_SIZE, 0);
        let record = &mut out[start..];
        put_u64(record, 0, self.logical_offset);
        put_u16(record, 8, self.segment_ordinal);
        put_u32(record, 10, self.segment_block_ordinal);
        put_u64(record, 14, self.physical_offset);
        put_u32(record, 22, self.stored_len);
        put_u32(record, 26, self.plain_len);
        record[30] = self.codec as u8;
        record[32..48].copy_from_slice(&self.cipher_digest);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReuseRecord {
    pub chunk_id: [u8; 32],
    pub block: BlockRef,
}

impl ReuseRecord {
    pub fn parse(bytes: &[u8], offset: usize) -> Result<Self> {
        let record = checked_slice(bytes, offset, REUSE_RECORD_SIZE)?;
        Ok(Self {
            chunk_id: read_array(record, 0)?,
            block: BlockRef::parse(record, 32)?,
        })
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.chunk_id);
        self.block.encode_into(out);
    }
}

#[derive(Clone, Debug)]
pub struct CatalogData {
    pub segments: Vec<SegmentRecord>,
    pub files: Vec<FileRecord>,
    pub path_slots: Vec<PathSlot>,
    pub path_pool: Vec<u8>,
    pub pages: Vec<PageRecord>,
    pub total_blocks: u32,
    pub map_page_count: u32,
    pub reuse_page_count: u32,
}

impl CatalogData {
    pub fn encode(&self) -> Result<Vec<u8>> {
        validate_count(self.segments.len(), MAX_SEGMENTS, "segment count")?;
        validate_count(self.files.len(), MAX_FILES, "file count")?;
        validate_count(self.total_blocks as usize, MAX_BLOCKS, "block count")?;
        validate_count(self.pages.len(), MAX_PAGES, "page count")?;
        validate_count(self.path_pool.len(), MAX_PATH_POOL_LEN, "path pool")?;
        if self.path_slots.is_empty() || !self.path_slots.len().is_power_of_two() {
            return Err(Error::InvalidFormat(
                "path slot count is not a power of two",
            ));
        }
        if self.map_page_count.checked_add(self.reuse_page_count) != Some(self.pages.len() as u32) {
            return Err(Error::InvalidFormat("page kind counts"));
        }

        let segment_offset = CATALOG_HEADER_SIZE;
        let file_offset =
            checked_table_end(segment_offset, self.segments.len(), SEGMENT_RECORD_SIZE)?;
        let path_slot_offset = checked_table_end(file_offset, self.files.len(), FILE_RECORD_SIZE)?;
        let path_pool_offset =
            checked_table_end(path_slot_offset, self.path_slots.len(), PATH_SLOT_SIZE)?;
        let page_offset = path_pool_offset
            .checked_add(self.path_pool.len())
            .ok_or(Error::InvalidFormat("catalog size overflow"))?;
        let total_len = checked_table_end(page_offset, self.pages.len(), PAGE_RECORD_SIZE)?;
        if total_len > MAX_CATALOG_PLAIN_LEN {
            return Err(Error::LimitExceeded("catalog plaintext length"));
        }

        let mut out = vec![0_u8; CATALOG_HEADER_SIZE];
        out[..8].copy_from_slice(CATALOG_MAGIC);
        put_u16(&mut out, 8, FORMAT_MAJOR);
        put_u16(&mut out, 10, CATALOG_HEADER_SIZE as u16);
        put_u32(&mut out, 12, usize_to_u32(self.segments.len())?);
        put_u32(&mut out, 16, usize_to_u32(self.files.len())?);
        put_u32(&mut out, 20, self.total_blocks);
        put_u32(&mut out, 24, usize_to_u32(self.path_pool.len())?);
        put_u32(&mut out, 28, usize_to_u32(self.path_slots.len())?);
        put_u32(&mut out, 32, usize_to_u32(self.pages.len())?);
        put_u32(&mut out, 36, self.map_page_count);
        put_u32(&mut out, 40, self.reuse_page_count);
        put_u32(&mut out, 44, usize_to_u32(segment_offset)?);
        put_u32(&mut out, 48, usize_to_u32(file_offset)?);
        put_u32(&mut out, 52, usize_to_u32(path_slot_offset)?);
        put_u32(&mut out, 56, usize_to_u32(path_pool_offset)?);
        put_u32(&mut out, 60, usize_to_u32(page_offset)?);
        for record in &self.segments {
            record.encode_into(&mut out);
        }
        for record in &self.files {
            record.encode_into(&mut out);
        }
        for slot in &self.path_slots {
            slot.encode_into(&mut out);
        }
        out.extend_from_slice(&self.path_pool);
        for page in &self.pages {
            page.encode_into(&mut out);
        }
        debug_assert_eq!(out.len(), total_len);
        Ok(out)
    }
}

#[derive(Clone, Debug)]
pub struct Catalog {
    bytes: Arc<[u8]>,
    segment_count: u32,
    file_count: u32,
    total_blocks: u32,
    path_pool_len: u32,
    path_slot_count: u32,
    page_count: u32,
    map_page_count: u32,
    reuse_page_count: u32,
    segment_offset: usize,
    file_offset: usize,
    path_slot_offset: usize,
    path_pool_offset: usize,
    page_offset: usize,
}

impl Catalog {
    pub fn parse(bytes: Arc<[u8]>) -> Result<Self> {
        require_len(&bytes, CATALOG_HEADER_SIZE, "catalog header")?;
        require_magic(&bytes, CATALOG_MAGIC, "catalog magic")?;
        if read_u16(&bytes, 8)? != FORMAT_MAJOR
            || read_u16(&bytes, 10)? as usize != CATALOG_HEADER_SIZE
        {
            return Err(Error::InvalidFormat("catalog version or header size"));
        }
        let segment_count = read_u32(&bytes, 12)?;
        let file_count = read_u32(&bytes, 16)?;
        let total_blocks = read_u32(&bytes, 20)?;
        let path_pool_len = read_u32(&bytes, 24)?;
        let path_slot_count = read_u32(&bytes, 28)?;
        let page_count = read_u32(&bytes, 32)?;
        let map_page_count = read_u32(&bytes, 36)?;
        let reuse_page_count = read_u32(&bytes, 40)?;
        validate_count(segment_count as usize, MAX_SEGMENTS, "segment count")?;
        validate_count(file_count as usize, MAX_FILES, "file count")?;
        validate_count(total_blocks as usize, MAX_BLOCKS, "block count")?;
        validate_count(page_count as usize, MAX_PAGES, "page count")?;
        validate_count(path_pool_len as usize, MAX_PATH_POOL_LEN, "path pool")?;
        if path_slot_count == 0 || !path_slot_count.is_power_of_two() {
            return Err(Error::InvalidFormat(
                "path slot count is not a power of two",
            ));
        }
        if map_page_count.checked_add(reuse_page_count) != Some(page_count) {
            return Err(Error::InvalidFormat("page kind counts"));
        }

        let segment_offset = read_u32(&bytes, 44)? as usize;
        let file_offset = read_u32(&bytes, 48)? as usize;
        let path_slot_offset = read_u32(&bytes, 52)? as usize;
        let path_pool_offset = read_u32(&bytes, 56)? as usize;
        let page_offset = read_u32(&bytes, 60)? as usize;
        let expected_file = checked_table_end(
            CATALOG_HEADER_SIZE,
            segment_count as usize,
            SEGMENT_RECORD_SIZE,
        )?;
        let expected_slots =
            checked_table_end(expected_file, file_count as usize, FILE_RECORD_SIZE)?;
        let expected_pool =
            checked_table_end(expected_slots, path_slot_count as usize, PATH_SLOT_SIZE)?;
        let expected_pages = expected_pool
            .checked_add(path_pool_len as usize)
            .ok_or(Error::InvalidFormat("catalog size overflow"))?;
        let expected_len =
            checked_table_end(expected_pages, page_count as usize, PAGE_RECORD_SIZE)?;
        if (
            segment_offset,
            file_offset,
            path_slot_offset,
            path_pool_offset,
            page_offset,
        ) != (
            CATALOG_HEADER_SIZE,
            expected_file,
            expected_slots,
            expected_pool,
            expected_pages,
        ) || expected_len != bytes.len()
        {
            return Err(Error::InvalidFormat("non-canonical catalog layout"));
        }

        let result = Self {
            bytes,
            segment_count,
            file_count,
            total_blocks,
            path_pool_len,
            path_slot_count,
            page_count,
            map_page_count,
            reuse_page_count,
            segment_offset,
            file_offset,
            path_slot_offset,
            path_pool_offset,
            page_offset,
        };
        result.validate_records()?;
        Ok(result)
    }

    fn validate_records(&self) -> Result<()> {
        let expected_map_pages = self.total_blocks.div_ceil(BLOCKS_PER_MAP_PAGE as u32);
        let expected_reuse_pages = self.total_blocks.div_ceil(REUSE_PER_PAGE as u32);
        if self.map_page_count != expected_map_pages
            || self.reuse_page_count != expected_reuse_pages
        {
            return Err(Error::InvalidFormat(
                "page count does not cover block records",
            ));
        }
        let mut expected_page_offset = 0_u64;
        for index in 0..self.segment_count {
            let segment = self.segment(index)?;
            if segment.file_len != (SEGMENT_HEADER_SIZE as u64).saturating_add(segment.payload_len)
            {
                return Err(Error::InvalidFormat("segment record length"));
            }
        }
        for index in 0..self.file_count {
            let file = self.file(index)?;
            let path_end = (file.path_offset as usize)
                .checked_add(file.path_len as usize)
                .ok_or(Error::InvalidFormat("path range overflow"))?;
            if file.path_len == 0 || path_end > self.path_pool_len as usize {
                return Err(Error::InvalidFormat("file path range"));
            }
            let block_end = file
                .first_block
                .checked_add(file.block_count)
                .ok_or(Error::InvalidFormat("file block range overflow"))?;
            if block_end > self.total_blocks {
                return Err(Error::InvalidFormat("file block range"));
            }
            validate_canonical_path(self.path(index)?)?;
        }
        for index in 0..self.path_slot_count {
            let slot = self.path_slot(index)?;
            if slot.file_index != EMPTY_PATH_SLOT && slot.file_index >= self.file_count {
                return Err(Error::InvalidFormat("path slot file index"));
            }
        }
        for index in 0..self.page_count {
            let page = self.page(index)?;
            let expected_kind = if index < self.map_page_count {
                PageKind::BlockMap
            } else {
                PageKind::Reuse
            };
            if page.kind != expected_kind || page.nonce_ordinal != index + 1 {
                return Err(Error::InvalidFormat("non-canonical page ordering"));
            }
            let (capacity, record_size, local_page, total_records) =
                if page.kind == PageKind::BlockMap {
                    (
                        BLOCKS_PER_MAP_PAGE as u32,
                        BLOCK_REF_SIZE,
                        index,
                        self.total_blocks,
                    )
                } else {
                    (
                        REUSE_PER_PAGE as u32,
                        REUSE_RECORD_SIZE,
                        index - self.map_page_count,
                        self.total_blocks,
                    )
                };
            let first = local_page
                .checked_mul(capacity)
                .ok_or(Error::InvalidFormat("page record range overflow"))?;
            let record_count = total_records.saturating_sub(first).min(capacity) as usize;
            let expected_plain_len = PAGE_HEADER_SIZE
                .checked_add(
                    record_count
                        .checked_mul(record_size)
                        .ok_or(Error::InvalidFormat("page plaintext length overflow"))?,
                )
                .ok_or(Error::InvalidFormat("page plaintext length overflow"))?;
            if page.plain_len as usize != expected_plain_len {
                return Err(Error::InvalidFormat(
                    "page plaintext length is non-canonical",
                ));
            }
            if page.relative_offset != expected_page_offset {
                return Err(Error::InvalidFormat("non-contiguous page region"));
            }
            expected_page_offset = expected_page_offset
                .checked_add(u64::from(page.stored_len))
                .ok_or(Error::InvalidFormat("page region overflow"))?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn segment_count(&self) -> u32 {
        self.segment_count
    }

    #[must_use]
    pub const fn file_count(&self) -> u32 {
        self.file_count
    }

    #[must_use]
    pub const fn total_blocks(&self) -> u32 {
        self.total_blocks
    }

    #[must_use]
    pub const fn map_page_count(&self) -> u32 {
        self.map_page_count
    }

    #[must_use]
    pub const fn reuse_page_count(&self) -> u32 {
        self.reuse_page_count
    }

    pub fn segment(&self, index: u32) -> Result<SegmentRecord> {
        if index >= self.segment_count {
            return Err(Error::InvalidFormat("segment index"));
        }
        SegmentRecord::parse(
            &self.bytes,
            table_offset(self.segment_offset, index, SEGMENT_RECORD_SIZE)?,
        )
    }

    pub fn file(&self, index: u32) -> Result<FileRecord> {
        if index >= self.file_count {
            return Err(Error::InvalidFormat("file index"));
        }
        FileRecord::parse(
            &self.bytes,
            table_offset(self.file_offset, index, FILE_RECORD_SIZE)?,
        )
    }

    pub fn path_slot(&self, index: u32) -> Result<PathSlot> {
        if index >= self.path_slot_count {
            return Err(Error::InvalidFormat("path slot index"));
        }
        PathSlot::parse(
            &self.bytes,
            table_offset(self.path_slot_offset, index, PATH_SLOT_SIZE)?,
        )
    }

    pub fn page(&self, index: u32) -> Result<PageRecord> {
        if index >= self.page_count {
            return Err(Error::InvalidFormat("page index"));
        }
        PageRecord::parse(
            &self.bytes,
            table_offset(self.page_offset, index, PAGE_RECORD_SIZE)?,
        )
    }

    pub fn path(&self, file_index: u32) -> Result<&str> {
        let file = self.file(file_index)?;
        let start = self
            .path_pool_offset
            .checked_add(file.path_offset as usize)
            .ok_or(Error::InvalidFormat("path offset overflow"))?;
        let bytes = checked_slice(&self.bytes, start, file.path_len as usize)?;
        std::str::from_utf8(bytes).map_err(|_| Error::InvalidFormat("path is not UTF-8"))
    }

    pub fn find_file(&self, path: &str, path_key: &[u8; 32]) -> Result<Option<u32>> {
        validate_canonical_path(path)?;
        let digest = blake3::keyed_hash(path_key, path.as_bytes());
        let mut hash_bytes = [0_u8; 8];
        hash_bytes.copy_from_slice(&digest.as_bytes()[..8]);
        let hash = u64::from_le_bytes(hash_bytes);
        let mask = self.path_slot_count - 1;
        for probe in 0..self.path_slot_count {
            let slot = self.path_slot(hash.wrapping_add(u64::from(probe)) as u32 & mask)?;
            if slot.file_index == EMPTY_PATH_SLOT {
                return Ok(None);
            }
            if slot.hash == hash && self.path(slot.file_index)? == path {
                return Ok(Some(slot.file_index));
            }
        }
        Err(Error::InvalidFormat("path index has no empty slot"))
    }
}

pub fn encode_map_page(first_record: u32, records: &[BlockRef]) -> Result<Vec<u8>> {
    if records.is_empty() || records.len() > BLOCKS_PER_MAP_PAGE {
        return Err(Error::InvalidFormat("map page record count"));
    }
    let mut out = vec![0_u8; PAGE_HEADER_SIZE];
    out[..8].copy_from_slice(MAP_PAGE_MAGIC);
    put_u16(&mut out, 8, FORMAT_MAJOR);
    put_u16(&mut out, 10, BLOCK_REF_SIZE as u16);
    put_u32(&mut out, 12, first_record);
    for record in records {
        record.encode_into(&mut out);
    }
    Ok(out)
}

pub fn parse_map_page(bytes: &[u8], expected_first: u32) -> Result<Vec<BlockRef>> {
    let count = validate_map_page(bytes, expected_first)?;
    let mut records = Vec::with_capacity(count);
    for index in 0..count {
        records.push(BlockRef::parse(
            bytes,
            PAGE_HEADER_SIZE + index * BLOCK_REF_SIZE,
        )?);
    }
    Ok(records)
}

pub fn validate_map_page(bytes: &[u8], expected_first: u32) -> Result<usize> {
    parse_page_header(bytes, MAP_PAGE_MAGIC, BLOCK_REF_SIZE, expected_first)
}

pub fn map_page_record(bytes: &[u8], index: usize) -> Result<BlockRef> {
    BlockRef::parse(
        bytes,
        PAGE_HEADER_SIZE
            .checked_add(
                index
                    .checked_mul(BLOCK_REF_SIZE)
                    .ok_or(Error::InvalidFormat("map page index overflow"))?,
            )
            .ok_or(Error::InvalidFormat("map page offset overflow"))?,
    )
}

pub fn encode_reuse_page(first_record: u32, records: &[ReuseRecord]) -> Result<Vec<u8>> {
    if records.is_empty() || records.len() > REUSE_PER_PAGE {
        return Err(Error::InvalidFormat("reuse page record count"));
    }
    let mut out = vec![0_u8; PAGE_HEADER_SIZE];
    out[..8].copy_from_slice(REUSE_PAGE_MAGIC);
    put_u16(&mut out, 8, FORMAT_MAJOR);
    put_u16(&mut out, 10, REUSE_RECORD_SIZE as u16);
    put_u32(&mut out, 12, first_record);
    for record in records {
        record.encode_into(&mut out);
    }
    Ok(out)
}

pub fn parse_reuse_page(bytes: &[u8], expected_first: u32) -> Result<Vec<ReuseRecord>> {
    let count = parse_page_header(bytes, REUSE_PAGE_MAGIC, REUSE_RECORD_SIZE, expected_first)?;
    let mut records = Vec::with_capacity(count);
    for index in 0..count {
        records.push(ReuseRecord::parse(
            bytes,
            PAGE_HEADER_SIZE + index * REUSE_RECORD_SIZE,
        )?);
    }
    Ok(records)
}

pub fn validate_canonical_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(Error::InvalidPath);
    }
    Ok(())
}

fn parse_page_header(
    bytes: &[u8],
    magic: &[u8; 8],
    record_size: usize,
    expected_first: u32,
) -> Result<usize> {
    require_len(bytes, PAGE_HEADER_SIZE, "page header")?;
    require_magic(bytes, magic, "page magic")?;
    if read_u16(bytes, 8)? != FORMAT_MAJOR || read_u16(bytes, 10)? as usize != record_size {
        return Err(Error::InvalidFormat("page version or record size"));
    }
    if read_u32(bytes, 12)? != expected_first {
        return Err(Error::InvalidFormat("page first record"));
    }
    let payload = bytes.len() - PAGE_HEADER_SIZE;
    if payload == 0 || !payload.is_multiple_of(record_size) {
        return Err(Error::InvalidFormat("page payload length"));
    }
    Ok(payload / record_size)
}

fn checked_table_end(start: usize, count: usize, record_size: usize) -> Result<usize> {
    start
        .checked_add(
            count
                .checked_mul(record_size)
                .ok_or(Error::InvalidFormat("table size overflow"))?,
        )
        .ok_or(Error::InvalidFormat("table end overflow"))
}

fn table_offset(start: usize, index: u32, record_size: usize) -> Result<usize> {
    checked_table_end(start, index as usize, record_size)
}

fn validate_encoded_length(
    codec: Codec,
    stored_len: u32,
    plain_len: u32,
    name: &'static str,
) -> Result<()> {
    let raw_len = plain_len
        .checked_add(16)
        .ok_or(Error::InvalidFormat(name))?;
    let canonical = match codec {
        Codec::Raw => stored_len == raw_len,
        Codec::Zstd => stored_len >= 16 && stored_len < raw_len,
    };
    if canonical {
        Ok(())
    } else {
        Err(Error::InvalidFormat(name))
    }
}

fn validate_count(value: usize, maximum: usize, name: &'static str) -> Result<()> {
    if value > maximum {
        Err(Error::LimitExceeded(name))
    } else {
        Ok(())
    }
}

fn usize_to_u32(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::InvalidFormat("value does not fit u32"))
}

fn require_len(bytes: &[u8], minimum: usize, name: &'static str) -> Result<()> {
    if bytes.len() < minimum {
        Err(Error::InvalidFormat(name))
    } else {
        Ok(())
    }
}

fn require_magic(bytes: &[u8], magic: &[u8; 8], name: &'static str) -> Result<()> {
    if bytes.get(..8) == Some(magic.as_slice()) {
        Ok(())
    } else {
        Err(Error::InvalidFormat(name))
    }
}

fn require_zero(bytes: &[u8], range: std::ops::Range<usize>, name: &'static str) -> Result<()> {
    let value = bytes.get(range).ok_or(Error::InvalidFormat(name))?;
    if value.iter().all(|byte| *byte == 0) {
        Ok(())
    } else {
        Err(Error::InvalidFormat(name))
    }
}

fn checked_slice(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    let end = offset
        .checked_add(len)
        .ok_or(Error::InvalidFormat("slice range overflow"))?;
    bytes
        .get(offset..end)
        .ok_or(Error::InvalidFormat("slice range is out of bounds"))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
    checked_slice(bytes, offset, N)?
        .try_into()
        .map_err(|_| Error::InvalidFormat("fixed array"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(read_array(bytes, offset)?))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_ref_wire_size_is_exact() {
        let record = BlockRef {
            logical_offset: 7,
            segment_ordinal: 2,
            segment_block_ordinal: 3,
            physical_offset: 4096,
            stored_len: 32,
            plain_len: 16,
            codec: Codec::Raw,
            cipher_digest: [4; 16],
        };
        let mut bytes = Vec::new();
        record.encode_into(&mut bytes);
        assert_eq!(bytes.len(), BLOCK_REF_SIZE);
        assert_eq!(BlockRef::parse(&bytes, 0).unwrap(), record);
    }

    #[test]
    fn paths_are_strictly_canonical() {
        assert!(validate_canonical_path("video/opening.mp4").is_ok());
        for invalid in ["", "/a", "a/", "a//b", "a/../b", "a\\b"] {
            assert!(validate_canonical_path(invalid).is_err(), "{invalid}");
        }
    }
}
