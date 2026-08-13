//! Normative Hakutaku v1 wire records.
//!
//! Every record is encoded explicitly. Rust struct layout and serde are never
//! part of the format contract.

use crate::{Error, Result};
use std::fmt;
use std::sync::Arc;

/// Encoded byte length of the fixed snapshot header.
pub const SNAPSHOT_HEADER_SIZE: usize = 4096;
/// Encoded byte length of the fixed segment header.
pub const SEGMENT_HEADER_SIZE: usize = 4096;
/// Encoded byte length of the catalog header.
pub const CATALOG_HEADER_SIZE: usize = 64;
/// Encoded byte length of one segment record.
pub const SEGMENT_RECORD_SIZE: usize = 96;
/// Encoded byte length of one file record.
pub const FILE_RECORD_SIZE: usize = 32;
/// Encoded byte length of one path-index slot.
pub const PATH_SLOT_SIZE: usize = 16;
/// Encoded byte length of one snapshot page descriptor.
pub const PAGE_RECORD_SIZE: usize = 64;
/// Encoded byte length of a map or reuse page header.
pub const PAGE_HEADER_SIZE: usize = 16;
/// Encoded byte length of one block reference.
pub const BLOCK_REF_SIZE: usize = 48;
/// Encoded byte length of one reusable-chunk record.
pub const REUSE_RECORD_SIZE: usize = 80;
/// Maximum block references stored in one 16-KiB map page.
pub const BLOCKS_PER_MAP_PAGE: usize = (16 * 1024 - PAGE_HEADER_SIZE) / BLOCK_REF_SIZE;
/// Maximum reusable-chunk records stored in one 16-KiB page.
pub const REUSE_PER_PAGE: usize = (16 * 1024 - PAGE_HEADER_SIZE) / REUSE_RECORD_SIZE;

/// Maximum decompressed catalog size accepted by the runtime.
pub const MAX_CATALOG_PLAIN_LEN: usize = 64 * 1024 * 1024;
/// Maximum encrypted catalog size accepted by the runtime.
pub const MAX_CATALOG_STORED_LEN: usize = MAX_CATALOG_PLAIN_LEN + 1024 * 1024 + 16;
/// Maximum decompressed metadata-page size.
pub const MAX_PAGE_PLAIN_LEN: usize = 1024 * 1024;
/// Maximum plaintext block size.
pub const MAX_BLOCK_PLAIN_LEN: usize = 1024 * 1024;
/// Maximum segments referenced by one snapshot.
pub const MAX_SEGMENTS: usize = 128;
/// Maximum files indexed by one snapshot.
pub const MAX_FILES: usize = 1_000_000;
/// Maximum content blocks indexed by one snapshot.
pub const MAX_BLOCKS: usize = 10_000_000;
/// Maximum metadata pages referenced by one snapshot.
pub const MAX_PAGES: usize = 100_000;
/// Maximum canonical path-pool size.
pub const MAX_PATH_POOL_LEN: usize = 32 * 1024 * 1024;
/// Sentinel used by an unoccupied path-index slot.
pub const EMPTY_PATH_SLOT: u32 = u32::MAX;

const SNAPSHOT_MAGIC: &[u8; 8] = b"HAKU0001";
const CATALOG_MAGIC: &[u8; 8] = b"HAKCAT01";
const SEGMENT_MAGIC: &[u8; 8] = b"HAKSEG01";
const MAP_PAGE_MAGIC: &[u8; 8] = b"HAKMAP01";
const REUSE_PAGE_MAGIC: &[u8; 8] = b"HAKREU01";

/// Current incompatible format generation.
pub const FORMAT_MAJOR: u16 = 1;
/// Current backwards-compatible format revision.
pub const FORMAT_MINOR: u16 = 0;
/// Byte offset of the snapshot's Ed25519 signature.
pub const SIGNATURE_OFFSET: usize = 152;
/// Encoded Ed25519 signature length.
pub const SIGNATURE_LEN: usize = 64;

/// Stable 128-bit identity shared by every release of one project.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProjectId(
    /// Raw project identifier bytes.
    pub [u8; 16],
);

/// Content-derived identity of one immutable segment file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(
    /// Raw BLAKE3 segment digest.
    pub [u8; 32],
);

