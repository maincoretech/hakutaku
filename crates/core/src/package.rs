use crate::cache::ClockCache;
use crate::crypto::{self, Aes256Key, ProjectKeys};
use crate::format::{
    AccessClass, BLOCKS_PER_MAP_PAGE, BlockRef, Catalog, Codec, FileRecord, LayoutKind, PageKind,
    ProjectId, REUSE_PER_PAGE, ReuseRecord, SEGMENT_HEADER_SIZE, SegmentHeader, SegmentId,
    SegmentRecord, SnapshotHeader, map_page_record, parse_reuse_page, validate_map_page,
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
    pub map_page_cache_bytes: usize,
    pub plaintext_cache_bytes: usize,
    pub idle_segment_handles: usize,
    pub normal_probation_entries: usize,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            map_page_cache_bytes: 2 * 1024 * 1024,
            plaintext_cache_bytes: 64 * 1024 * 1024,
            idle_segment_handles: 16,
            normal_probation_entries: 256,
        }
    }
}

impl ResourceBudget {
    #[must_use]
    pub const fn memory_constrained() -> Self {
        Self {
            map_page_cache_bytes: 512 * 1024,
            plaintext_cache_bytes: 16 * 1024 * 1024,
            idle_segment_handles: 4,
            normal_probation_entries: 64,
        }
    }

    #[must_use]
    pub const fn cache_disabled() -> Self {
        Self {
            map_page_cache_bytes: 0,
            plaintext_cache_bytes: 0,
            idle_segment_handles: 0,
            normal_probation_entries: 0,
        }
    }
}

#[derive(Clone)]
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
pub struct AssetInfo {
    pub path: String,
    pub len: u64,
    pub access: AccessClass,
}

#[derive(Clone)]
pub struct Asset {
    package: Package,
    file_index: u32,
    record: FileRecord,
}

pub struct AssetCursor {
    asset: Asset,
    position: u64,
    current_block: Option<(BlockRef, Arc<[u8]>)>,
}

impl Package {
    pub fn open(
        snapshot: Arc<dyn PositionedFile>,
        source: Arc<dyn SegmentSource>,
        root_key: [u8; 32],
        verifying_key: [u8; 32],
        budget: ResourceBudget,
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
        let catalog_plain_len = usize::try_from(header.catalog_plain_len)
            .map_err(|_| Error::LimitExceeded("catalog plaintext length"))?;
        let catalog_bytes = decompress_zstd_exact(&catalog_ciphertext, catalog_plain_len)?;
        let catalog = Catalog::parse(Arc::from(catalog_bytes))?;
        if catalog
            .map_page_count()
            .checked_add(catalog.reuse_page_count())
            != Some(header.page_count)
        {
            return Err(Error::InvalidFormat(
                "snapshot and catalog page counts differ",
            ));
        }
        let page_bytes = if header.page_count == 0 {
            0
        } else {
            let last = catalog.page(header.page_count - 1)?;
            last.relative_offset
                .checked_add(u64::from(last.stored_len))
                .ok_or(Error::InvalidFormat("page region length overflow"))?
        };
        let expected_snapshot_len = header
            .page_region_offset
            .checked_add(page_bytes)
            .ok_or(Error::InvalidFormat("snapshot length overflow"))?;
        if snapshot.len()? != expected_snapshot_len {
            return Err(Error::InvalidFormat("snapshot file length"));
        }
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
                probation: Mutex::new(VecDeque::with_capacity(budget.normal_probation_entries)),
                handles: Mutex::new(HandleCache::new(budget.idle_segment_handles)),
                budget,
            }),
        })
    }

    pub fn open_directory(
        snapshot_path: impl AsRef<Path>,
        segment_directory: impl AsRef<Path>,
        root_key: [u8; 32],
        verifying_key: [u8; 32],
        budget: ResourceBudget,
    ) -> Result<Self> {
        Self::open(
            Arc::new(LocalFile::open(snapshot_path)?),
            Arc::new(DirectorySegmentSource::new(
                segment_directory.as_ref().to_path_buf(),
            )),
            root_key,
            verifying_key,
            budget,
        )
    }

    #[must_use]
    pub fn project_id(&self) -> ProjectId {
        self.inner.header.project_id
    }

    #[must_use]
    pub fn release_sequence(&self) -> u64 {
        self.inner.header.release_sequence
    }

    #[must_use]
    pub fn source_fingerprint(&self) -> [u8; 32] {
        self.inner.header.source_fingerprint
    }

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

    /// Segment identities referenced by this complete snapshot.
    pub fn segment_ids(&self) -> Result<Vec<SegmentId>> {
        (0..self.inner.catalog.segment_count())
            .map(|index| self.inner.catalog.segment(index).map(|record| record.id))
            .collect()
    }

    pub fn trim(&self) {
        lock(&self.inner.page_cache).clear();
        lock(&self.inner.block_cache).clear();
        lock(&self.inner.probation).clear();
        lock(&self.inner.handles).clear();
    }

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
            let expected_first = reuse_index
                .checked_mul(REUSE_PER_PAGE as u32)
                .ok_or(Error::InvalidFormat("reuse record index overflow"))?;
            records.extend(parse_reuse_page(&bytes, expected_first)?);
        }
        if records.len() != self.inner.catalog.total_blocks() as usize {
            return Err(Error::InvalidFormat("reuse record total"));
        }
        Ok(records)
    }

    pub fn segment_record(&self, ordinal: u16) -> Result<SegmentRecord> {
        self.inner.catalog.segment(u32::from(ordinal))
    }
}

