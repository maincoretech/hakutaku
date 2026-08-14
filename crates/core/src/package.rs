use crate::cache::ClockCache;
use crate::crypto::{self, Aes256Key, ProjectKeys};
use crate::format::{
    AccessClass, Availability, BLOCKS_PER_MAP_PAGE, BlockRef, Catalog, Codec, FileRecord,
    LayoutKind, PageKind, ProjectId, REUSE_PER_PAGE, ReuseRecord, SEGMENT_HEADER_SIZE,
    SegmentHeader, SegmentId, SegmentRecord, SnapshotHeader, map_page_record, parse_reuse_page,
    validate_map_page,
};
use crate::io::{DirectorySegmentSource, LocalFile, PositionedFile, SegmentSource};
use crate::{Error, Result};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

thread_local! {
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

/// Explicit bounds for all rebuildable runtime state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceBudget {
    /// Maximum decrypted block-map bytes retained in memory.
    pub map_page_cache_bytes: usize,
    /// Maximum plaintext content-block bytes retained in memory.
    pub plaintext_cache_bytes: usize,
    /// Maximum explicitly prefetched plaintext bytes retained for future reads.
    pub prefetch_cache_bytes: usize,
    /// Maximum idle open segment handles retained for reuse.
    pub idle_segment_handles: usize,
    /// Maximum one-hit normal blocks tracked before cache admission.
    pub normal_probation_entries: usize,
}

/// Caller-owned release acceptance policy.
///
/// Hakutaku authenticates the signed sequence but deliberately does not own
/// persistent rollback state. Launchers should persist the highest accepted
/// sequence and provide it here on subsequent opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenPolicy {
    /// Lowest signed release sequence accepted by the caller.
    pub minimum_release_sequence: Option<u64>,
}

impl OpenPolicy {
    #[must_use]
    /// Requires an authenticated snapshot at or above `minimum`.
    pub const fn requiring(minimum: u64) -> Self {
        Self {
            minimum_release_sequence: Some(minimum),
        }
    }
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            map_page_cache_bytes: 2 * 1024 * 1024,
            plaintext_cache_bytes: 64 * 1024 * 1024,
            prefetch_cache_bytes: 2 * 1024 * 1024,
            idle_segment_handles: 16,
            normal_probation_entries: 256,
        }
    }
}

impl ResourceBudget {
    #[must_use]
    /// Returns conservative cache bounds suitable for memory-constrained devices.
    pub const fn memory_constrained() -> Self {
        Self {
            map_page_cache_bytes: 512 * 1024,
            plaintext_cache_bytes: 16 * 1024 * 1024,
            prefetch_cache_bytes: 512 * 1024,
            idle_segment_handles: 4,
            normal_probation_entries: 64,
        }
    }

    #[must_use]
    /// Disables every rebuildable cache while preserving correct reads.
    pub const fn cache_disabled() -> Self {
        Self {
            map_page_cache_bytes: 0,
            plaintext_cache_bytes: 0,
            prefetch_cache_bytes: 0,
            idle_segment_handles: 0,
            normal_probation_entries: 0,
        }
    }
}

#[derive(Clone)]
/// Authenticated, random-access view of one immutable release snapshot.
pub struct Package {
    inner: Arc<PackageInner>,
}

struct PackageInner {
    snapshot: Arc<dyn PositionedFile>,
    source: Arc<dyn SegmentSource>,
    header: SnapshotHeader,
    catalog: Catalog,
    keys: ProjectKeys,
    snapshot_key: Aes256Key,
    path_key: [u8; 32],
    page_cache: Mutex<ClockCache<u32>>,
    block_cache: Mutex<ClockCache<BlockKey>>,
    prefetch_cache: Mutex<ClockCache<BlockKey>>,
    probation: Mutex<VecDeque<BlockKey>>,
    handles: Mutex<HandleCache>,
    budget: ResourceBudget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BlockKey {
    segment_ordinal: u16,
    block_ordinal: u32,
}

struct HandleCache {
    capacity: usize,
    values: HashMap<SegmentId, Arc<SegmentHandle>>,
    order: VecDeque<SegmentId>,
}

impl HandleCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            values: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, id: &SegmentId) -> Option<Arc<SegmentHandle>> {
        let value = Arc::clone(self.values.get(id)?);
        if let Some(position) = self.order.iter().position(|candidate| candidate == id) {
            self.order.remove(position);
        }
        self.order.push_back(*id);
        Some(value)
    }

    fn insert(&mut self, id: SegmentId, value: Arc<SegmentHandle>) {
        if self.capacity == 0 {
            return;
        }
        if self.values.contains_key(&id) {
            return;
        }
        while self.values.len() >= self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.values.remove(&oldest);
        }
        self.order.push_back(id);
        self.values.insert(id, value);
    }

    fn clear(&mut self) {
        self.values.clear();
        self.order.clear();
    }
}

struct SegmentHandle {
    file: Arc<dyn PositionedFile>,
    key: Aes256Key,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Public metadata for one indexed asset.
pub struct AssetInfo {
    /// Canonical package-relative UTF-8 path.
    pub path: String,
    /// Plaintext asset length in bytes.
    pub len: u64,
    /// Runtime cache and access hint.
    pub access: AccessClass,
}

/// Immutable segment metadata available before any segment is opened.
///
/// Launchers can use this signed snapshot data to determine which required
/// segments must be installed and which deferred segments may be fetched on
/// demand. The runtime itself stays transport-agnostic through [`SegmentSource`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentInfo {
    /// Content-derived immutable segment identifier.
    pub id: SegmentId,
    /// Complete segment-file length.
    pub len: u64,
    /// Installation policy signed into the snapshot.
    pub availability: Availability,
}

#[derive(Clone)]
/// Lightweight handle for random or sequential reads from one package asset.
pub struct Asset {
    package: Package,
    file_index: u32,
    record: FileRecord,
}

/// [`Read`] and [`Seek`] cursor over an [`Asset`].
pub struct AssetCursor {
    asset: Asset,
    position: u64,
    current_block: Option<CursorBlock>,
    previous_streaming_block: Option<CursorBlock>,
    reader: BlockReader,
}

/// Persistent random-access session for repeated reads from one [`Asset`].
///
/// Unlike [`Asset::read_at`], a session retains its active segment handle,
/// decode buffers, and the current/previous streaming blocks between calls.
pub struct AssetReadSession {
    asset: Asset,
    current_block: Option<CursorBlock>,
    previous_streaming_block: Option<CursorBlock>,
    reader: BlockReader,
}

struct CursorBlock {
    reference: BlockRef,
    data: BlockData,
}

impl CursorBlock {
    fn covers(&self, position: u64) -> bool {
        let end = self
            .reference
            .logical_offset
            .saturating_add(u64::from(self.reference.plain_len));
        position >= self.reference.logical_offset && position < end
    }
}

#[derive(Default)]
struct BlockReader {
    ciphertext: Vec<u8>,
    plaintext: Vec<u8>,
    segment: Option<ReaderSegment>,
}

struct ReaderSegment {
    ordinal: u16,
    handle: Arc<SegmentHandle>,
}