impl fmt::Display for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Canonical encoding used for a metadata page or content block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    /// Plain bytes followed only by the authentication tag.
    Raw = 0,
    /// Zstandard-compressed bytes followed by the authentication tag.
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

/// Installation policy for an immutable segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Availability {
    /// Must be installed before the package is opened.
    Required = 0,
    /// May be fetched on demand after the snapshot is installed.
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

/// Block-boundary strategy used by an asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LayoutKind {
    /// Fixed-size blocks permit direct arithmetic lookup.
    Fixed = 0,
    /// Content-defined blocks require map lookup by logical offset.
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

/// Runtime caching and access hint attached to an asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AccessClass {
    /// Small launch-critical content retained aggressively.
    Hot = 0,
    /// Ordinary content admitted after repeated access.
    Normal = 1,
    /// Sequential media content that bypasses plaintext caching.
    Streaming = 2,
    /// One-shot content that bypasses plaintext caching.
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

/// Logical record type stored in an encrypted metadata page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PageKind {
    /// Maps asset logical ranges to encrypted segment blocks.
    BlockMap = 1,
    /// Maps content hashes to blocks reusable by future releases.
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

/// Fixed, signed metadata preceding a `game.haku` snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// Project to which this snapshot belongs.
    pub project_id: ProjectId,
    /// Monotonically increasing publisher release number.
    pub release_sequence: u64,
    /// Encrypted catalog length, including its AEAD tag.
    pub catalog_stored_len: u64,
    /// Exact catalog length after decompression.
    pub catalog_plain_len: u64,
    /// Absolute snapshot offset of the encrypted page region.
    pub page_region_offset: u64,
    /// Number of encrypted metadata pages.
    pub page_count: u32,
    /// Per-release salt used for snapshot-key derivation.
    pub snapshot_salt: [u8; 16],
    /// Per-release nonce prefix; page ordinals supply the remaining bytes.
    pub nonce_prefix: [u8; 8],
    /// Identifier of the publisher verification key.
    pub signing_key_id: [u8; 16],
    /// Digest of the canonical input tree used to detect unchanged builds.
    pub source_fingerprint: [u8; 32],
    /// Ed25519 signature over the header and encrypted catalog.
    pub signature: [u8; SIGNATURE_LEN],
}

impl SnapshotHeader {
    /// Parses and validates a fixed-size snapshot header.
    ///
    /// # Errors
    ///
    /// Returns an error for truncated, unsupported, non-canonical, or excessive input.
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
    /// Encodes the header, optionally replacing its signature with zeroes for signing.
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

/// Fixed metadata preceding one immutable `.taku` segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentHeader {
    /// Project to which the segment belongs.
    pub project_id: ProjectId,
    /// Random segment identity used by key derivation.
    pub segment_uid: [u8; 16],
    /// Random per-segment key-derivation salt.
    pub salt: [u8; 16],
    /// Per-segment nonce prefix.
    pub nonce_prefix: [u8; 8],
    /// Number of encrypted blocks in the payload.
    pub block_count: u32,
    /// Bytes following the fixed header.
    pub payload_len: u64,
    /// Complete segment-file length.
    pub file_len: u64,
}

impl SegmentHeader {
    /// Parses and validates a fixed-size segment header.
    ///
    /// # Errors
    ///
    /// Returns an error for truncated, unsupported, or non-canonical input.
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
    /// Encodes the segment header in its canonical wire representation.
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

/// Catalog inventory entry for one immutable segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentRecord {
    /// Content-derived segment identifier and filename stem.
    pub id: SegmentId,
    /// Key-derivation identity copied from the segment header.
    pub uid: [u8; 16],
    /// Key-derivation salt copied from the segment header.
    pub salt: [u8; 16],
    /// Nonce prefix copied from the segment header.
    pub nonce_prefix: [u8; 8],
    /// Complete segment-file length.
    pub file_len: u64,
    /// Bytes following the segment header.
    pub payload_len: u64,
    /// Number of encrypted content blocks.
    pub block_count: u32,
    /// Installation policy for this segment.
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

    /// Appends this record's canonical wire representation to `out`.
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

/// Catalog metadata for one canonical asset path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileRecord {
    /// Byte offset into the catalog path pool.
    pub path_offset: u32,
    /// UTF-8 path length in bytes.
    pub path_len: u16,
    /// Block-boundary strategy.
    pub layout: LayoutKind,
    /// Runtime cache and access hint.
    pub access: AccessClass,
    /// Complete plaintext asset length.
    pub logical_len: u64,
    /// Global index of the first block reference.
    pub first_block: u32,
    /// Number of block references owned by the asset.
    pub block_count: u32,
    /// Fixed block size, or zero for content-defined layouts.
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