impl PackageInner {
    fn block_ref(&self, global_index: u32) -> Result<BlockRef> {
        if global_index >= self.catalog.total_blocks() {
            return Err(Error::InvalidFormat("global block index"));
        }
        let page_index = global_index / BLOCKS_PER_MAP_PAGE as u32;
        if page_index >= self.catalog.map_page_count() {
            return Err(Error::InvalidFormat("map page index"));
        }
        let expected_first = page_index * BLOCKS_PER_MAP_PAGE as u32;
        let bytes = self.load_page(page_index, PageKind::BlockMap)?;
        let count = validate_map_page(&bytes, expected_first)?;
        let local_index = (global_index - expected_first) as usize;
        if local_index >= count {
            return Err(Error::InvalidFormat("map page local index"));
        }
        map_page_record(&bytes, local_index)
    }

    fn load_page(&self, page_index: u32, expected_kind: PageKind) -> Result<Arc<[u8]>> {
        if let Some(value) = lock(&self.page_cache).get(&page_index) {
            return Ok(value);
        }
        let page = self.catalog.page(page_index)?;
        if page.kind != expected_kind {
            return Err(Error::InvalidFormat("page kind at use site"));
        }
        let offset = self
            .header
            .page_region_offset
            .checked_add(page.relative_offset)
            .ok_or(Error::InvalidRange)?;
        let mut ciphertext = vec![0_u8; page.stored_len as usize];
        self.snapshot.read_exact_at(offset, &mut ciphertext)?;
        if &blake3::hash(&ciphertext).as_bytes()[..] != page.digest.as_slice() {
            return Err(Error::Authentication("signed page digest"));
        }
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
        let decoded = decode(page.codec, &ciphertext, page.plain_len as usize)?;
        let decoded: Arc<[u8]> = Arc::from(decoded);
        lock(&self.page_cache).insert(page_index, Arc::clone(&decoded));
        Ok(decoded)
    }