impl BlockReader {
    fn prepare_ciphertext(&mut self, len: usize) -> &mut Vec<u8> {
        if self.ciphertext.capacity() < len && self.plaintext.capacity() >= len {
            std::mem::swap(&mut self.ciphertext, &mut self.plaintext);
        }
        self.ciphertext.resize(len, 0);
        &mut self.ciphertext
    }

    fn take_raw(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.ciphertext)
    }

    fn prepare_plaintext(&mut self, len: usize) -> &mut Vec<u8> {
        self.plaintext.resize(len, 0);
        &mut self.plaintext
    }

    fn take_plaintext(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.plaintext)
    }

    fn recycle(&mut self, data: BlockData) {
        let BlockData::Owned(mut bytes) = data else {
            return;
        };
        bytes.clear();
        if bytes.capacity() > self.plaintext.capacity() {
            self.plaintext = bytes;
        }
    }

    fn segment_handle(
        &mut self,
        package: &PackageInner,
        ordinal: u16,
        record: &SegmentRecord,
    ) -> Result<Arc<SegmentHandle>> {
        if let Some(segment) = &self.segment
            && segment.ordinal == ordinal
        {
            return Ok(Arc::clone(&segment.handle));
        }
        let handle = package.open_segment(record)?;
        self.segment = Some(ReaderSegment {
            ordinal,
            handle: Arc::clone(&handle),
        });
        Ok(handle)
    }
}

enum BlockData {
    Shared(Arc<Vec<u8>>),
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for BlockData {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Shared(bytes) => bytes,
            Self::Owned(bytes) => bytes,
        }
    }
}

impl BlockData {
    fn into_shared(self) -> Arc<Vec<u8>> {
        match self {
            Self::Shared(bytes) => bytes,
            Self::Owned(bytes) => Arc::new(bytes),
        }
    }
}

impl Package {
    /// Opens and authenticates a package through caller-provided random-access backends.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O failures, invalid signatures, failed authentication,
    /// resource-limit violations, or any non-canonical snapshot structure.
    pub fn open(
        snapshot: Arc<dyn PositionedFile>,
        source: Arc<dyn SegmentSource>,
        root_key: [u8; 32],
        verifying_key: [u8; 32],
        budget: ResourceBudget,
    ) -> Result<Self> {
        Self::open_with_policy(
            snapshot,
            source,
            root_key,
            verifying_key,
            budget,
            OpenPolicy::default(),
        )
    }

    /// Opens and authenticates a package while enforcing caller-owned rollback policy.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReleaseRollback`] after signature verification when the
    /// signed sequence is below the caller's accepted floor, in addition to the
    /// errors returned by [`Package::open`].
    pub fn open_with_policy(
        snapshot: Arc<dyn PositionedFile>,
        source: Arc<dyn SegmentSource>,
        root_key: [u8; 32],
        verifying_key: [u8; 32],
        budget: ResourceBudget,
        policy: OpenPolicy,
    ) -> Result<Self> {
        let mut header_bytes = vec![0_u8; crate::format::SNAPSHOT_HEADER_SIZE];
        snapshot.read_exact_at(0, &mut header_bytes)?;
        let header = SnapshotHeader::parse(&header_bytes)?;
        let catalog_len = usize::try_from(header.catalog_stored_len)
            .map_err(|_| Error::LimitExceeded("catalog stored length"))?;
        let mut catalog_ciphertext = vec![0_u8; catalog_len];
        snapshot.read_exact_at(
            crate::format::SNAPSHOT_HEADER_SIZE as u64,
            &mut catalog_ciphertext,
        )?;
        crypto::verify_snapshot_signature(&header, &catalog_ciphertext, &verifying_key)?;
        if let Some(minimum) = policy.minimum_release_sequence
            && header.release_sequence < minimum
        {
            return Err(Error::ReleaseRollback {
                minimum,
                actual: header.release_sequence,
            });
        }

        let keys = ProjectKeys::new(root_key, header.project_id);
        let snapshot_key_bytes = keys.snapshot_key(&header.snapshot_salt);
        let snapshot_key = Aes256Key::new(&snapshot_key_bytes)?;
        let aad = crypto::catalog_aad(&header);
        snapshot_key.open(
            crypto::nonce(header.nonce_prefix, 0),
            &aad,
            &mut catalog_ciphertext,
            "snapshot catalog",
        )?;
        // SnapshotHeader::parse bounds this to 64 MiB, which fits every
        // supported 32-bit and 64-bit target.
        let catalog_plain_len = header.catalog_plain_len as usize;
        let catalog_bytes = decompress_zstd_exact(&catalog_ciphertext, catalog_plain_len)?;
        let catalog = Catalog::parse(Arc::from(catalog_bytes))?;
        validate_page_counts(
            catalog.map_page_count(),
            catalog.reuse_page_count(),
            header.page_count,
        )?;
        let page_bytes = if header.page_count == 0 {
            0
        } else {
            let last = catalog.page(header.page_count - 1)?;
            checked_page_region_len(last.relative_offset, last.stored_len)?
        };
        validate_snapshot_len(snapshot.len()?, header.page_region_offset, page_bytes)?;
        let path_key = keys.path_key();
        Ok(Self {
            inner: Arc::new(PackageInner {
                snapshot,
                source,
                header,
                catalog,
                keys,
                snapshot_key,
                path_key,
                page_cache: Mutex::new(ClockCache::new(budget.map_page_cache_bytes)),
                block_cache: Mutex::new(ClockCache::new(budget.plaintext_cache_bytes)),
                prefetch_cache: Mutex::new(ClockCache::new(budget.prefetch_cache_bytes)),
                // Probation is sparse in normal play; grow only when a Normal
                // block is actually touched instead of reserving caller budget
                // eagerly on package open (important for mobile launchers).
                probation: Mutex::new(VecDeque::new()),
                handles: Mutex::new(HandleCache::new(budget.idle_segment_handles)),
                budget,
            }),
        })
    }

    /// Opens a local `game.haku` snapshot and its sibling segment directory.
    ///
    /// # Errors
    ///
    /// Propagates filesystem and package-validation failures.
    pub fn open_directory(
        snapshot_path: impl AsRef<Path>,
        segment_directory: impl AsRef<Path>,
        root_key: [u8; 32],
        verifying_key: [u8; 32],
        budget: ResourceBudget,
    ) -> Result<Self> {
        Self::open_directory_with_policy(
            snapshot_path,
            segment_directory,
            root_key,
            verifying_key,
            budget,
            OpenPolicy::default(),
        )
    }

    /// Opens a local package while enforcing caller-owned rollback policy.
    ///
    /// # Errors
    ///
    /// Propagates filesystem, authentication, canonical-format, and rollback failures.
    pub fn open_directory_with_policy(
        snapshot_path: impl AsRef<Path>,
        segment_directory: impl AsRef<Path>,
        root_key: [u8; 32],
        verifying_key: [u8; 32],
        budget: ResourceBudget,
        policy: OpenPolicy,
    ) -> Result<Self> {
        Self::open_with_policy(
            Arc::new(LocalFile::open(snapshot_path)?),
            Arc::new(DirectorySegmentSource::new(
                segment_directory.as_ref().to_path_buf(),
            )),
            root_key,
            verifying_key,
            budget,
            policy,
        )
    }