    /// Appends this record's canonical wire representation to `out`.
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

/// One open-addressed lookup slot in the keyed path index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathSlot {
    /// Keyed 64-bit digest of a canonical path.
    pub hash: u64,
    /// Catalog file index, or [`EMPTY_PATH_SLOT`].
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

    /// Appends this slot's canonical wire representation to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.resize(start + PATH_SLOT_SIZE, 0);
        put_u64(&mut out[start..], 0, self.hash);
        put_u32(&mut out[start..], 8, self.file_index);
    }
}

/// Catalog descriptor for one encrypted metadata page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRecord {
    /// Record kind held by the page.
    pub kind: PageKind,
    /// Canonical plaintext encoding.
    pub codec: Codec,
    /// Unique snapshot-key nonce ordinal.
    pub nonce_ordinal: u32,
    /// Offset relative to the snapshot page-region start.
    pub relative_offset: u64,
    /// Encrypted page length, including its authentication tag.
    pub stored_len: u32,
    /// Exact page length after decompression.
    pub plain_len: u32,
    /// BLAKE3 digest of the encrypted page bytes.
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

    /// Appends this descriptor's canonical wire representation to `out`.
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

/// Random-access mapping from one asset range to an encrypted segment block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRef {
    /// Plaintext byte offset within the owning asset.
    pub logical_offset: u64,
    /// Segment index within the catalog.
    pub segment_ordinal: u16,
    /// Unique block nonce ordinal within that segment.
    pub segment_block_ordinal: u32,
    /// Absolute byte offset within the segment file.
    pub physical_offset: u64,
    /// Encrypted block length, including its authentication tag.
    pub stored_len: u32,
    /// Exact block length after decompression.
    pub plain_len: u32,
    /// Canonical plaintext encoding.
    pub codec: Codec,
    /// Truncated BLAKE3 digest of the encrypted block.
    pub cipher_digest: [u8; 16],
}

impl BlockRef {
    /// Parses one block reference at `offset`.
    ///
    /// # Errors
    ///
    /// Returns an error for truncated or non-canonical input.
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

    /// Appends this reference's canonical wire representation to `out`.
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

/// Content hash paired with the block that can satisfy it in a later release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReuseRecord {
    /// BLAKE3 digest of the plaintext chunk.
    pub chunk_id: [u8; 32],
    /// Existing encrypted block containing that chunk.
    pub block: BlockRef,
}

impl ReuseRecord {
    /// Parses one reuse record at `offset`.
    ///
    /// # Errors
    ///
    /// Returns an error for truncated or non-canonical input.
    pub fn parse(bytes: &[u8], offset: usize) -> Result<Self> {
        let record = checked_slice(bytes, offset, REUSE_RECORD_SIZE)?;
        Ok(Self {
            chunk_id: read_array(record, 0)?,
            block: BlockRef::parse(record, 32)?,
        })
    }

    /// Appends this record's canonical wire representation to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.chunk_id);
        self.block.encode_into(out);
    }
}

/// Publisher-side, owned representation of a complete catalog.
#[derive(Clone, Debug)]
pub struct CatalogData {
    /// Immutable segment inventory.
    pub segments: Vec<SegmentRecord>,
    /// Asset inventory.
    pub files: Vec<FileRecord>,
    /// Keyed open-addressed path index.
    pub path_slots: Vec<PathSlot>,
    /// Concatenated canonical UTF-8 paths.
    pub path_pool: Vec<u8>,
    /// Encrypted metadata-page descriptors.
    pub pages: Vec<PageRecord>,
    /// Total block references across all assets.
    pub total_blocks: u32,
    /// Number of leading block-map pages.
    pub map_page_count: u32,
    /// Number of trailing reusable-chunk pages.
    pub reuse_page_count: u32,
}