    fn load_block(&self, reference: &BlockRef, access: AccessClass) -> Result<Arc<[u8]>> {
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
            return Ok(value);
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
        let handle = self.open_segment(&segment)?;
        let mut ciphertext = vec![0_u8; reference.stored_len as usize];
        handle
            .file
            .read_exact_at(reference.physical_offset, &mut ciphertext)?;
        if blake3::hash(&ciphertext).as_bytes()[..16] != reference.cipher_digest {
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
            &mut ciphertext,
            "segment block",
        )?;
        let plaintext: Arc<[u8]> = Arc::from(decode(
            reference.codec,
            &ciphertext,
            reference.plain_len as usize,
        )?);

        let admit = match access {
            AccessClass::Hot => true,
            AccessClass::Normal => self.second_normal_access(key),
            AccessClass::Streaming | AccessClass::Transient => false,
        };
        if admit {
            lock(&self.block_cache).insert(key, Arc::clone(&plaintext));
        }
        Ok(plaintext)
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
    pub const fn len(&self) -> u64 {
        self.record.logical_len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.record.logical_len == 0
    }

    #[must_use]
    pub const fn file_index(&self) -> u32 {
        self.file_index
    }

    pub fn read(&self) -> Result<Vec<u8>> {
        let len = usize::try_from(self.len()).map_err(|_| Error::LimitExceeded("asset length"))?;
        let mut result = vec![0_u8; len];
        let read = self.read_at(0, &mut result)?;
        if read != len {
            return Err(Error::InvalidFormat("short complete asset read"));
        }
        Ok(result)
    }

    pub fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<usize> {
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
            let reference = self.reference_for_offset(logical)?;
            let block = self
                .package
                .inner
                .load_block(&reference, self.record.access)?;
            let within = usize::try_from(logical - reference.logical_offset)
                .map_err(|_| Error::InvalidFormat("block logical offset"))?;
            if within >= block.len() {
                return Err(Error::InvalidFormat(
                    "block does not cover requested offset",
                ));
            }
            let amount = (block.len() - within).min(requested - written);
            destination[written..written + amount].copy_from_slice(&block[within..within + amount]);
            written += amount;
            logical += amount as u64;
        }
        Ok(written)
    }

    #[must_use]
    pub fn cursor(&self) -> AssetCursor {
        AssetCursor {
            asset: self.clone(),
            position: 0,
            current_block: None,
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
            let covered = self.current_block.as_ref().is_some_and(|(reference, _)| {
                let end = reference
                    .logical_offset
                    .saturating_add(u64::from(reference.plain_len));
                self.position >= reference.logical_offset && self.position < end
            });
            if !covered {
                let reference = self
                    .asset
                    .reference_for_offset(self.position)
                    .map_err(std::io::Error::other)?;
                let block = self
                    .asset
                    .package
                    .inner
                    .load_block(&reference, self.asset.record.access)
                    .map_err(std::io::Error::other)?;
                self.current_block = Some((reference, block));
            }
            let Some((reference, block)) = self.current_block.as_ref() else {
                return Err(std::io::Error::other("cursor block was not loaded"));
            };
            let within = usize::try_from(self.position - reference.logical_offset)
                .map_err(std::io::Error::other)?;
            let amount = (block.len() - within).min(requested - written);
            buffer[written..written + amount].copy_from_slice(&block[within..within + amount]);
            written += amount;
            self.position += amount as u64;
        }
        Ok(written)
    }
}

impl Seek for AssetCursor {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let target = match position {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::End(delta) => i128::from(self.asset.len()) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
        };
        self.position = u64::try_from(target)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "negative seek"))?;
        if self.current_block.as_ref().is_some_and(|(reference, _)| {
            let end = reference
                .logical_offset
                .saturating_add(u64::from(reference.plain_len));
            self.position < reference.logical_offset || self.position >= end
        }) {
            self.current_block = None;
        }
        Ok(self.position)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn decode(codec: Codec, stored: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    match codec {
        Codec::Raw => {
            if stored.len() != expected_len {
                return Err(Error::InvalidFormat("RAW plaintext length"));
            }
            Ok(stored.to_vec())
        }
        Codec::Zstd => decompress_zstd_exact(stored, expected_len),
    }
}

fn decompress_zstd_exact(stored: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    let mut output = vec![0_u8; expected_len];
    let written = DECOMPRESSOR.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(zstd::bulk::Decompressor::new()?);
        }
        let Some(decompressor) = slot.as_mut() else {
            return Err(std::io::Error::other(
                "zstd decompressor initialization failed",
            ));
        };
        decompressor.decompress_to_buffer(stored, &mut output)
    });
    let written = written.map_err(Error::Compression)?;
    if written != expected_len {
        return Err(Error::InvalidFormat("zstd plaintext length"));
    }
    Ok(output)
}