    #[must_use]
    /// Returns the stable project identifier authenticated by this snapshot.
    pub fn project_id(&self) -> ProjectId {
        self.inner.header.project_id
    }

    #[must_use]
    /// Returns the monotonic publisher release sequence.
    pub fn release_sequence(&self) -> u64 {
        self.inner.header.release_sequence
    }

    #[must_use]
    /// Returns the canonical source-tree fingerprint recorded by the packer.
    pub fn source_fingerprint(&self) -> [u8; 32] {
        self.inner.header.source_fingerprint
    }

    /// Resolves a canonical asset path to a read handle.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AssetNotFound`] when absent, or a validation error for bad paths.
    pub fn asset(&self, path: &str) -> Result<Asset> {
        let Some(file_index) = self.inner.catalog.find_file(path, &self.inner.path_key)? else {
            return Err(Error::AssetNotFound);
        };
        Ok(Asset {
            package: self.clone(),
            file_index,
            record: self.inner.catalog.file(file_index)?,
        })
    }

    /// Reports whether a canonical asset path exists without opening segment data.
    ///
    /// # Errors
    ///
    /// Returns a validation error for a non-canonical path or malformed catalog.
    pub fn contains_asset(&self, path: &str) -> Result<bool> {
        Ok(self
            .inner
            .catalog
            .find_file(path, &self.inner.path_key)?
            .is_some())
    }

    /// Returns public metadata for every asset in catalog order.
    ///
    /// # Errors
    ///
    /// Returns an error if validated catalog data cannot be decoded.
    pub fn list_assets(&self) -> Result<Vec<AssetInfo>> {
        let mut assets = Vec::with_capacity(self.inner.catalog.file_count() as usize);
        for index in 0..self.inner.catalog.file_count() {
            let file = self.inner.catalog.file(index)?;
            assets.push(AssetInfo {
                path: self.inner.catalog.path(index)?.to_owned(),
                len: file.logical_len,
                access: file.access,
            });
        }
        Ok(assets)
    }

    /// Signed segment inventory for installers, patchers, and mobile launchers.
    pub fn list_segments(&self) -> Result<Vec<SegmentInfo>> {
        (0..self.inner.catalog.segment_count())
            .map(|index| {
                let record = self.inner.catalog.segment(index)?;
                Ok(SegmentInfo {
                    id: record.id,
                    len: record.file_len,
                    availability: record.availability,
                })
            })
            .collect()
    }

    /// Segment identities referenced by this complete snapshot.
    pub fn segment_ids(&self) -> Result<Vec<SegmentId>> {
        (0..self.inner.catalog.segment_count())
            .map(|index| self.inner.catalog.segment(index).map(|record| record.id))
            .collect()
    }

    /// Releases all rebuildable plaintext, page, and handle caches.
    pub fn trim(&self) {
        lock(&self.inner.page_cache).clear();
        lock(&self.inner.block_cache).clear();
        lock(&self.inner.prefetch_cache).clear();
        lock(&self.inner.probation).clear();
        lock(&self.inner.handles).clear();
    }

    /// Streams every segment and verifies its content-derived identifier.
    ///
    /// # Errors
    ///
    /// Returns an error for unavailable, unreadable, or corrupted segments.
    pub fn verify_segments(&self) -> Result<()> {
        let mut buffer = vec![0_u8; 1024 * 1024];
        for index in 0..self.inner.catalog.segment_count() {
            let record = self.inner.catalog.segment(index)?;
            let handle = self.inner.open_segment(&record)?;
            let mut hasher = blake3::Hasher::new();
            let mut offset = 0_u64;
            while offset < record.file_len {
                let remaining =
                    usize::try_from((record.file_len - offset).min(buffer.len() as u64))
                        .map_err(|_| Error::InvalidRange)?;
                handle
                    .file
                    .read_exact_at(offset, &mut buffer[..remaining])?;
                hasher.update(&buffer[..remaining]);
                offset += remaining as u64;
            }
            if hasher.finalize().as_bytes() != &record.id.0 {
                return Err(Error::Authentication("segment ID"));
            }
        }
        Ok(())
    }

    /// Packer-only cold metadata. Runtime users should never call this method.
    pub fn reuse_records(&self) -> Result<Vec<ReuseRecord>> {
        let mut records = Vec::with_capacity(self.inner.catalog.total_blocks() as usize);
        let first_page = self.inner.catalog.map_page_count();
        for reuse_index in 0..self.inner.catalog.reuse_page_count() {
            let bytes = self
                .inner
                .load_page(first_page + reuse_index, PageKind::Reuse)?;
            let expected_first = checked_record_index(reuse_index, REUSE_PER_PAGE as u32)?;
            records.extend(parse_reuse_page(&bytes, expected_first)?);
        }
        validate_record_total(records.len(), self.inner.catalog.total_blocks())?;
        Ok(records)
    }

    /// Returns the complete signed record for a segment ordinal.
    ///
    /// # Errors
    ///
    /// Returns an error when `ordinal` is outside the segment inventory.
    pub fn segment_record(&self, ordinal: u16) -> Result<SegmentRecord> {
        self.inner.catalog.segment(u32::from(ordinal))
    }
}

impl PackageInner {
    fn block_ref(&self, global_index: u32) -> Result<BlockRef> {
        let page_index = validate_global_block_index(
            global_index,
            self.catalog.total_blocks(),
            self.catalog.map_page_count(),
        )?;
        let expected_first = page_index * BLOCKS_PER_MAP_PAGE as u32;
        let bytes = self.load_page(page_index, PageKind::BlockMap)?;
        let count = validate_map_page(&bytes, expected_first)?;
        let local_index = (global_index - expected_first) as usize;
        validate_local_block_index(local_index, count)?;
        map_page_record(&bytes, local_index)
    }

    fn load_page(&self, page_index: u32, expected_kind: PageKind) -> Result<Arc<Vec<u8>>> {
        if let Some(value) = lock(&self.page_cache).get(&page_index) {
            return Ok(value);
        }
        let page = self.catalog.page(page_index)?;
        if page.kind != expected_kind {
            return Err(Error::InvalidFormat("page kind at use site"));
        }
        let offset = checked_offset(self.header.page_region_offset, page.relative_offset)?;
        let mut ciphertext = vec![0_u8; page.stored_len as usize];
        self.snapshot.read_exact_at(offset, &mut ciphertext)?;
        validate_digest(&ciphertext, &page.digest, "signed page digest")?;
        let aad = crypto::page_aad(
            self.header.project_id,
            self.header.release_sequence,
            page.kind,
            page.codec,
            page.nonce_ordinal,
            page.stored_len,
            page.plain_len,
        );
        self.snapshot_key.open(
            crypto::nonce(self.header.nonce_prefix, page.nonce_ordinal),
            &aad,
            &mut ciphertext,
            "snapshot page",
        )?;
        let decoded = decode_owned(page.codec, ciphertext, page.plain_len as usize)?;
        let decoded = Arc::new(decoded);
        lock(&self.page_cache).insert(page_index, Arc::clone(&decoded));
        Ok(decoded)
    }