impl CatalogData {
    /// Validates and encodes this catalog into its canonical plaintext form.
    ///
    /// # Errors
    ///
    /// Returns an error when counts, ranges, or layout invariants are invalid.
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
        validate_catalog_len(total_len)?;

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

/// Borrow-free runtime view over a validated encoded catalog.
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
    /// Parses an encoded catalog and validates every cross-record invariant.
    ///
    /// # Errors
    ///
    /// Returns an error for truncated, excessive, or non-canonical data.
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
        // Segment and file counts have explicit maxima, while the slot count
        // is only required to be a power of two. Keep its multiplication
        // checked so a hostile header cannot wrap usize on 32-bit targets.
        let expected_file = CATALOG_HEADER_SIZE + segment_count as usize * SEGMENT_RECORD_SIZE;
        let expected_slots = expected_file + file_count as usize * FILE_RECORD_SIZE;
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
    /// Returns the number of referenced immutable segments.
    pub const fn segment_count(&self) -> u32 {
        self.segment_count
    }

    #[must_use]
    /// Returns the number of indexed assets.
    pub const fn file_count(&self) -> u32 {
        self.file_count
    }

    #[must_use]
    /// Returns the total number of content blocks.
    pub const fn total_blocks(&self) -> u32 {
        self.total_blocks
    }

    #[must_use]
    /// Returns the number of leading block-map pages.
    pub const fn map_page_count(&self) -> u32 {
        self.map_page_count
    }

    #[must_use]
    /// Returns the number of trailing reusable-chunk pages.
    pub const fn reuse_page_count(&self) -> u32 {
        self.reuse_page_count
    }

    /// Decodes a segment record by catalog index.
    ///
    /// # Errors
    ///
    /// Returns an error when `index` is outside the validated inventory.
    pub fn segment(&self, index: u32) -> Result<SegmentRecord> {
        if index >= self.segment_count {
            return Err(Error::InvalidFormat("segment index"));
        }
        SegmentRecord::parse(
            &self.bytes,
            table_offset(self.segment_offset, index, SEGMENT_RECORD_SIZE)?,
        )
    }

    /// Decodes a file record by catalog index.
    ///
    /// # Errors
    ///
    /// Returns an error when `index` is outside the validated inventory.
    pub fn file(&self, index: u32) -> Result<FileRecord> {
        if index >= self.file_count {
            return Err(Error::InvalidFormat("file index"));
        }
        FileRecord::parse(
            &self.bytes,
            table_offset(self.file_offset, index, FILE_RECORD_SIZE)?,
        )
    }

    /// Decodes a keyed path-index slot.
    ///
    /// # Errors
    ///
    /// Returns an error when `index` is outside the validated table.
    pub fn path_slot(&self, index: u32) -> Result<PathSlot> {
        if index >= self.path_slot_count {
            return Err(Error::InvalidFormat("path slot index"));
        }
        PathSlot::parse(
            &self.bytes,
            table_offset(self.path_slot_offset, index, PATH_SLOT_SIZE)?,
        )
    }

    /// Decodes an encrypted metadata-page descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error when `index` is outside the validated table.
    pub fn page(&self, index: u32) -> Result<PageRecord> {
        if index >= self.page_count {
            return Err(Error::InvalidFormat("page index"));
        }
        PageRecord::parse(
            &self.bytes,
            table_offset(self.page_offset, index, PAGE_RECORD_SIZE)?,
        )
    }

    /// Resolves the canonical UTF-8 path owned by a file record.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid file index, range, or UTF-8 sequence.
    pub fn path(&self, file_index: u32) -> Result<&str> {
        let file = self.file(file_index)?;
        let start = self
            .path_pool_offset
            .checked_add(file.path_offset as usize)
            .ok_or(Error::InvalidFormat("path offset overflow"))?;
        let bytes = checked_slice(&self.bytes, start, file.path_len as usize)?;
        std::str::from_utf8(bytes).map_err(|_| Error::InvalidFormat("path is not UTF-8"))
    }