    #[cfg(test)]
    fn load_block(&self, reference: &BlockRef, access: AccessClass) -> Result<BlockData> {
        self.load_block_buffered(reference, access, &mut BlockReader::default())
    }

    fn load_block_buffered(
        &self,
        reference: &BlockRef,
        access: AccessClass,
        reader: &mut BlockReader,
    ) -> Result<BlockData> {
        let key = BlockKey {
            segment_ordinal: reference.segment_ordinal,
            block_ordinal: reference.segment_block_ordinal,
        };
        if matches!(access, AccessClass::Hot | AccessClass::Normal)
            && let Some(value) = lock(&self.block_cache).get(&key)
        {
            if value.len() != reference.plain_len as usize {
                return Err(Error::InvalidFormat("cached block length mismatch"));
            }
            return Ok(BlockData::Shared(value));
        }
        if let Some(value) = lock(&self.prefetch_cache).get(&key) {
            if value.len() != reference.plain_len as usize {
                return Err(Error::InvalidFormat("prefetched block length mismatch"));
            }
            return Ok(BlockData::Shared(value));
        }

        let segment = self.catalog.segment(u32::from(reference.segment_ordinal))?;
        if reference.segment_block_ordinal >= segment.block_count {
            return Err(Error::InvalidFormat("segment block ordinal"));
        }
        let stored_end = reference
            .physical_offset
            .checked_add(u64::from(reference.stored_len))
            .ok_or(Error::InvalidRange)?;
        if reference.physical_offset < SEGMENT_HEADER_SIZE as u64 || stored_end > segment.file_len {
            return Err(Error::InvalidFormat("block physical range"));
        }
        let handle = reader.segment_handle(self, reference.segment_ordinal, &segment)?;
        let ciphertext = reader.prepare_ciphertext(reference.stored_len as usize);
        handle
            .file
            .read_exact_at(reference.physical_offset, ciphertext)?;
        if blake3::hash(ciphertext).as_bytes()[..16] != reference.cipher_digest {
            return Err(Error::Authentication("signed block digest"));
        }
        let aad = crypto::block_aad(
            self.header.project_id,
            &segment.uid,
            reference.segment_block_ordinal,
            reference.codec,
            reference.stored_len,
            reference.plain_len,
        );
        handle.key.open(
            crypto::nonce(segment.nonce_prefix, reference.segment_block_ordinal),
            &aad,
            ciphertext,
            "segment block",
        )?;
        let plaintext = decode_buffered(reference.codec, reader, reference.plain_len as usize)?;

        let admit = match access {
            AccessClass::Hot => true,
            AccessClass::Normal => self.second_normal_access(key),
            AccessClass::Streaming | AccessClass::Transient => false,
        };
        if admit {
            let plaintext = Arc::new(plaintext);
            lock(&self.block_cache).insert(key, Arc::clone(&plaintext));
            Ok(BlockData::Shared(plaintext))
        } else {
            Ok(BlockData::Owned(plaintext))
        }
    }

    fn prefetch_block(
        &self,
        reference: &BlockRef,
        access: AccessClass,
        reader: &mut BlockReader,
    ) -> Result<()> {
        if self.budget.prefetch_cache_bytes == 0
            || reference.plain_len as usize > self.budget.prefetch_cache_bytes
        {
            return Ok(());
        }
        let key = BlockKey {
            segment_ordinal: reference.segment_ordinal,
            block_ordinal: reference.segment_block_ordinal,
        };
        if lock(&self.block_cache).get(&key).is_some()
            || lock(&self.prefetch_cache).get(&key).is_some()
        {
            return Ok(());
        }
        let plaintext = self.load_block_buffered(reference, access, reader)?;
        if lock(&self.block_cache).get(&key).is_none() {
            lock(&self.prefetch_cache).insert(key, plaintext.into_shared());
        }
        Ok(())
    }

    fn second_normal_access(&self, key: BlockKey) -> bool {
        let capacity = self.budget.normal_probation_entries;
        if capacity == 0 {
            return false;
        }
        let mut probation = lock(&self.probation);
        if let Some(position) = probation.iter().position(|candidate| *candidate == key) {
            probation.remove(position);
            return true;
        }
        while probation.len() >= capacity {
            probation.pop_front();
        }
        probation.push_back(key);
        false
    }

    fn open_segment(&self, record: &SegmentRecord) -> Result<Arc<SegmentHandle>> {
        if let Some(handle) = lock(&self.handles).get(&record.id) {
            return Ok(handle);
        }
        let file = self.source.open(record.id)?;
        if file.len()? != record.file_len {
            return Err(Error::InvalidFormat("segment host file length"));
        }
        let mut bytes = vec![0_u8; SEGMENT_HEADER_SIZE];
        file.read_exact_at(0, &mut bytes)?;
        let header = SegmentHeader::parse(&bytes)?;
        if header.project_id != self.header.project_id
            || header.segment_uid != record.uid
            || header.salt != record.salt
            || header.nonce_prefix != record.nonce_prefix
            || header.block_count != record.block_count
            || header.payload_len != record.payload_len
            || header.file_len != record.file_len
        {
            return Err(Error::InvalidFormat(
                "segment header does not match catalog",
            ));
        }
        let key_bytes = self.keys.segment_key(&header);
        let handle = Arc::new(SegmentHandle {
            file,
            key: Aes256Key::new(&key_bytes)?,
        });
        lock(&self.handles).insert(record.id, Arc::clone(&handle));
        Ok(handle)
    }
}

impl Asset {
    #[must_use]
    /// Returns the plaintext asset length in bytes.
    pub const fn len(&self) -> u64 {
        self.record.logical_len
    }

    #[must_use]
    /// Reports whether this asset has zero plaintext bytes.
    pub const fn is_empty(&self) -> bool {
        self.record.logical_len == 0
    }

    #[must_use]
    /// Returns this asset's stable catalog index.
    pub const fn file_index(&self) -> u32 {
        self.file_index
    }

    /// Reads and returns the complete plaintext asset.
    ///
    /// # Errors
    ///
    /// Returns an error for allocation limits, unavailable segments, or failed validation.
    pub fn read(&self) -> Result<Vec<u8>> {
        let len = usize::try_from(self.len()).map_err(|_| Error::LimitExceeded("asset length"))?;
        let mut result = vec![0_u8; len];
        let read = self.read_at(0, &mut result)?;
        validate_complete_read(read, len)?;
        Ok(result)
    }

    /// Reads up to `destination.len()` bytes from a plaintext logical offset.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid offset, unavailable segment, or failed authentication.
    pub fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<usize> {
        self.read_session().read_at(offset, destination)
    }

    #[must_use]
    /// Creates a persistent session for repeated random-access reads.
    pub fn read_session(&self) -> AssetReadSession {
        AssetReadSession {
            asset: self.clone(),
            current_block: None,
            previous_streaming_block: None,
            reader: BlockReader::default(),
        }
    }