    /// Looks up a canonical path in the keyed open-addressed index.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-canonical path or malformed index.
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

/// Encodes one canonical block-map page.
///
/// # Errors
///
/// Returns an error when `records` is empty or exceeds one page.
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

/// Validates and decodes all records from one block-map page.
///
/// # Errors
///
/// Returns an error for a malformed header, payload, or block record.
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

/// Validates a block-map page and returns its record count without allocating.
///
/// # Errors
///
/// Returns an error for a malformed header or payload length.
pub fn validate_map_page(bytes: &[u8], expected_first: u32) -> Result<usize> {
    parse_page_header(bytes, MAP_PAGE_MAGIC, BLOCK_REF_SIZE, expected_first)
}

/// Decodes one block reference from an already validated map page.
///
/// # Errors
///
/// Returns an error when the index overflows or lies outside the page.
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

/// Encodes one canonical reusable-chunk page.
///
/// # Errors
///
/// Returns an error when `records` is empty or exceeds one page.
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

/// Validates and decodes all records from one reusable-chunk page.
///
/// # Errors
///
/// Returns an error for a malformed header, payload, or reuse record.
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

/// Checks the platform-independent path rules used by package indexes.
///
/// # Errors
///
/// Returns [`Error::InvalidPath`] for empty, absolute, ambiguous, or non-portable paths.
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

fn validate_catalog_len(total_len: usize) -> Result<()> {
    validate_count(total_len, MAX_CATALOG_PLAIN_LEN, "catalog plaintext length")
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
    let mut result = [0; N];
    result.copy_from_slice(checked_slice(bytes, offset, N)?);
    Ok(result)
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

    fn block() -> BlockRef {
        BlockRef {
            logical_offset: 0,
            segment_ordinal: 0,
            segment_block_ordinal: 0,
            physical_offset: SEGMENT_HEADER_SIZE as u64,
            stored_len: 32,
            plain_len: 16,
            codec: Codec::Raw,
            cipher_digest: [4; 16],
        }
    }

    fn segment_header() -> SegmentHeader {
        SegmentHeader {
            project_id: ProjectId([1; 16]),
            segment_uid: [2; 16],
            salt: [3; 16],
            nonce_prefix: [4; 8],
            block_count: 1,
            payload_len: 32,
            file_len: SEGMENT_HEADER_SIZE as u64 + 32,
        }
    }

    fn snapshot_header() -> SnapshotHeader {
        SnapshotHeader {
            project_id: ProjectId([1; 16]),
            release_sequence: 1,
            catalog_stored_len: 16,
            catalog_plain_len: 1,
            page_region_offset: SNAPSHOT_HEADER_SIZE as u64 + 16,
            page_count: 0,
            snapshot_salt: [2; 16],
            nonce_prefix: [3; 8],
            signing_key_id: [4; 16],
            source_fingerprint: [5; 32],
            signature: [6; SIGNATURE_LEN],
        }
    }

    fn catalog_data() -> (CatalogData, [u8; 32]) {
        let path = b"a";
        let path_key = [9; 32];
        let digest = blake3::keyed_hash(&path_key, path);
        let hash = u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap());
        let mut slots = vec![
            PathSlot {
                hash: 0,
                file_index: EMPTY_PATH_SLOT,
            };
            2
        ];
        slots[hash as usize & 1] = PathSlot {
            hash,
            file_index: 0,
        };
        let first_page_len = (PAGE_HEADER_SIZE + BLOCK_REF_SIZE) as u32;
        let second_page_len = (PAGE_HEADER_SIZE + REUSE_RECORD_SIZE) as u32;
        (
            CatalogData {
                segments: vec![SegmentRecord {
                    id: SegmentId([7; 32]),
                    uid: [2; 16],
                    salt: [3; 16],
                    nonce_prefix: [4; 8],
                    file_len: SEGMENT_HEADER_SIZE as u64 + 32,
                    payload_len: 32,
                    block_count: 1,
                    availability: Availability::Required,
                }],
                files: vec![FileRecord {
                    path_offset: 0,
                    path_len: 1,
                    layout: LayoutKind::Fixed,
                    access: AccessClass::Hot,
                    logical_len: 16,
                    first_block: 0,
                    block_count: 1,
                    fixed_block_len: 16,
                }],
                path_slots: slots,
                path_pool: path.to_vec(),
                pages: vec![
                    PageRecord {
                        kind: PageKind::BlockMap,
                        codec: Codec::Raw,
                        nonce_ordinal: 1,
                        relative_offset: 0,
                        stored_len: first_page_len + 16,
                        plain_len: first_page_len,
                        digest: [0; 32],
                    },
                    PageRecord {
                        kind: PageKind::Reuse,
                        codec: Codec::Raw,
                        nonce_ordinal: 2,
                        relative_offset: u64::from(first_page_len + 16),
                        stored_len: second_page_len + 16,
                        plain_len: second_page_len,
                        digest: [0; 32],
                    },
                ],
                total_blocks: 1,
                map_page_count: 1,
                reuse_page_count: 1,
            },
            path_key,
        )
    }

    #[test]
    fn block_ref_wire_size_is_exact() {
        let record = block();
        let mut bytes = Vec::new();
        record.encode_into(&mut bytes);
        assert_eq!(bytes.len(), BLOCK_REF_SIZE);
        assert_eq!(BlockRef::parse(&bytes, 0).unwrap(), record);
    }

    #[test]
    fn paths_are_strictly_canonical() {
        assert!(validate_canonical_path("video/opening.mp4").is_ok());
        for invalid in ["", "/a", "a/", "a//b", "a/./b", "a/../b", "a\\b", "a\0b"] {
            assert!(validate_canonical_path(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn header_parsers_reject_every_noncanonical_field_group() {
        let snapshot = snapshot_header();
        assert_eq!(
            SnapshotHeader::parse(&snapshot.encode(false)).unwrap(),
            snapshot
        );
        assert_ne!(snapshot.encode(true), snapshot.encode(false));
        let mut cases = Vec::new();
        cases.push(vec![0; SNAPSHOT_HEADER_SIZE - 1]);
        for (offset, value) in [
            (0, 0),
            (8, 2),
            (12, 1),
            (40, 1),
            (48, 0),
            (56, 0),
            (64, 0),
            (74, 2),
            (76, 1),
            (SIGNATURE_OFFSET + SIGNATURE_LEN, 1),
        ] {
            let mut bytes = snapshot.encode(false).to_vec();
            bytes[offset] = value;
            cases.push(bytes);
        }
        for bytes in cases {
            assert!(SnapshotHeader::parse(&bytes).is_err());
        }

        let segment = segment_header();
        assert_eq!(SegmentHeader::parse(&segment.encode()).unwrap(), segment);
        let mut cases = vec![vec![0; SEGMENT_HEADER_SIZE - 1]];
        for (offset, value) in [(0, 0), (8, 2), (12, 1), (76, 1), (96, 1), (88, 0)] {
            let mut bytes = segment.encode().to_vec();
            bytes[offset] = value;
            cases.push(bytes);
        }
        for bytes in cases {
            assert!(SegmentHeader::parse(&bytes).is_err());
        }
    }

    #[test]
    fn wire_records_roundtrip_and_reject_invalid_discriminants_and_lengths() {
        for value in [0, 1] {
            assert!(Codec::parse(value).is_ok());
            assert!(Availability::parse(value).is_ok());
            assert!(LayoutKind::parse(value).is_ok());
        }
        for value in 0..=3 {
            assert!(AccessClass::parse(value).is_ok());
        }
        assert!(PageKind::parse(1).is_ok());
        assert!(PageKind::parse(2).is_ok());
        assert!(Codec::parse(2).is_err());
        assert!(Availability::parse(2).is_err());
        assert!(LayoutKind::parse(2).is_err());
        assert!(AccessClass::parse(4).is_err());
        assert!(PageKind::parse(0).is_err());

        let segment = SegmentRecord {
            id: SegmentId([1; 32]),
            uid: [2; 16],
            salt: [3; 16],
            nonce_prefix: [4; 8],
            file_len: SEGMENT_HEADER_SIZE as u64,
            payload_len: 0,
            block_count: 0,
            availability: Availability::Deferred,
        };
        let mut bytes = Vec::new();
        segment.encode_into(&mut bytes);
        assert_eq!(SegmentRecord::parse(&bytes, 0).unwrap(), segment);
        bytes[93] = 1;
        assert!(SegmentRecord::parse(&bytes, 0).is_err());
        bytes[93] = 0;
        bytes[92] = 2;
        assert!(SegmentRecord::parse(&bytes, 0).is_err());

        let files = [
            FileRecord {
                path_offset: 0,
                path_len: 1,
                layout: LayoutKind::Fixed,
                access: AccessClass::Transient,
                logical_len: 1,
                first_block: 0,
                block_count: 1,
                fixed_block_len: 1,
            },
            FileRecord {
                layout: LayoutKind::ContentDefined,
                fixed_block_len: 0,
                ..FileRecord {
                    path_offset: 0,
                    path_len: 1,
                    layout: LayoutKind::Fixed,
                    access: AccessClass::Streaming,
                    logical_len: 1,
                    first_block: 0,
                    block_count: 1,
                    fixed_block_len: 1,
                }
            },
        ];
        for file in files {
            let mut bytes = Vec::new();
            file.encode_into(&mut bytes);
            assert_eq!(FileRecord::parse(&bytes, 0).unwrap(), file);
        }
        let mut bytes = Vec::new();
        files[0].encode_into(&mut bytes);
        bytes[24..28].fill(0);
        assert!(FileRecord::parse(&bytes, 0).is_err());
        bytes[6] = LayoutKind::ContentDefined as u8;
        bytes[24] = 1;
        assert!(FileRecord::parse(&bytes, 0).is_err());
        bytes[28] = 1;
        assert!(FileRecord::parse(&bytes, 0).is_err());

        let slot = PathSlot {
            hash: 3,
            file_index: 4,
        };
        let mut bytes = Vec::new();
        slot.encode_into(&mut bytes);
        assert_eq!(PathSlot::parse(&bytes, 0).unwrap(), slot);
        bytes[12] = 1;
        assert!(PathSlot::parse(&bytes, 0).is_err());
    }

    #[test]
    fn pages_and_catalog_cover_success_and_corruption_paths() {
        let block = block();
        let map = encode_map_page(3, &[block]).unwrap();
        assert_eq!(parse_map_page(&map, 3).unwrap(), [block]);
        assert_eq!(map_page_record(&map, 0).unwrap(), block);
        let reuse = ReuseRecord {
            chunk_id: [8; 32],
            block,
        };
        let reuse_page = encode_reuse_page(4, &[reuse]).unwrap();
        assert_eq!(parse_reuse_page(&reuse_page, 4).unwrap(), [reuse]);
        assert!(encode_map_page(0, &[]).is_err());
        assert!(encode_reuse_page(0, &[]).is_err());
        assert!(encode_map_page(0, &vec![block; BLOCKS_PER_MAP_PAGE + 1]).is_err());
        assert!(encode_reuse_page(0, &vec![reuse; REUSE_PER_PAGE + 1]).is_err());
        assert!(map_page_record(&map, usize::MAX).is_err());
        let mutations = [(0, 0), (8, 2), (10, 0), (12, 0)];
        for (offset, value) in mutations {
            let mut bytes = map.clone();
            bytes[offset] = value;
            assert!(validate_map_page(&bytes, 3).is_err());
        }
        assert!(validate_map_page(&map[..PAGE_HEADER_SIZE], 3).is_err());
        assert!(validate_map_page(&map[..map.len() - 1], 3).is_err());
        let mut damaged_map = map.clone();
        damaged_map[PAGE_HEADER_SIZE + 31] = 1;
        assert!(parse_map_page(&damaged_map, 3).is_err());
        let mut damaged_reuse = reuse_page.clone();
        damaged_reuse[PAGE_HEADER_SIZE + 32 + 31] = 1;
        assert!(parse_reuse_page(&damaged_reuse, 4).is_err());

        let mut page_bytes = Vec::new();
        PageRecord {
            kind: PageKind::BlockMap,
            codec: Codec::Raw,
            nonce_ordinal: 1,
            relative_offset: 0,
            stored_len: 32,
            plain_len: 16,
            digest: [0; 32],
        }
        .encode_into(&mut page_bytes);
        for (offset, value) in [(16, 15), (20, 0), (0, 0), (1, 2), (2, 1), (56, 1)] {
            let mut damaged = page_bytes.clone();
            damaged[offset] = value;
            assert!(PageRecord::parse(&damaged, 0).is_err());
        }
        let mut block_bytes = Vec::new();
        block.encode_into(&mut block_bytes);
        for (offset, value) in [(31, 1), (30, 2), (26, 0), (22, 31)] {
            let mut damaged = block_bytes.clone();
            damaged[offset] = value;
            assert!(BlockRef::parse(&damaged, 0).is_err());
        }

        let (data, path_key) = catalog_data();
        let bytes = data.encode().unwrap();
        let catalog = Catalog::parse(bytes.clone().into()).unwrap();
        assert_eq!(catalog.segment_count(), 1);
        assert_eq!(catalog.file_count(), 1);
        assert_eq!(catalog.total_blocks(), 1);
        assert_eq!(catalog.map_page_count(), 1);
        assert_eq!(catalog.reuse_page_count(), 1);
        assert_eq!(catalog.path(0).unwrap(), "a");
        assert_eq!(catalog.find_file("a", &path_key).unwrap(), Some(0));
        assert_eq!(catalog.find_file("missing", &path_key).unwrap(), None);
        assert!(catalog.segment(1).is_err());
        assert!(catalog.file(1).is_err());
        assert!(catalog.path_slot(2).is_err());
        assert!(catalog.page(2).is_err());
        assert!(catalog.find_file("/bad", &path_key).is_err());

        let mut invalid = data.clone();
        invalid.path_slots.clear();
        assert!(invalid.encode().is_err());
        let mut invalid = data.clone();
        invalid.reuse_page_count = 0;
        assert!(invalid.encode().is_err());

        for (offset, value) in [
            (0, 0),
            (8, 2),
            (10, 0),
            (12, 0xff),
            (28, 0),
            (36, 0),
            (44, 0),
        ] {
            let mut damaged = bytes.clone();
            damaged[offset] = value;
            assert!(Catalog::parse(damaged.into()).is_err());
        }
        assert!(Catalog::parse(bytes[..CATALOG_HEADER_SIZE - 1].into()).is_err());

        let mut oversized_slots = bytes.clone();
        oversized_slots[28..32].copy_from_slice(&(1_u32 << 31).to_le_bytes());
        assert!(Catalog::parse(oversized_slots.into()).is_err());

        let segment_offset = CATALOG_HEADER_SIZE;
        let file_offset = segment_offset + SEGMENT_RECORD_SIZE;
        let slot_offset = file_offset + FILE_RECORD_SIZE;
        let page_offset = slot_offset + 2 * PATH_SLOT_SIZE + 1;
        let occupied_slot = [slot_offset, slot_offset + PATH_SLOT_SIZE]
            .into_iter()
            .find(|offset| bytes[offset + 8..offset + 12] != EMPTY_PATH_SLOT.to_le_bytes())
            .unwrap();
        for (offset, value) in [
            (20, 0xff),
            (segment_offset + 72, 0),
            (file_offset + 4, 0),
            (file_offset + 20, 2),
            (occupied_slot + 8, 1),
            (page_offset, PageKind::Reuse as u8),
            (page_offset + 20, 1),
            (page_offset + 8, 1),
        ] {
            let mut damaged = bytes.clone();
            damaged[offset] = value;
            assert!(Catalog::parse(damaged.into()).is_err(), "offset {offset}");
        }

        let empty_slot = [slot_offset, slot_offset + PATH_SLOT_SIZE]
            .into_iter()
            .find(|offset| *offset != occupied_slot)
            .unwrap();
        let mut full_index = bytes.clone();
        full_index[empty_slot + 8..empty_slot + 12].copy_from_slice(&0_u32.to_le_bytes());
        let full_index = Catalog::parse(full_index.into()).unwrap();
        assert!(full_index.find_file("missing", &path_key).is_err());

        let mut wrong_plain_len = bytes.clone();
        wrong_plain_len[page_offset + 16] = 79;
        wrong_plain_len[page_offset + 20] = 63;
        assert!(Catalog::parse(wrong_plain_len.into()).is_err());
    }

    #[test]
    fn low_level_bounds_helpers_reject_overflow_and_bad_ranges() {
        assert!(checked_table_end(usize::MAX, 1, 1).is_err());
        assert!(checked_table_end(0, usize::MAX, 2).is_err());
        assert!(usize_to_u32(usize::MAX).is_err());
        assert!(require_len(&[], 1, "length").is_err());
        assert!(require_magic(&[], b"12345678", "magic").is_err());
        assert!(require_zero(&[], 0..1, "zero").is_err());
        assert!(require_zero(&[1], 0..1, "zero").is_err());
        assert!(checked_slice(&[], usize::MAX, 2).is_err());
        assert!(checked_slice(&[], 0, 1).is_err());
        assert!(validate_count(2, 1, "count").is_err());
        assert!(validate_catalog_len(MAX_CATALOG_PLAIN_LEN + 1).is_err());
        assert!(validate_encoded_length(Codec::Raw, 31, 16, "raw").is_err());
        assert!(validate_encoded_length(Codec::Zstd, 32, 16, "zstd").is_err());
        assert!(validate_encoded_length(Codec::Raw, 0, u32::MAX, "overflow").is_err());
    }
}