    fn read_at_buffered(
        &self,
        offset: u64,
        destination: &mut [u8],
        current_block: &mut Option<CursorBlock>,
        previous_streaming_block: &mut Option<CursorBlock>,
        reader: &mut BlockReader,
    ) -> Result<usize> {
        if offset > self.len() {
            return Err(Error::InvalidRange);
        }
        let requested = usize::try_from((self.len() - offset).min(destination.len() as u64))
            .map_err(|_| Error::InvalidRange)?;
        if requested == 0 {
            return Ok(0);
        }
        let mut written = 0_usize;
        let mut logical = offset;
        while written < requested {
            let covered = current_block
                .as_ref()
                .is_some_and(|block| block.covers(logical));
            if !covered {
                if previous_streaming_block
                    .as_ref()
                    .is_some_and(|block| block.covers(logical))
                {
                    std::mem::swap(current_block, previous_streaming_block);
                } else {
                    let reference = self.reference_for_offset(logical)?;
                    if self.record.access == AccessClass::Streaming {
                        if let Some(previous) = previous_streaming_block.take() {
                            reader.recycle(previous.data);
                        }
                        *previous_streaming_block = current_block.take();
                    } else {
                        if let Some(current) = current_block.take() {
                            reader.recycle(current.data);
                        }
                    }
                    let data = self.package.inner.load_block_buffered(
                        &reference,
                        self.record.access,
                        reader,
                    )?;
                    *current_block = Some(CursorBlock { reference, data });
                }
            }
            let block = current_block.as_ref().expect("read block loaded above");
            let within = usize::try_from(logical - block.reference.logical_offset)
                .map_err(|_| Error::InvalidFormat("block logical offset"))?;
            let bytes = block.data.as_ref();
            validate_block_coverage(within, bytes.len())?;
            let amount = (bytes.len() - within).min(requested - written);
            destination[written..written + amount].copy_from_slice(&bytes[within..within + amount]);
            written += amount;
            logical += amount as u64;
        }
        Ok(written)
    }

    /// Authenticates and decodes every block intersecting a logical range into
    /// the package's bounded prefetch cache.
    ///
    /// The operation is synchronous so engines can schedule it on their own
    /// existing task pool without Hakutaku creating threads or requiring an
    /// async runtime. The configured prefetch budget remains a strict bound.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid offset, unavailable segment, failed
    /// authentication, or malformed block coverage.
    pub fn prefetch_range(&self, offset: u64, len: usize) -> Result<()> {
        if offset > self.len() {
            return Err(Error::InvalidRange);
        }
        let requested_len = u64::try_from(len).map_err(|_| Error::InvalidRange)?;
        let requested = usize::try_from((self.len() - offset).min(requested_len))
            .map_err(|_| Error::InvalidRange)?;
        if requested == 0 {
            return Ok(());
        }
        let end = offset
            .checked_add(requested as u64)
            .ok_or(Error::InvalidRange)?;
        let mut logical = offset;
        let mut reader = BlockReader::default();
        while logical < end {
            let reference = self.reference_for_offset(logical)?;
            self.package
                .inner
                .prefetch_block(&reference, self.record.access, &mut reader)?;
            logical = reference
                .logical_offset
                .checked_add(u64::from(reference.plain_len))
                .ok_or(Error::InvalidRange)?;
        }
        Ok(())
    }

    #[must_use]
    /// Creates an independent sequential [`Read`] and [`Seek`] cursor.
    pub fn cursor(&self) -> AssetCursor {
        AssetCursor {
            asset: self.clone(),
            position: 0,
            current_block: None,
            previous_streaming_block: None,
            reader: BlockReader::default(),
        }
    }

    fn reference_for_offset(&self, offset: u64) -> Result<BlockRef> {
        if self.record.block_count == 0 {
            return Err(Error::InvalidFormat("non-empty file has no blocks"));
        }
        let local_index = match self.record.layout {
            LayoutKind::Fixed => {
                let block_len = u64::from(self.record.fixed_block_len);
                let quotient = offset.checked_div(block_len).unwrap_or(0);
                u32::try_from(quotient).map_err(|_| Error::InvalidRange)?
            }
            LayoutKind::ContentDefined => {
                let mut low = 0_u32;
                let mut high = self.record.block_count;
                while low < high {
                    let mid = low + (high - low) / 2;
                    let reference = self.package.inner.block_ref(
                        self.record
                            .first_block
                            .checked_add(mid)
                            .ok_or(Error::InvalidRange)?,
                    )?;
                    if reference.logical_offset <= offset {
                        low = mid + 1;
                    } else {
                        high = mid;
                    }
                }
                low.saturating_sub(1)
            }
        };
        if local_index >= self.record.block_count {
            return Err(Error::InvalidFormat("file block index"));
        }
        let reference = self.package.inner.block_ref(
            self.record
                .first_block
                .checked_add(local_index)
                .ok_or(Error::InvalidRange)?,
        )?;
        let end = reference
            .logical_offset
            .checked_add(u64::from(reference.plain_len))
            .ok_or(Error::InvalidRange)?;
        if offset < reference.logical_offset || offset >= end {
            return Err(Error::InvalidFormat("block logical coverage"));
        }
        Ok(reference)
    }
}

impl AssetReadSession {
    /// Reads through this persistent session at a logical asset offset.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid offset, unavailable segment, or failed authentication.
    pub fn read_at(&mut self, offset: u64, destination: &mut [u8]) -> Result<usize> {
        self.asset.read_at_buffered(
            offset,
            destination,
            &mut self.current_block,
            &mut self.previous_streaming_block,
            &mut self.reader,
        )
    }
}

impl Read for AssetCursor {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.position > self.asset.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cursor is beyond the end of the asset",
            ));
        }
        let requested =
            usize::try_from((self.asset.len() - self.position).min(buffer.len() as u64))
                .map_err(std::io::Error::other)?;
        let mut written = 0;
        while written < requested {
            let covered = self
                .current_block
                .as_ref()
                .is_some_and(|block| block.covers(self.position));
            if !covered {
                if self
                    .previous_streaming_block
                    .as_ref()
                    .is_some_and(|block| block.covers(self.position))
                {
                    std::mem::swap(&mut self.current_block, &mut self.previous_streaming_block);
                    continue;
                }
                let reference = self
                    .asset
                    .reference_for_offset(self.position)
                    .map_err(std::io::Error::other)?;
                if self.asset.record.access == AccessClass::Streaming {
                    if let Some(previous) = self.previous_streaming_block.take() {
                        self.reader.recycle(previous.data);
                    }
                    self.previous_streaming_block = self.current_block.take();
                } else {
                    if let Some(current) = self.current_block.take() {
                        self.reader.recycle(current.data);
                    }
                }
                let block = self
                    .asset
                    .package
                    .inner
                    .load_block_buffered(&reference, self.asset.record.access, &mut self.reader)
                    .map_err(std::io::Error::other)?;
                self.current_block = Some(CursorBlock {
                    reference,
                    data: block,
                });
            }
            let block = self
                .current_block
                .as_ref()
                .expect("cursor block loaded above");
            let bytes = block.data.as_ref();
            let within = usize::try_from(self.position - block.reference.logical_offset)
                .map_err(std::io::Error::other)?;
            let amount = (bytes.len() - within).min(requested - written);
            buffer[written..written + amount].copy_from_slice(&bytes[within..within + amount]);
            written += amount;
            self.position += amount as u64;
        }
        Ok(written)
    }
}

impl AssetCursor {
    #[must_use]
    /// Returns the underlying asset length.
    pub const fn len(&self) -> u64 {
        self.asset.len()
    }

    #[must_use]
    /// Reports whether the underlying asset is empty.
    pub const fn is_empty(&self) -> bool {
        self.asset.is_empty()
    }

    #[must_use]
    /// Returns the current logical cursor position.
    pub const fn position(&self) -> u64 {
        self.position
    }
}

impl Seek for AssetCursor {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let target = match position {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::End(delta) => i128::from(self.asset.len()) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
        };
        let target = u64::try_from(target)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "negative seek"))?;
        if target > self.asset.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek is beyond the end of the asset",
            ));
        }
        self.position = target;
        Ok(self.position)
    }
}

fn validate_page_counts(map: u32, reuse: u32, expected: u32) -> Result<()> {
    if map.checked_add(reuse) == Some(expected) {
        Ok(())
    } else {
        Err(Error::InvalidFormat(
            "snapshot and catalog page counts differ",
        ))
    }
}

fn checked_page_region_len(relative_offset: u64, stored_len: u32) -> Result<u64> {
    relative_offset
        .checked_add(u64::from(stored_len))
        .ok_or(Error::InvalidFormat("page region length overflow"))
}

fn validate_snapshot_len(actual: u64, page_offset: u64, page_bytes: u64) -> Result<()> {
    let expected = page_offset
        .checked_add(page_bytes)
        .ok_or(Error::InvalidFormat("snapshot length overflow"))?;
    if actual == expected {
        Ok(())
    } else {
        Err(Error::InvalidFormat("snapshot file length"))
    }
}

fn checked_record_index(page: u32, records_per_page: u32) -> Result<u32> {
    page.checked_mul(records_per_page)
        .ok_or(Error::InvalidFormat("reuse record index overflow"))
}

fn validate_record_total(actual: usize, expected: u32) -> Result<()> {
    if actual == expected as usize {
        Ok(())
    } else {
        Err(Error::InvalidFormat("reuse record total"))
    }
}

fn validate_global_block_index(global: u32, total: u32, pages: u32) -> Result<u32> {
    if global >= total {
        return Err(Error::InvalidFormat("global block index"));
    }
    let page = global / BLOCKS_PER_MAP_PAGE as u32;
    if page >= pages {
        Err(Error::InvalidFormat("map page index"))
    } else {
        Ok(page)
    }
}

fn validate_local_block_index(local: usize, count: usize) -> Result<()> {
    if local < count {
        Ok(())
    } else {
        Err(Error::InvalidFormat("map page local index"))
    }
}

fn checked_offset(base: u64, relative: u64) -> Result<u64> {
    base.checked_add(relative).ok_or(Error::InvalidRange)
}

fn validate_digest(bytes: &[u8], expected: &[u8; 32], scope: &'static str) -> Result<()> {
    if blake3::hash(bytes).as_bytes() == expected {
        Ok(())
    } else {
        Err(Error::Authentication(scope))
    }
}

fn validate_complete_read(actual: usize, expected: usize) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::InvalidFormat("short complete asset read"))
    }
}

fn validate_block_coverage(within: usize, block_len: usize) -> Result<()> {
    if within < block_len {
        Ok(())
    } else {
        Err(Error::InvalidFormat(
            "block does not cover requested offset",
        ))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn decode_owned(codec: Codec, stored: Vec<u8>, expected_len: usize) -> Result<Vec<u8>> {
    match codec {
        Codec::Raw => {
            if stored.len() != expected_len {
                return Err(Error::InvalidFormat("RAW plaintext length"));
            }
            Ok(stored)
        }
        Codec::Zstd => decompress_zstd_exact(&stored, expected_len),
    }
}

fn decode_buffered(codec: Codec, reader: &mut BlockReader, expected_len: usize) -> Result<Vec<u8>> {
    match codec {
        Codec::Raw => {
            if reader.ciphertext.len() != expected_len {
                return Err(Error::InvalidFormat("RAW plaintext length"));
            }
            Ok(reader.take_raw())
        }
        Codec::Zstd => {
            let stored = std::mem::take(&mut reader.ciphertext);
            let result = decompress_zstd_exact_into(
                &stored,
                reader.prepare_plaintext(expected_len),
                expected_len,
            );
            reader.ciphertext = stored;
            result?;
            Ok(reader.take_plaintext())
        }
    }
}

fn decompress_zstd_exact(stored: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    let mut output = vec![0_u8; expected_len];
    decompress_zstd_exact_into(stored, &mut output, expected_len)?;
    Ok(output)
}

fn decompress_zstd_exact_into(stored: &[u8], output: &mut [u8], expected_len: usize) -> Result<()> {
    let written = DECOMPRESSOR.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(zstd::bulk::Decompressor::new()?);
        }
        let decompressor = slot.as_mut().expect("decompressor initialized above");
        decompressor.decompress_to_buffer(stored, output)
    });
    let written = written.map_err(Error::Compression)?;
    if written != expected_len {
        return Err(Error::InvalidFormat("zstd plaintext length"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{
        CatalogData, EMPTY_PATH_SLOT, PageRecord, PathSlot, SegmentId, encode_map_page,
        encode_reuse_page,
    };

    struct MemoryFile(Vec<u8>);

    impl PositionedFile for MemoryFile {
        fn len(&self) -> Result<u64> {
            Ok(self.0.len() as u64)
        }

        fn read_exact_at(&self, offset: u64, destination: &mut [u8]) -> Result<()> {
            let start = usize::try_from(offset).map_err(|_| Error::InvalidRange)?;
            let end = start
                .checked_add(destination.len())
                .ok_or(Error::InvalidRange)?;
            destination.copy_from_slice(self.0.get(start..end).ok_or(Error::InvalidRange)?);
            Ok(())
        }
    }

    struct MemorySegments(HashMap<SegmentId, Arc<[u8]>>);

    impl SegmentSource for MemorySegments {
        fn open(&self, id: SegmentId) -> Result<Arc<dyn PositionedFile>> {
            self.0
                .get(&id)
                .map(|bytes| Arc::new(MemoryFile(bytes.to_vec())) as Arc<dyn PositionedFile>)
                .ok_or(Error::SegmentUnavailable(id))
        }
    }

    fn package_with_budget(budget: ResourceBudget) -> (Package, BlockRef) {
        let project_id = ProjectId([1; 16]);
        let root_key = [2; 32];
        let keys = ProjectKeys::new(root_key, project_id);
        let snapshot_salt = [3; 16];
        let snapshot_nonce = [4; 8];
        let snapshot_key_bytes = keys.snapshot_key(&snapshot_salt);
        let snapshot_key = Aes256Key::new(&snapshot_key_bytes).unwrap();
        let segment_header = SegmentHeader {
            project_id,
            segment_uid: [5; 16],
            salt: [6; 16],
            nonce_prefix: [7; 8],
            block_count: 1,
            payload_len: 20,
            file_len: SEGMENT_HEADER_SIZE as u64 + 20,
        };
        let segment_key = Aes256Key::new(&keys.segment_key(&segment_header)).unwrap();
        let mut encrypted_block = b"data".to_vec();
        segment_key
            .seal(
                crypto::nonce(segment_header.nonce_prefix, 0),
                &crypto::block_aad(
                    project_id,
                    &segment_header.segment_uid,
                    0,
                    Codec::Raw,
                    20,
                    4,
                ),
                &mut encrypted_block,
            )
            .unwrap();
        let block_digest = blake3::hash(&encrypted_block);
        let mut cipher_digest = [0; 16];
        cipher_digest.copy_from_slice(&block_digest.as_bytes()[..16]);
        let reference = BlockRef {
            logical_offset: 0,
            segment_ordinal: 0,
            segment_block_ordinal: 0,
            physical_offset: SEGMENT_HEADER_SIZE as u64,
            stored_len: 20,
            plain_len: 4,
            codec: Codec::Raw,
            cipher_digest,
        };
        let reuse = ReuseRecord {
            chunk_id: *blake3::hash(b"data").as_bytes(),
            block: reference,
        };
        let plain_pages = [
            (
                PageKind::BlockMap,
                encode_map_page(0, &[reference]).unwrap(),
            ),
            (PageKind::Reuse, encode_reuse_page(0, &[reuse]).unwrap()),
        ];
        let mut page_region = Vec::new();
        let mut pages = Vec::new();
        for (index, (kind, plain)) in plain_pages.into_iter().enumerate() {
            let nonce_ordinal = index as u32 + 1;
            let relative_offset = page_region.len() as u64;
            let mut encrypted = plain.clone();
            let stored_len = plain.len() as u32 + 16;
            snapshot_key
                .seal(
                    crypto::nonce(snapshot_nonce, nonce_ordinal),
                    &crypto::page_aad(
                        project_id,
                        1,
                        kind,
                        Codec::Raw,
                        nonce_ordinal,
                        stored_len,
                        plain.len() as u32,
                    ),
                    &mut encrypted,
                )
                .unwrap();
            pages.push(PageRecord {
                kind,
                codec: Codec::Raw,
                nonce_ordinal,
                relative_offset,
                stored_len,
                plain_len: plain.len() as u32,
                digest: *blake3::hash(&encrypted).as_bytes(),
            });
            page_region.extend_from_slice(&encrypted);
        }
        let segment_id = SegmentId([8; 32]);
        let segment_record = SegmentRecord {
            id: segment_id,
            uid: segment_header.segment_uid,
            salt: segment_header.salt,
            nonce_prefix: segment_header.nonce_prefix,
            file_len: segment_header.file_len,
            payload_len: segment_header.payload_len,
            block_count: 1,
            availability: Availability::Required,
        };
        let path_key = keys.path_key();
        let hash = u64::from_le_bytes(
            blake3::keyed_hash(&path_key, b"a").as_bytes()[..8]
                .try_into()
                .unwrap(),
        );
        let mut path_slots = vec![
            PathSlot {
                hash: 0,
                file_index: EMPTY_PATH_SLOT,
            };
            2
        ];
        path_slots[hash as usize & 1] = PathSlot {
            hash,
            file_index: 0,
        };
        let catalog = Catalog::parse(Arc::from(
            CatalogData {
                segments: vec![segment_record],
                files: vec![FileRecord {
                    path_offset: 0,
                    path_len: 1,
                    layout: LayoutKind::Fixed,
                    access: AccessClass::Normal,
                    logical_len: 4,
                    first_block: 0,
                    block_count: 1,
                    fixed_block_len: 4,
                }],
                path_slots,
                path_pool: b"a".to_vec(),
                pages,
                total_blocks: 1,
                map_page_count: 1,
                reuse_page_count: 1,
            }
            .encode()
            .unwrap(),
        ))
        .unwrap();
        let mut segment = segment_header.encode().to_vec();
        segment.extend_from_slice(&encrypted_block);
        let source = MemorySegments(HashMap::from([(segment_id, Arc::from(segment))]));
        let header = SnapshotHeader {
            project_id,
            release_sequence: 1,
            catalog_stored_len: 16,
            catalog_plain_len: 1,
            page_region_offset: 0,
            page_count: 2,
            snapshot_salt,
            nonce_prefix: snapshot_nonce,
            signing_key_id: [0; 16],
            source_fingerprint: [0; 32],
            signature: [0; crate::format::SIGNATURE_LEN],
        };
        let package = Package {
            inner: Arc::new(PackageInner {
                snapshot: Arc::new(MemoryFile(page_region)),
                source: Arc::new(source),
                header,
                catalog,
                keys,
                snapshot_key,
                path_key,
                page_cache: Mutex::new(ClockCache::new(budget.map_page_cache_bytes)),
                block_cache: Mutex::new(ClockCache::new(budget.plaintext_cache_bytes)),
                prefetch_cache: Mutex::new(ClockCache::new(budget.prefetch_cache_bytes)),
                probation: Mutex::new(VecDeque::new()),
                handles: Mutex::new(HandleCache::new(budget.idle_segment_handles)),
                budget,
            }),
        };
        (package, reference)
    }

    fn handle() -> Arc<SegmentHandle> {
        Arc::new(SegmentHandle {
            file: Arc::new(MemoryFile(Vec::new())),
            key: Aes256Key::new(&[0; 32]).unwrap(),
        })
    }

    #[test]
    fn handle_cache_obeys_capacity_recency_and_duplicate_rules() {
        let first = SegmentId([1; 32]);
        let second = SegmentId([2; 32]);
        let mut disabled = HandleCache::new(0);
        disabled.insert(first, handle());
        assert!(disabled.get(&first).is_none());

        let mut cache = HandleCache::new(1);
        cache.insert(first, handle());
        cache.insert(first, handle());
        assert!(cache.get(&first).is_some());
        cache.insert(second, handle());
        assert!(cache.get(&first).is_none());
        assert!(cache.get(&second).is_some());
        cache.clear();
        assert!(cache.values.is_empty());

        cache.values.insert(first, handle());
        cache.order.clear();
        cache.insert(second, handle());
        assert_eq!(cache.values.len(), 2);

        let handle = handle();
        assert_eq!(handle.file.len().unwrap(), 0);
        handle.file.read_exact_at(0, &mut []).unwrap();
    }

    #[test]
    fn block_reader_retains_its_active_segment_across_global_trim() {
        let (package, _) = package_with_budget(ResourceBudget::default());
        let record = package.inner.catalog.segment(0).unwrap();
        let mut reader = BlockReader::default();

        let first = reader.segment_handle(&package.inner, 0, &record).unwrap();
        package.trim();
        let retained = reader.segment_handle(&package.inner, 0, &record).unwrap();

        assert!(Arc::ptr_eq(&first, &retained));
    }

    #[test]
    fn decode_helpers_enforce_exact_plaintext_lengths() {
        let shared = Arc::new(b"shared".to_vec());
        assert!(Arc::ptr_eq(
            &BlockData::Shared(Arc::clone(&shared)).into_shared(),
            &shared
        ));
        assert_eq!(
            decode_owned(Codec::Raw, b"raw".to_vec(), 3).unwrap(),
            b"raw"
        );
        assert!(decode_owned(Codec::Raw, b"raw".to_vec(), 2).is_err());
        let compressed = zstd::bulk::compress(b"compressed", 1).unwrap();
        assert_eq!(
            decode_owned(Codec::Zstd, compressed.clone(), 10).unwrap(),
            b"compressed"
        );
        assert!(decode_owned(Codec::Zstd, compressed, 9).is_err());
        let compressed = zstd::bulk::compress(b"compressed", 1).unwrap();
        assert!(decode_owned(Codec::Zstd, compressed, 11).is_err());
        assert!(decode_owned(Codec::Zstd, b"not-zstd".to_vec(), 10).is_err());
        let mut reader = BlockReader {
            ciphertext: b"raw".to_vec(),
            ..BlockReader::default()
        };
        assert!(decode_buffered(Codec::Raw, &mut reader, 2).is_err());
    }

    #[test]
    fn runtime_defenses_reject_invalid_block_and_segment_metadata() {
        let (package, reference) = package_with_budget(ResourceBudget::default());
        assert_eq!(package.asset("a").unwrap().read().unwrap(), b"data");
        assert_eq!(package.reuse_records().unwrap().len(), 1);
        assert!(package.inner.block_ref(1).is_err());
        lock(&package.inner.page_cache).clear();
        assert!(package.inner.load_page(0, PageKind::Reuse).is_err());

        lock(&package.inner.block_cache).insert(
            BlockKey {
                segment_ordinal: 0,
                block_ordinal: 0,
            },
            Arc::new(b"bad".to_vec()),
        );
        assert!(
            package
                .inner
                .load_block(&reference, AccessClass::Hot)
                .is_err()
        );
        lock(&package.inner.block_cache).clear();
        lock(&package.inner.prefetch_cache).insert(
            BlockKey {
                segment_ordinal: 0,
                block_ordinal: 0,
            },
            Arc::new(b"bad".to_vec()),
        );
        assert!(
            package
                .inner
                .load_block(&reference, AccessClass::Transient)
                .is_err()
        );
        lock(&package.inner.prefetch_cache).clear();

        let mut invalid = reference;
        invalid.segment_block_ordinal = 1;
        assert!(
            package
                .inner
                .load_block(&invalid, AccessClass::Transient)
                .is_err()
        );
        let mut invalid = reference;
        invalid.physical_offset = 0;
        assert!(
            package
                .inner
                .load_block(&invalid, AccessClass::Transient)
                .is_err()
        );
        let mut invalid = reference;
        invalid.physical_offset = u64::MAX;
        assert!(
            package
                .inner
                .load_block(&invalid, AccessClass::Transient)
                .is_err()
        );

        assert!(!package.inner.second_normal_access(BlockKey {
            segment_ordinal: 0,
            block_ordinal: 7,
        }));
        assert!(package.inner.second_normal_access(BlockKey {
            segment_ordinal: 0,
            block_ordinal: 7,
        }));

        let (limited, _) = package_with_budget(ResourceBudget {
            normal_probation_entries: 1,
            ..ResourceBudget::cache_disabled()
        });
        assert!(!limited.inner.second_normal_access(BlockKey {
            segment_ordinal: 0,
            block_ordinal: 1,
        }));
        assert!(!limited.inner.second_normal_access(BlockKey {
            segment_ordinal: 0,
            block_ordinal: 2,
        }));

        let record = package.segment_record(0).unwrap();
        let (mut bad_length, _) = package_with_budget(ResourceBudget::cache_disabled());
        Arc::get_mut(&mut bad_length.inner).unwrap().source = Arc::new(MemorySegments(
            HashMap::from([(record.id, Arc::from(vec![0; 1]))]),
        ));
        assert!(bad_length.inner.open_segment(&record).is_err());

        let (mut bad_header, _) = package_with_budget(ResourceBudget::cache_disabled());
        let header = SegmentHeader {
            project_id: ProjectId([9; 16]),
            segment_uid: record.uid,
            salt: record.salt,
            nonce_prefix: record.nonce_prefix,
            block_count: record.block_count,
            payload_len: record.payload_len,
            file_len: record.file_len,
        };
        let mut bytes = header.encode().to_vec();
        bytes.resize(record.file_len as usize, 0);
        Arc::get_mut(&mut bad_header.inner).unwrap().source = Arc::new(MemorySegments(
            HashMap::from([(record.id, Arc::from(bytes))]),
        ));
        assert!(bad_header.inner.open_segment(&record).is_err());
    }

    #[test]
    fn asset_and_cursor_defenses_cover_invalid_ranges_and_layouts() {
        let (package, _) = package_with_budget(ResourceBudget::cache_disabled());
        let asset = package.asset("a").unwrap();
        assert!(asset.read_at(5, &mut [0; 1]).is_err());
        let mut empty_blocks = asset.clone();
        empty_blocks.record.block_count = 0;
        assert!(empty_blocks.reference_for_offset(0).is_err());
        let mut too_few_blocks = asset.clone();
        too_few_blocks.record.logical_len = 8;
        assert!(too_few_blocks.reference_for_offset(4).is_err());
        let mut cursor = asset.cursor();
        let mut byte = [0];
        assert_eq!(cursor.read(&mut byte).unwrap(), 1);
        cursor.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(cursor.read(&mut byte).unwrap(), 0);
        assert!(cursor.seek(SeekFrom::Current(-5)).is_err());
        cursor.position = asset.len() + 1;
        assert!(cursor.read(&mut byte).is_err());
    }

    #[test]
    fn snapshot_and_index_validators_cover_all_failure_modes() {
        validate_page_counts(1, 1, 2).unwrap();
        assert!(validate_page_counts(u32::MAX, 1, 0).is_err());
        assert!(validate_page_counts(1, 1, 1).is_err());
        assert_eq!(checked_page_region_len(1, 2).unwrap(), 3);
        assert!(checked_page_region_len(u64::MAX, 1).is_err());
        validate_snapshot_len(3, 1, 2).unwrap();
        assert!(validate_snapshot_len(4, 1, 2).is_err());
        assert!(validate_snapshot_len(0, u64::MAX, 1).is_err());
        assert_eq!(checked_record_index(2, 3).unwrap(), 6);
        assert!(checked_record_index(u32::MAX, 2).is_err());
        validate_record_total(2, 2).unwrap();
        assert!(validate_record_total(1, 2).is_err());
        assert_eq!(validate_global_block_index(0, 1, 1).unwrap(), 0);
        assert!(validate_global_block_index(1, 1, 1).is_err());
        assert!(validate_global_block_index(0, 1, 0).is_err());
        validate_local_block_index(0, 1).unwrap();
        assert!(validate_local_block_index(1, 1).is_err());
        assert_eq!(checked_offset(1, 2).unwrap(), 3);
        assert!(checked_offset(u64::MAX, 1).is_err());
        let digest = *blake3::hash(b"page").as_bytes();
        validate_digest(b"page", &digest, "page").unwrap();
        assert!(validate_digest(b"other", &digest, "page").is_err());
        validate_complete_read(1, 1).unwrap();
        assert!(validate_complete_read(0, 1).is_err());
        validate_block_coverage(0, 1).unwrap();
        assert!(validate_block_coverage(1, 1).is_err());
    }
}
