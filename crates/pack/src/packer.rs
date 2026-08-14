use crate::build_cache::{self, BuildCache, CachedChunk, CachedEntry};
use crate::source::{SourceFile, classify, collect_files};
use crate::{Error, Identity, Result};
use hakutaku_core::crypto::{self, Aes256Key, ProjectKeys};
use hakutaku_core::format::{
    AccessClass, Availability, BLOCKS_PER_MAP_PAGE, BlockRef, CatalogData, Codec, EMPTY_PATH_SLOT,
    FileRecord, LayoutKind, PageKind, PageRecord, PathSlot, ProjectId, REUSE_PER_PAGE, ReuseRecord,
    SEGMENT_HEADER_SIZE, SegmentHeader, SegmentId, SegmentRecord, SnapshotHeader, encode_map_page,
    encode_reuse_page, validate_canonical_path,
};
use hakutaku_core::{Package, ResourceBudget, SEGMENT_FILE_EXTENSION, segment_file_name};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const FASTCDC_MIN: u32 = 32 * 1024;
const FASTCDC_AVG: u32 = 128 * 1024;
const FASTCDC_MAX: u32 = 512 * 1024;
const COMPRESSION_SAVINGS: usize = 64;
const DEFAULT_SEGMENT_TARGET: u64 = 512 * 1024 * 1024;
const HOT_SEGMENT_TARGET: u64 = 64 * 1024 * 1024;
const NORMAL_SEGMENT_TARGET: u64 = 256 * 1024 * 1024;
const TRANSIENT_SEGMENT_TARGET: u64 = 128 * 1024 * 1024;
const BUILD_CACHE_FILE: &str = ".hakutaku-build-cache";

#[derive(Clone, Debug)]
/// Inputs and policy controls for one deterministic package build.
pub struct PackOptions {
    /// Directory recursively containing canonical source assets.
    pub input_directory: PathBuf,
    /// Publisher output directory containing `game.haku` and `data/`.
    pub output_directory: PathBuf,
    /// Whether unchanged encrypted blocks may be reused from the previous release.
    pub incremental: bool,
    /// Trusts a local size/mtime/file-identity cache to skip unchanged source
    /// bodies. Intended only for development builds; final builds should leave
    /// this disabled to retain byte-for-byte source verification.
    pub development_cache: bool,
    /// Zstandard compression level used when compression is canonical and beneficial.
    pub compression_level: i32,
    /// Approximate maximum payload bytes written to one new segment.
    pub segment_target_bytes: u64,
    /// Canonical asset paths or directory prefixes whose segments may be
    /// installed on demand. Required and deferred blocks are never mixed in
    /// one segment.
    pub deferred_prefixes: Vec<String>,
}

impl PackOptions {
    #[must_use]
    /// Creates options with safe incremental-build defaults.
    pub fn new(input_directory: impl Into<PathBuf>, output_directory: impl Into<PathBuf>) -> Self {
        Self {
            input_directory: input_directory.into(),
            output_directory: output_directory.into(),
            incremental: true,
            development_cache: false,
            compression_level: 3,
            segment_target_bytes: DEFAULT_SEGMENT_TARGET,
            deferred_prefixes: Vec::new(),
        }
    }

    pub(crate) fn availability(&self, path: &str) -> Availability {
        if self.deferred_prefixes.iter().any(|prefix| {
            path == prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            Availability::Deferred
        } else {
            Availability::Required
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
/// Current phase and byte progress reported during a package build.
pub struct PackProgress {
    /// Stable human-readable build phase.
    pub phase: &'static str,
    /// Canonical asset path currently being processed, when applicable.
    pub current_path: Option<String>,
    /// Input bytes processed during the active phase.
    pub completed_bytes: u64,
    /// Total input bytes expected during the active phase.
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
/// Summary of the immutable release produced by a build.
pub struct PackReport {
    /// Whether the canonical source fingerprint differed from the prior release.
    pub changed: bool,
    /// Monotonic sequence of the resulting snapshot.
    pub release_sequence: u64,
    /// Number of indexed source files.
    pub file_count: u32,
    /// Number of content blocks across all files.
    pub block_count: u32,
    /// Blocks reused byte-for-byte from older immutable segments.
    pub reused_blocks: u32,
    /// Blocks encrypted into newly created segments.
    pub new_blocks: u32,
    /// Newly created immutable segment files.
    pub new_segments: u32,
    /// Complete byte size of newly created segment files.
    pub new_segment_bytes: u64,
    /// Total bytes occupied by every segment retained by this release.
    pub retained_segment_bytes: u64,
    /// Unique encrypted block bytes referenced by this release.
    pub referenced_block_bytes: u64,
    /// Payload bytes retained only because a live block shares their segment.
    pub stranded_segment_bytes: u64,
}

/// Builds a package without progress notifications.
///
/// # Errors
///
/// Returns an error for invalid options, filesystem failures, or cryptographic failures.
pub fn pack_directory(options: &PackOptions, identity: &Identity) -> Result<PackReport> {
    pack_directory_with_progress(options, identity, |_| {})
}

/// Builds a package and reports synchronous phase progress to `progress`.
///
/// # Errors
///
/// Returns an error for invalid options, filesystem failures, or cryptographic failures.
pub fn pack_directory_with_progress<F>(
    options: &PackOptions,
    identity: &Identity,
    mut progress: F,
) -> Result<PackReport>
where
    F: FnMut(PackProgress),
{
    validate_options(options)?;
    validate_identity_location(options, identity)?;
    std::fs::create_dir_all(&options.output_directory)?;
    let _lock = BuildLock::acquire(&options.output_directory)?;
    recover_interrupted_release(&options.output_directory)?;
    let data_directory = options.output_directory.join("data");
    std::fs::create_dir_all(&data_directory)?;

    let files = collect_files(&options.input_directory)?;
    let total_bytes = files.iter().try_fold(0_u64, |total, file| {
        total
            .checked_add(file.len)
            .ok_or_else(|| Error::InvalidInput("input size overflow".into()))
    })?;
    progress(PackProgress {
        phase: "Indexing",
        current_path: None,
        completed_bytes: 0,
        total_bytes,
    });

    let snapshot_path = options.output_directory.join("game.haku");
    let old_package = if options.incremental && snapshot_path.is_file() {
        Some(Package::open_directory(
            &snapshot_path,
            &data_directory,
            identity.root_key(),
            identity.public_key(),
            ResourceBudget::memory_constrained(),
        )?)
    } else {
        None
    };
    let build_cache = if options.development_cache {
        old_package
            .as_ref()
            .map(|package| {
                BuildCache::load(
                    &options.output_directory.join(BUILD_CACHE_FILE),
                    identity.project_id(),
                    package.release_sequence(),
                )
            })
            .transpose()?
            .flatten()
    } else {
        None
    };
    let reuse = load_reuse_index(old_package.as_ref())?;
    let release_sequence = old_package.as_ref().map_or(Ok(1), |package| {
        package
            .release_sequence()
            .checked_add(1)
            .ok_or_else(|| Error::InvalidInput("release sequence is exhausted".into()))
    })?;

    let keys = ProjectKeys::new(identity.root_key(), identity.project_id());
    let mut compressor = zstd::bulk::Compressor::new(options.compression_level)?;
    compressor.include_checksum(false)?;
    compressor.include_dictid(false)?;
    let mut context = BuildContext {
        options,
        identity,
        keys: &keys,
        reuse_index: reuse.blocks,
        reuse_segments: reuse.segments,
        current_reuse: HashMap::new(),
        used_existing_segments: BTreeMap::new(),
        new_segment_records: Vec::new(),
        active_segments: Vec::new(),
        pending_blocks: Vec::new(),
        chunk_ids: Vec::new(),
        file_records: Vec::with_capacity(files.len()),
        file_paths: Vec::with_capacity(files.len()),
        path_pool: Vec::new(),
        fingerprint: blake3::Hasher::new(),
        compressor,
        reused_blocks: 0,
        new_blocks: 0,
        new_segment_bytes: 0,
        completed_bytes: 0,
    };
    context
        .fingerprint
        .update(b"Hakutaku source fingerprint v1");
    context
        .fingerprint
        .update(&(files.len() as u64).to_le_bytes());

    for source in &files {
        progress(PackProgress {
            phase: "Packing",
            current_path: Some(source.logical_path.clone()),
            completed_bytes: context.completed_bytes,
            total_bytes,
        });
        context.add_file(
            source,
            build_cache
                .as_ref()
                .and_then(|cache| cache.get(&source.logical_path)),
        )?;
    }
    context.finish_active_segments()?;
    if !context.new_segment_records.is_empty() {
        sync_directory(&data_directory)?;
    }
    let source_fingerprint = *context.fingerprint.finalize().as_bytes();

    if old_package
        .as_ref()
        .is_some_and(|old| old.source_fingerprint() == source_fingerprint)
    {
        let old = old_package.as_ref().expect("checked above");
        let active_segments = old.segment_ids()?;
        let active_release_sequence = old.release_sequence();
        let storage = package_storage_stats(old)?;
        if options.development_cache {
            save_build_cache(options, identity, active_release_sequence, &files, &context)?;
        }
        drop(old_package);
        cleanup_unreferenced_segments(&data_directory, &active_segments)?;
        return Ok(PackReport {
            changed: false,
            release_sequence: active_release_sequence,
            file_count: u32::try_from(files.len())
                .map_err(|_| Error::InvalidInput("too many files".into()))?,
            block_count: u32::try_from(context.pending_blocks.len())
                .map_err(|_| Error::InvalidInput("too many blocks".into()))?,
            reused_blocks: context.reused_blocks,
            new_blocks: 0,
            new_segments: 0,
            new_segment_bytes: 0,
            retained_segment_bytes: storage.retained,
            referenced_block_bytes: storage.referenced,
            stranded_segment_bytes: storage.stranded,
        });
    }

    progress(PackProgress {
        phase: "Writing snapshot",
        current_path: None,
        completed_bytes: total_bytes,
        total_bytes,
    });
    let report = context.write_snapshot(release_sequence, source_fingerprint, &snapshot_path)?;

    progress(PackProgress {
        phase: "Verifying",
        current_path: None,
        completed_bytes: total_bytes,
        total_bytes,
    });
    let staged_snapshot = snapshot_path.with_extension(format!("haku.part-{}", std::process::id()));
    if let Err(error) = verify_staged_release(
        &staged_snapshot,
        &data_directory,
        &files,
        identity,
        !options.development_cache,
    ) {
        let _ = std::fs::remove_file(&staged_snapshot);
        return Err(error);
    }
    if options.development_cache {
        save_build_cache(options, identity, report.release_sequence, &files, &context)?;
    }
    if let Err(error) = commit_snapshot(&snapshot_path) {
        let _ = std::fs::remove_file(&staged_snapshot);
        return Err(error);
    }
    let active_segments = context.active_segment_ids();
    drop(old_package);
    cleanup_unreferenced_segments(&data_directory, &active_segments)?;
    progress(PackProgress {
        phase: "Complete",
        current_path: None,
        completed_bytes: total_bytes,
        total_bytes,
    });
    Ok(report)
}

#[derive(Clone)]
struct ReusableBlock {
    block: BlockRef,
    segment_ordinal: u16,
}

struct ReuseIndex {
    blocks: HashMap<([u8; 32], Availability), ReusableBlock>,
    segments: Vec<SegmentRecord>,
}

#[derive(Clone, Copy)]
enum SegmentLocator {
    Existing(SegmentId),
    New(usize),
}

#[derive(Clone, Copy)]
struct PendingBlock {
    locator: SegmentLocator,
    reference: BlockRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SegmentClass {
    availability: Availability,
    access: AccessClass,
}

impl SegmentClass {
    const fn new(availability: Availability, access: AccessClass) -> Self {
        Self {
            availability,
            access,
        }
    }

    const fn target_bytes(self, configured: u64) -> u64 {
        let class_target = match self.access {
            AccessClass::Hot => HOT_SEGMENT_TARGET,
            AccessClass::Normal => NORMAL_SEGMENT_TARGET,
            AccessClass::Streaming => configured,
            AccessClass::Transient => TRANSIENT_SEGMENT_TARGET,
        };
        if configured < class_target {
            configured
        } else {
            class_target
        }
    }
}

struct ActiveSegment {
    class: SegmentClass,
    index: usize,
    writer: NewSegment,
}

struct BuildContext<'a> {
    options: &'a PackOptions,
    identity: &'a Identity,
    keys: &'a ProjectKeys,
    reuse_index: HashMap<([u8; 32], Availability), ReusableBlock>,
    reuse_segments: Vec<SegmentRecord>,
    current_reuse: HashMap<([u8; 32], SegmentClass), PendingBlock>,
    used_existing_segments: BTreeMap<SegmentId, SegmentRecord>,
    new_segment_records: Vec<Option<SegmentRecord>>,
    active_segments: Vec<ActiveSegment>,
    pending_blocks: Vec<PendingBlock>,
    chunk_ids: Vec<[u8; 32]>,
    file_records: Vec<FileRecord>,
    file_paths: Vec<String>,
    path_pool: Vec<u8>,
    fingerprint: blake3::Hasher,
    compressor: zstd::bulk::Compressor<'static>,
    reused_blocks: u32,
    new_blocks: u32,
    new_segment_bytes: u64,
    completed_bytes: u64,
}

impl BuildContext<'_> {
    fn active_segment_ids(&self) -> Vec<SegmentId> {
        self.used_existing_segments
            .keys()
            .copied()
            .chain(
                self.new_segment_records
                    .iter()
                    .flatten()
                    .map(|record| record.id),
            )
            .collect()
    }

    fn add_file(&mut self, source: &SourceFile, cached: Option<&CachedEntry>) -> Result<()> {
        let (layout, fixed_block_len, access) = classify(source);
        let availability = self.options.availability(&source.logical_path);
        let first_block = u32::try_from(self.pending_blocks.len())
            .map_err(|_| Error::InvalidInput("too many blocks".into()))?;
        let path_offset = u32::try_from(self.path_pool.len())
            .map_err(|_| Error::InvalidInput("path pool is too large".into()))?;
        let path_len = checked_path_len(&source.logical_path)?;
        self.path_pool
            .extend_from_slice(source.logical_path.as_bytes());
        self.file_paths.push(source.logical_path.clone());

        self.fingerprint
            .update(&(source.logical_path.len() as u64).to_le_bytes());
        self.fingerprint.update(source.logical_path.as_bytes());
        self.fingerprint.update(&source.len.to_le_bytes());
        self.fingerprint
            .update(&[layout as u8, access as u8, availability as u8]);
        self.fingerprint.update(&fixed_block_len.to_le_bytes());

        let before = self.pending_blocks.len();
        if source.len > 0 {
            if let Some(cached) = cached.filter(|cached| {
                self.cached_entry_usable(
                    source,
                    cached,
                    layout,
                    fixed_block_len,
                    availability,
                    access,
                )
            }) {
                for chunk in &cached.chunks {
                    self.record_chunk(chunk.chunk_id);
                    if !self.try_reuse_chunk(
                        chunk.logical_offset,
                        chunk.plain_len,
                        chunk.chunk_id,
                        availability,
                        access,
                    )? {
                        return Err(Error::InvalidInput(
                            "development cache lost a reusable block".into(),
                        ));
                    }
                }
            } else {
                match layout {
                    LayoutKind::Fixed => {
                        self.add_fixed_file(source, fixed_block_len as usize, availability, access)?
                    }
                    LayoutKind::ContentDefined => {
                        self.add_content_defined_file(source, availability, access)?
                    }
                }
            }
        }
        let block_count = u32::try_from(self.pending_blocks.len() - before)
            .map_err(|_| Error::InvalidInput("file has too many blocks".into()))?;
        self.fingerprint.update(&block_count.to_le_bytes());
        self.file_records.push(FileRecord {
            path_offset,
            path_len,
            layout,
            access,
            logical_len: source.len,
            first_block,
            block_count,
            fixed_block_len,
        });
        self.completed_bytes = self
            .completed_bytes
            .checked_add(source.len)
            .ok_or_else(|| Error::InvalidInput("input size overflow".into()))?;
        Ok(())
    }

    fn cached_entry_usable(
        &self,
        source: &SourceFile,
        cached: &CachedEntry,
        layout: LayoutKind,
        fixed_block_len: u32,
        availability: Availability,
        access: AccessClass,
    ) -> bool {
        if cached.stamp != source.stamp()
            || cached.layout != layout
            || cached.fixed_block_len != fixed_block_len
            || cached.availability != availability
            || cached.access != access
        {
            return false;
        }
        let mut expected_offset = 0_u64;
        for chunk in &cached.chunks {
            if chunk.logical_offset != expected_offset || chunk.plain_len == 0 {
                return false;
            }
            expected_offset = match expected_offset.checked_add(u64::from(chunk.plain_len)) {
                Some(offset) => offset,
                None => return false,
            };
            let class = SegmentClass::new(availability, access);
            let current = self
                .current_reuse
                .get(&(chunk.chunk_id, class))
                .is_some_and(|pending| pending.reference.plain_len == chunk.plain_len);
            let existing = self
                .reuse_index
                .get(&(chunk.chunk_id, availability))
                .is_some_and(|reused| {
                    reused.block.plain_len == chunk.plain_len
                        && self
                            .reuse_segments
                            .get(reused.segment_ordinal as usize)
                            .is_some()
                });
            if !current && !existing {
                return false;
            }
        }
        expected_offset == source.len
    }

    fn add_fixed_file(
        &mut self,
        source: &SourceFile,
        block_size: usize,
        availability: Availability,
        access: AccessClass,
    ) -> Result<()> {
        let mut reader =
            BufReader::with_capacity(block_size.min(1024 * 1024), source.open_verified()?);
        let mut buffer = vec![0_u8; block_size];
        let mut logical_offset = 0_u64;
        loop {
            let read = read_chunk(&mut reader, &mut buffer)?;
            if read == 0 {
                break;
            }
            self.add_chunk(logical_offset, &buffer[..read], availability, access)?;
            logical_offset = logical_offset
                .checked_add(read as u64)
                .ok_or_else(|| Error::InvalidInput("file offset overflow".into()))?;
        }
        validate_source_len(logical_offset, source)?;
        source.validate_open_file(reader.get_ref())?;
        Ok(())
    }

    fn add_content_defined_file(
        &mut self,
        source: &SourceFile,
        availability: Availability,
        access: AccessClass,
    ) -> Result<()> {
        let mut file = source.open_verified()?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(source.len)
                .map_err(|_| Error::InvalidInput("source file is too large".into()))?,
        );
        file.read_to_end(&mut bytes)?;
        validate_source_len(bytes.len() as u64, source)?;
        source.validate_open_file(&file)?;
        for chunk in fastcdc::v2020::FastCDC::new(&bytes, FASTCDC_MIN, FASTCDC_AVG, FASTCDC_MAX) {
            let end = chunk
                .offset
                .checked_add(chunk.length)
                .ok_or_else(|| Error::InvalidInput("FastCDC range overflow".into()))?;
            self.add_chunk(
                chunk.offset as u64,
                &bytes[chunk.offset..end],
                availability,
                access,
            )?;
        }
        Ok(())
    }

    fn add_chunk(
        &mut self,
        logical_offset: u64,
        plaintext: &[u8],
        availability: Availability,
        access: AccessClass,
    ) -> Result<()> {
        let chunk_id = *blake3::hash(plaintext).as_bytes();
        self.record_chunk(chunk_id);
        if self.try_reuse_chunk(
            logical_offset,
            u32::try_from(plaintext.len())
                .map_err(|_| Error::InvalidInput("block plaintext length".into()))?,
            chunk_id,
            availability,
            access,
        )? {
            return Ok(());
        }
        let class = SegmentClass::new(availability, access);
        let reuse_key = (chunk_id, class);

        let (codec, encoded) = compress_block(&mut self.compressor, plaintext)?;
        let estimated_stored = encoded
            .len()
            .checked_add(16)
            .ok_or_else(|| Error::InvalidInput("block length overflow".into()))?
            as u64;
        let target_bytes = class.target_bytes(self.options.segment_target_bytes);
        if let Some(position) = self.active_segment_position(class)
            && self.active_segments[position].writer.block_count > 0
            && self.active_segments[position]
                .writer
                .payload_len
                .saturating_add(estimated_stored)
                > target_bytes
        {
            self.finish_active_segment(position)?;
        }
        let position = match self.active_segment_position(class) {
            Some(position) => position,
            None => self.start_segment(class)?,
        };
        let active = &mut self.active_segments[position];
        let segment_index = active.index;
        let reference =
            active
                .writer
                .write_block(logical_offset, plaintext.len(), codec, encoded)?;
        let pending = PendingBlock {
            locator: SegmentLocator::New(segment_index),
            reference,
        };
        self.pending_blocks.push(pending);
        self.current_reuse.insert(reuse_key, pending);
        self.new_blocks = self.new_blocks.saturating_add(1);
        Ok(())
    }

    fn record_chunk(&mut self, chunk_id: [u8; 32]) {
        self.fingerprint.update(&chunk_id);
        self.chunk_ids.push(chunk_id);
    }

    fn try_reuse_chunk(
        &mut self,
        logical_offset: u64,
        plain_len: u32,
        chunk_id: [u8; 32],
        availability: Availability,
        access: AccessClass,
    ) -> Result<bool> {
        let class = SegmentClass::new(availability, access);
        let reuse_key = (chunk_id, class);
        if let Some(reused) = self.current_reuse.get(&reuse_key).copied()
            && reused.reference.plain_len == plain_len
        {
            let mut reused = reused;
            reused.reference.logical_offset = logical_offset;
            self.pending_blocks.push(reused);
            self.reused_blocks = self.reused_blocks.saturating_add(1);
            return Ok(true);
        }
        // Existing immutable blocks keep their historical placement. A full
        // build is the explicit operation that rewrites them into new classes.
        if let Some(reused) = self.reuse_index.get(&(chunk_id, availability))
            && reused.block.plain_len == plain_len
        {
            let segment = self
                .reuse_segments
                .get(reused.segment_ordinal as usize)
                .ok_or_else(|| Error::InvalidInput("reuse segment ordinal".into()))?;
            let mut reference = reused.block;
            reference.logical_offset = logical_offset;
            self.used_existing_segments
                .entry(segment.id)
                .or_insert_with(|| segment.clone());
            let pending = PendingBlock {
                locator: SegmentLocator::Existing(segment.id),
                reference,
            };
            self.pending_blocks.push(pending);
            self.current_reuse.insert(reuse_key, pending);
            self.reused_blocks = self.reused_blocks.saturating_add(1);
            return Ok(true);
        }
        Ok(false)
    }

    fn active_segment_position(&self, class: SegmentClass) -> Option<usize> {
        self.active_segments
            .iter()
            .position(|segment| segment.class == class)
    }

    fn start_segment(&mut self, class: SegmentClass) -> Result<usize> {
        let index = self.new_segment_records.len();
        let writer = NewSegment::create(
            &self.options.output_directory.join("data"),
            index,
            self.identity.project_id(),
            self.keys,
            class.availability,
        )?;
        self.new_segment_records.push(None);
        self.active_segments.push(ActiveSegment {
            class,
            index,
            writer,
        });
        Ok(self.active_segments.len() - 1)
    }

    fn finish_active_segment(&mut self, position: usize) -> Result<()> {
        let segment = self.active_segments.swap_remove(position);
        let (record, bytes) = segment.writer.finish()?;
        self.new_segment_bytes = self.new_segment_bytes.saturating_add(bytes);
        store_segment_record(&mut self.new_segment_records, segment.index, record)?;
        Ok(())
    }

    fn finish_active_segments(&mut self) -> Result<()> {
        while !self.active_segments.is_empty() {
            let position = self
                .active_segments
                .iter()
                .enumerate()
                .min_by_key(|(_, segment)| segment.index)
                .map(|(position, _)| position)
                .expect("active segments are non-empty");
            self.finish_active_segment(position)?;
        }
        Ok(())
    }

    fn write_snapshot(
        &mut self,
        release_sequence: u64,
        source_fingerprint: [u8; 32],
        target: &Path,
    ) -> Result<PackReport> {
        let mut segments: Vec<SegmentRecord> =
            self.used_existing_segments.values().cloned().collect();
        segments.extend(self.new_segment_records.iter().flatten().cloned());
        segments.sort_by_key(|record| record.id);
        let ordinals: HashMap<SegmentId, u16> = segments
            .iter()
            .enumerate()
            .map(|(index, record)| {
                u16::try_from(index)
                    .map(|ordinal| (record.id, ordinal))
                    .map_err(|_| Error::InvalidInput("too many referenced segments".into()))
            })
            .collect::<Result<_>>()?;
        for block in &mut self.pending_blocks {
            let id = match block.locator {
                SegmentLocator::Existing(id) => id,
                SegmentLocator::New(index) => self
                    .new_segment_records
                    .get(index)
                    .and_then(Option::as_ref)
                    .map(|record| record.id)
                    .ok_or_else(|| Error::InvalidInput("new segment index".into()))?,
            };
            block.reference.segment_ordinal = *ordinals
                .get(&id)
                .ok_or_else(|| Error::InvalidInput("block references missing segment".into()))?;
        }
        let block_refs: Vec<BlockRef> = self
            .pending_blocks
            .iter()
            .map(|item| item.reference)
            .collect();
        let storage = storage_stats(&segments, &block_refs)?;
        validate_pair_count(block_refs.len(), self.chunk_ids.len())?;

        let path_slots =
            build_path_slots(&self.file_paths, &self.file_records, &self.keys.path_key())?;
        let rng = SystemRandom::new();
        let snapshot_salt = random_array::<16>(&rng)?;
        let nonce_prefix = random_array::<8>(&rng)?;
        let snapshot_key_bytes = self.keys.snapshot_key(&snapshot_salt);
        let snapshot_key = Aes256Key::new(&snapshot_key_bytes)?;
        let encrypted_pages = build_pages(
            self.identity.project_id(),
            release_sequence,
            nonce_prefix,
            &snapshot_key,
            &block_refs,
            &self.chunk_ids,
            self.options.compression_level,
        )?;
        let catalog = CatalogData {
            segments,
            files: self.file_records.clone(),
            path_slots,
            path_pool: self.path_pool.clone(),
            pages: encrypted_pages.directory,
            total_blocks: u32::try_from(block_refs.len())
                .map_err(|_| Error::InvalidInput("too many blocks".into()))?,
            map_page_count: encrypted_pages.map_count,
            reuse_page_count: encrypted_pages.reuse_count,
        }
        .encode()?;
        let mut catalog_ciphertext =
            zstd::bulk::compress(&catalog, self.options.compression_level)?;
        let catalog_stored_len = u64::try_from(catalog_ciphertext.len() + 16)
            .map_err(|_| Error::InvalidInput("catalog length".into()))?;
        let mut header = SnapshotHeader {
            project_id: self.identity.project_id(),
            release_sequence,
            catalog_stored_len,
            catalog_plain_len: catalog.len() as u64,
            page_region_offset: (hakutaku_core::format::SNAPSHOT_HEADER_SIZE as u64)
                .checked_add(catalog_stored_len)
                .ok_or_else(|| Error::InvalidInput("snapshot length overflow".into()))?,
            page_count: encrypted_pages
                .map_count
                .checked_add(encrypted_pages.reuse_count)
                .ok_or_else(|| Error::InvalidInput("page count overflow".into()))?,
            snapshot_salt,
            nonce_prefix,
            signing_key_id: crypto::signing_key_id(&self.identity.public_key()),
            source_fingerprint,
            signature: [0; 64],
        };
        let aad = crypto::catalog_aad(&header);
        snapshot_key.seal(
            crypto::nonce(nonce_prefix, 0),
            &aad,
            &mut catalog_ciphertext,
        )?;
        validate_catalog_ciphertext_len(catalog_ciphertext.len(), catalog_stored_len)?;
        let zeroed_header = header.encode(true);
        let signature_message =
            crypto::snapshot_signature_message(&zeroed_header, &catalog_ciphertext);
        header.signature = self.identity.sign(&signature_message)?;

        let temporary = target.with_extension(format!("haku.part-{}", std::process::id()));
        let mut file = BufWriter::with_capacity(
            1024 * 1024,
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?,
        );
        let result = (|| -> Result<()> {
            file.write_all(&header.encode(false))?;
            file.write_all(&catalog_ciphertext)?;
            for page in &encrypted_pages.ciphertexts {
                file.write_all(page)?;
            }
            file.flush()?;
            file.get_ref().sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            drop(file);
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);

        Ok(PackReport {
            changed: true,
            release_sequence,
            file_count: u32::try_from(self.file_records.len())
                .map_err(|_| Error::InvalidInput("too many files".into()))?,
            block_count: u32::try_from(block_refs.len())
                .map_err(|_| Error::InvalidInput("too many blocks".into()))?,
            reused_blocks: self.reused_blocks,
            new_blocks: self.new_blocks,
            new_segments: u32::try_from(
                self.new_segment_records
                    .iter()
                    .filter(|record| record.is_some())
                    .count(),
            )
            .map_err(|_| Error::InvalidInput("too many new segments".into()))?,
            new_segment_bytes: self.new_segment_bytes,
            retained_segment_bytes: storage.retained,
            referenced_block_bytes: storage.referenced,
            stranded_segment_bytes: storage.stranded,
        })
    }
}

fn store_segment_record(
    records: &mut [Option<SegmentRecord>],
    index: usize,
    record: SegmentRecord,
) -> Result<()> {
    let slot = records
        .get_mut(index)
        .ok_or_else(|| Error::InvalidInput("new segment index".into()))?;
    if slot.is_some() {
        return Err(Error::InvalidInput("new segment finalized twice".into()));
    }
    *slot = Some(record);
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StorageStats {
    retained: u64,
    referenced: u64,
    stranded: u64,
}

fn package_storage_stats(package: &Package) -> Result<StorageStats> {
    let segment_count = package.segment_ids()?.len();
    let mut segments = Vec::with_capacity(segment_count);
    for index in 0..segment_count {
        let ordinal = u16::try_from(index)
            .map_err(|_| Error::InvalidInput("too many referenced segments".into()))?;
        segments.push(package.segment_record(ordinal)?);
    }
    let blocks = package
        .reuse_records()?
        .into_iter()
        .map(|record| record.block)
        .collect::<Vec<_>>();
    storage_stats(&segments, &blocks)
}

fn storage_stats(segments: &[SegmentRecord], blocks: &[BlockRef]) -> Result<StorageStats> {
    let retained = segments.iter().try_fold(0_u64, |total, segment| {
        total
            .checked_add(segment.file_len)
            .ok_or_else(|| Error::InvalidInput("retained segment size overflow".into()))
    })?;
    let payload = segments.iter().try_fold(0_u64, |total, segment| {
        total
            .checked_add(segment.payload_len)
            .ok_or_else(|| Error::InvalidInput("segment payload size overflow".into()))
    })?;
    let mut unique = HashMap::new();
    for block in blocks {
        let segment = segments
            .get(block.segment_ordinal as usize)
            .ok_or_else(|| Error::InvalidInput("block references missing segment".into()))?;
        if block.segment_block_ordinal >= segment.block_count {
            return Err(Error::InvalidInput(
                "block references missing segment block".into(),
            ));
        }
        let key = (block.segment_ordinal, block.segment_block_ordinal);
        if let Some(previous) = unique.insert(key, block.stored_len)
            && previous != block.stored_len
        {
            return Err(Error::InvalidInput(
                "shared block has inconsistent stored lengths".into(),
            ));
        }
    }
    let referenced = unique.values().try_fold(0_u64, |total, stored_len| {
        total
            .checked_add(u64::from(*stored_len))
            .ok_or_else(|| Error::InvalidInput("referenced block size overflow".into()))
    })?;
    let stranded = payload
        .checked_sub(referenced)
        .ok_or_else(|| Error::InvalidInput("referenced blocks exceed segment payload".into()))?;
    Ok(StorageStats {
        retained,
        referenced,
        stranded,
    })
}

struct NewSegment {
    temporary_path: PathBuf,
    data_directory: PathBuf,
    writer: Option<BufWriter<File>>,
    project_id: ProjectId,
    uid: [u8; 16],
    salt: [u8; 16],
    nonce_prefix: [u8; 8],
    key: Aes256Key,
    availability: Availability,
    block_count: u32,
    payload_len: u64,
}

impl NewSegment {
    fn create(
        data_directory: &Path,
        index: usize,
        project_id: ProjectId,
        keys: &ProjectKeys,
        availability: Availability,
    ) -> Result<Self> {
        let rng = SystemRandom::new();
        let uid = random_array::<16>(&rng)?;
        let salt = random_array::<16>(&rng)?;
        let nonce_prefix = random_array::<8>(&rng)?;
        let temporary_path = data_directory.join(format!(
            ".segment-{}-{index}.{SEGMENT_FILE_EXTENSION}.part",
            std::process::id()
        ));
        let file = OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(&temporary_path)?;
        let mut writer = BufWriter::with_capacity(1024 * 1024, file);
        writer.write_all(&[0_u8; SEGMENT_HEADER_SIZE])?;
        let incomplete_header = SegmentHeader {
            project_id,
            segment_uid: uid,
            salt,
            nonce_prefix,
            block_count: 0,
            payload_len: 0,
            file_len: SEGMENT_HEADER_SIZE as u64,
        };
        let key_bytes = keys.segment_key(&incomplete_header);
        let key = Aes256Key::new(&key_bytes)?;
        Ok(Self {
            temporary_path,
            data_directory: data_directory.to_path_buf(),
            writer: Some(writer),
            project_id,
            uid,
            salt,
            nonce_prefix,
            key,
            availability,
            block_count: 0,
            payload_len: 0,
        })
    }

    fn write_block(
        &mut self,
        logical_offset: u64,
        plain_len: usize,
        codec: Codec,
        mut encoded: Vec<u8>,
    ) -> Result<BlockRef> {
        let plain_len = u32::try_from(plain_len)
            .map_err(|_| Error::InvalidInput("block plaintext is too large".into()))?;
        let stored_len = u32::try_from(encoded.len() + 16)
            .map_err(|_| Error::InvalidInput("block ciphertext is too large".into()))?;
        let ordinal = self.block_count;
        let aad = crypto::block_aad(
            self.project_id,
            &self.uid,
            ordinal,
            codec,
            stored_len,
            plain_len,
        );
        self.key.seal(
            crypto::nonce(self.nonce_prefix, ordinal),
            &aad,
            &mut encoded,
        )?;
        let physical_offset = segment_physical_offset(self.payload_len)?;
        self.writer
            .as_mut()
            .expect("writer is active")
            .write_all(&encoded)?;
        self.payload_len = self
            .payload_len
            .checked_add(encoded.len() as u64)
            .ok_or_else(|| Error::InvalidInput("segment length overflow".into()))?;
        self.block_count = self
            .block_count
            .checked_add(1)
            .ok_or_else(|| Error::InvalidInput("segment block count overflow".into()))?;
        let digest = blake3::hash(&encoded);
        Ok(BlockRef {
            logical_offset,
            segment_ordinal: 0,
            segment_block_ordinal: ordinal,
            physical_offset,
            stored_len,
            plain_len,
            codec,
            cipher_digest: digest.as_bytes()[..16]
                .try_into()
                .expect("fixed digest prefix"),
        })
    }

    fn finish(mut self) -> Result<(SegmentRecord, u64)> {
        let file_len = (SEGMENT_HEADER_SIZE as u64)
            .checked_add(self.payload_len)
            .ok_or_else(|| Error::InvalidInput("segment file length overflow".into()))?;
        let header = SegmentHeader {
            project_id: self.project_id,
            segment_uid: self.uid,
            salt: self.salt,
            nonce_prefix: self.nonce_prefix,
            block_count: self.block_count,
            payload_len: self.payload_len,
            file_len,
        };
        let mut writer = self.writer.take().expect("writer is active");
        writer.flush()?;
        writer.seek(SeekFrom::Start(0))?;
        writer.write_all(&header.encode())?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        let id = hash_file(&self.temporary_path)?;
        let final_path = self.data_directory.join(segment_file_name(id));
        if final_path.exists() {
            if hash_file(&final_path)? != id {
                return Err(Error::InvalidInput(format!(
                    "existing segment has wrong content: {}",
                    final_path.display()
                )));
            }
            std::fs::remove_file(&self.temporary_path)?;
        } else {
            std::fs::rename(&self.temporary_path, &final_path)?;
        }
        Ok((
            SegmentRecord {
                id,
                uid: self.uid,
                salt: self.salt,
                nonce_prefix: self.nonce_prefix,
                file_len,
                payload_len: self.payload_len,
                block_count: self.block_count,
                availability: self.availability,
            },
            file_len,
        ))
    }
}

impl Drop for NewSegment {
    fn drop(&mut self) {
        if self.writer.is_some() {
            let _ = std::fs::remove_file(&self.temporary_path);
        }
    }
}

fn load_reuse_index(old: Option<&Package>) -> Result<ReuseIndex> {
    let Some(old) = old else {
        return Ok(ReuseIndex {
            blocks: HashMap::new(),
            segments: Vec::new(),
        });
    };
    let segments = old
        .list_segments()?
        .into_iter()
        .enumerate()
        .map(|(ordinal, _)| {
            let ordinal = u16::try_from(ordinal)
                .map_err(|_| Error::InvalidInput("too many reuse segments".into()))?;
            old.segment_record(ordinal).map_err(Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut blocks = HashMap::new();
    for reuse in old.reuse_records()? {
        let segment = segments
            .get(reuse.block.segment_ordinal as usize)
            .ok_or_else(|| Error::InvalidInput("reuse segment ordinal".into()))?;
        blocks
            .entry((reuse.chunk_id, segment.availability))
            .or_insert(ReusableBlock {
                block: reuse.block,
                segment_ordinal: reuse.block.segment_ordinal,
            });
    }
    Ok(ReuseIndex { blocks, segments })
}

struct EncryptedPages {
    directory: Vec<PageRecord>,
    ciphertexts: Vec<Vec<u8>>,
    map_count: u32,
    reuse_count: u32,
}

fn build_pages(
    project_id: ProjectId,
    release_sequence: u64,
    nonce_prefix: [u8; 8],
    snapshot_key: &Aes256Key,
    block_refs: &[BlockRef],
    chunk_ids: &[[u8; 32]],
    compression_level: i32,
) -> Result<EncryptedPages> {
    validate_pair_count(block_refs.len(), chunk_ids.len())?;
    let page_capacity =
        block_refs.len().div_ceil(BLOCKS_PER_MAP_PAGE) + chunk_ids.len().div_ceil(REUSE_PER_PAGE);
    let mut compressor = zstd::bulk::Compressor::new(compression_level)?;
    compressor.include_checksum(false)?;
    compressor.include_dictid(false)?;
    let mut directory = Vec::with_capacity(page_capacity);
    let mut ciphertexts = Vec::with_capacity(page_capacity);
    let mut relative_offset = 0_u64;
    for (page_index, records) in block_refs.chunks(BLOCKS_PER_MAP_PAGE).enumerate() {
        let first = u32::try_from(page_index * BLOCKS_PER_MAP_PAGE)
            .map_err(|_| Error::InvalidInput("map page index".into()))?;
        let plaintext = encode_map_page(first, records)?;
        encrypt_page(
            project_id,
            release_sequence,
            nonce_prefix,
            snapshot_key,
            &mut compressor,
            PageKind::BlockMap,
            plaintext,
            &mut relative_offset,
            &mut directory,
            &mut ciphertexts,
        )?;
    }
    let map_page_count = u32::try_from(directory.len())
        .map_err(|_| Error::InvalidInput("too many map pages".into()))?;
    for (page_index, chunk_page) in chunk_ids.chunks(REUSE_PER_PAGE).enumerate() {
        let first = u32::try_from(page_index * REUSE_PER_PAGE)
            .map_err(|_| Error::InvalidInput("reuse page index".into()))?;
        let block_start = page_index
            .checked_mul(REUSE_PER_PAGE)
            .ok_or_else(|| Error::InvalidInput("reuse page index".into()))?;
        let block_page = block_refs
            .get(block_start..block_start + chunk_page.len())
            .ok_or_else(|| Error::InvalidInput("reuse page block range".into()))?;
        let records = chunk_page
            .iter()
            .copied()
            .zip(block_page.iter().copied())
            .map(|(chunk_id, block)| ReuseRecord { chunk_id, block })
            .collect::<Vec<_>>();
        let plaintext = encode_reuse_page(first, &records)?;
        encrypt_page(
            project_id,
            release_sequence,
            nonce_prefix,
            snapshot_key,
            &mut compressor,
            PageKind::Reuse,
            plaintext,
            &mut relative_offset,
            &mut directory,
            &mut ciphertexts,
        )?;
    }
    let reuse_page_count = u32::try_from(directory.len())
        .map_err(|_| Error::InvalidInput("too many pages".into()))?
        .checked_sub(map_page_count)
        .ok_or_else(|| Error::InvalidInput("page count".into()))?;
    Ok(EncryptedPages {
        directory,
        ciphertexts,
        map_count: map_page_count,
        reuse_count: reuse_page_count,
    })
}

#[allow(clippy::too_many_arguments)]
fn encrypt_page(
    project_id: ProjectId,
    release_sequence: u64,
    nonce_prefix: [u8; 8],
    snapshot_key: &Aes256Key,
    compressor: &mut zstd::bulk::Compressor<'_>,
    kind: PageKind,
    plaintext: Vec<u8>,
    relative_offset: &mut u64,
    directory: &mut Vec<PageRecord>,
    ciphertexts: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let plain_len = u32::try_from(plaintext.len())
        .map_err(|_| Error::InvalidInput("page plaintext length".into()))?;
    let (codec, mut encoded) = compress_bytes(compressor, &plaintext)?;
    let stored_len = u32::try_from(encoded.len() + 16)
        .map_err(|_| Error::InvalidInput("page ciphertext length".into()))?;
    let nonce_ordinal = u32::try_from(directory.len() + 1)
        .map_err(|_| Error::InvalidInput("page nonce ordinal".into()))?;
    let aad = crypto::page_aad(
        project_id,
        release_sequence,
        kind,
        codec,
        nonce_ordinal,
        stored_len,
        plain_len,
    );
    snapshot_key.seal(
        crypto::nonce(nonce_prefix, nonce_ordinal),
        &aad,
        &mut encoded,
    )?;
    let digest = *blake3::hash(&encoded).as_bytes();
    directory.push(PageRecord {
        kind,
        codec,
        nonce_ordinal,
        relative_offset: *relative_offset,
        stored_len,
        plain_len,
        digest,
    });
    *relative_offset = relative_offset
        .checked_add(u64::from(stored_len))
        .ok_or_else(|| Error::InvalidInput("page region overflow".into()))?;
    ciphertexts.push(encoded);
    Ok(())
}

fn build_path_slots(
    paths: &[String],
    files: &[FileRecord],
    path_key: &[u8; 32],
) -> Result<Vec<PathSlot>> {
    if paths.len() != files.len() {
        return Err(Error::InvalidInput("path/file count mismatch".into()));
    }
    let desired = paths.len().saturating_mul(2).max(1);
    let slot_count = desired
        .checked_next_power_of_two()
        .ok_or_else(|| Error::InvalidInput("path index is too large".into()))?;
    let mut slots = vec![
        PathSlot {
            hash: 0,
            file_index: EMPTY_PATH_SLOT,
        };
        slot_count
    ];
    let mask = slot_count - 1;
    for (file_index, path) in paths.iter().enumerate() {
        let digest = blake3::keyed_hash(path_key, path.as_bytes());
        let hash = u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("fixed prefix"));
        let mut inserted = false;
        for probe in 0..slot_count {
            let slot = hash.wrapping_add(probe as u64) as usize & mask;
            if slots[slot].file_index == EMPTY_PATH_SLOT {
                slots[slot] = PathSlot {
                    hash,
                    file_index: u32::try_from(file_index)
                        .map_err(|_| Error::InvalidInput("too many files".into()))?,
                };
                inserted = true;
                break;
            }
        }
        if !inserted {
            return Err(Error::InvalidInput("path index is full".into()));
        }
    }
    Ok(slots)
}

fn compress_block(
    compressor: &mut zstd::bulk::Compressor<'_>,
    plaintext: &[u8],
) -> Result<(Codec, Vec<u8>)> {
    compress_bytes(compressor, plaintext)
}

fn compress_bytes(
    compressor: &mut zstd::bulk::Compressor<'_>,
    plaintext: &[u8],
) -> Result<(Codec, Vec<u8>)> {
    let compressed = compressor.compress(plaintext)?;
    if compressed.len().saturating_add(COMPRESSION_SAVINGS) < plaintext.len() {
        Ok((Codec::Zstd, compressed))
    } else {
        Ok((Codec::Raw, plaintext.to_vec()))
    }
}

fn checked_path_len(path: &str) -> Result<u16> {
    u16::try_from(path.len()).map_err(|_| Error::InvalidInput(format!("path is too long: {path}")))
}

fn validate_source_len(actual: u64, source: &SourceFile) -> Result<()> {
    if actual == source.len {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "file changed while packing: {}",
            source.logical_path
        )))
    }
}

fn validate_pair_count(blocks: usize, reuse: usize) -> Result<()> {
    if blocks == reuse {
        Ok(())
    } else {
        Err(Error::InvalidInput("block/reuse record mismatch".into()))
    }
}

fn validate_catalog_ciphertext_len(actual: usize, expected: u64) -> Result<()> {
    if actual as u64 == expected {
        Ok(())
    } else {
        Err(Error::InvalidInput("catalog ciphertext length".into()))
    }
}

fn segment_physical_offset(payload_len: u64) -> Result<u64> {
    (SEGMENT_HEADER_SIZE as u64)
        .checked_add(payload_len)
        .ok_or_else(|| Error::InvalidInput("segment offset overflow".into()))
}

fn read_chunk(reader: &mut impl Read, buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(filled)
}

fn hash_file(path: &Path) -> Result<SegmentId> {
    let mut reader = BufReader::with_capacity(1024 * 1024, File::open(path)?);
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(SegmentId(*hasher.finalize().as_bytes()))
}

fn verify_staged_release(
    snapshot: &Path,
    data_directory: &Path,
    files: &[SourceFile],
    identity: &Identity,
    verify_sources: bool,
) -> Result<()> {
    let package = Package::open_directory(
        snapshot,
        data_directory,
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::memory_constrained(),
    )?;
    package.verify_segments()?;
    if !verify_sources {
        return Ok(());
    }
    let mut expected = vec![0_u8; 1024 * 1024];
    let mut actual = vec![0_u8; 1024 * 1024];
    for source in files {
        let mut source_file = source.open_verified()?;
        let mut asset = package.asset(&source.logical_path)?.cursor();
        loop {
            let expected_len = source_file.read(&mut expected)?;
            let actual_len = asset.read(&mut actual)?;
            if expected_len != actual_len || expected[..expected_len] != actual[..actual_len] {
                return Err(Error::InvalidInput(format!(
                    "verification mismatch: {}",
                    source.logical_path
                )));
            }
            if expected_len == 0 {
                break;
            }
        }
        source.validate_open_file(&source_file)?;
    }
    Ok(())
}

fn save_build_cache(
    options: &PackOptions,
    identity: &Identity,
    release_sequence: u64,
    files: &[SourceFile],
    context: &BuildContext<'_>,
) -> Result<()> {
    validate_build_cache_record_count(files.len(), context.file_records.len())?;
    let mut entries = Vec::with_capacity(files.len());
    for (source, record) in files.iter().zip(&context.file_records) {
        let start = record.first_block as usize;
        let end = start
            .checked_add(record.block_count as usize)
            .ok_or_else(|| Error::InvalidInput("build-cache block range".into()))?;
        let chunk_ids = context
            .chunk_ids
            .get(start..end)
            .ok_or_else(|| Error::InvalidInput("build-cache chunk range".into()))?;
        let blocks = context
            .pending_blocks
            .get(start..end)
            .ok_or_else(|| Error::InvalidInput("build-cache block range".into()))?;
        let chunks = chunk_ids
            .iter()
            .copied()
            .zip(blocks)
            .map(|(chunk_id, pending)| CachedChunk {
                logical_offset: pending.reference.logical_offset,
                plain_len: pending.reference.plain_len,
                chunk_id,
            })
            .collect();
        entries.push((
            source.logical_path.clone(),
            CachedEntry {
                stamp: source.stamp(),
                layout: record.layout,
                fixed_block_len: record.fixed_block_len,
                access: record.access,
                availability: options.availability(&source.logical_path),
                chunks,
            },
        ));
    }
    build_cache::save(
        &options.output_directory.join(BUILD_CACHE_FILE),
        identity.project_id(),
        release_sequence,
        &entries,
    )
}

fn validate_build_cache_record_count(files: usize, records: usize) -> Result<()> {
    if files != records {
        return Err(Error::InvalidInput(
            "build-cache file record mismatch".into(),
        ));
    }
    Ok(())
}

fn commit_snapshot(target: &Path) -> Result<()> {
    let temporary = target.with_extension(format!("haku.part-{}", std::process::id()));
    #[cfg(unix)]
    {
        std::fs::rename(temporary, target)?;
    }
    #[cfg(windows)]
    {
        let previous = target.with_extension("haku.previous");
        if target.exists() {
            if previous.exists() {
                std::fs::remove_file(&previous)?;
            }
            std::fs::rename(target, &previous)?;
        }
        if let Err(error) = std::fs::rename(&temporary, target) {
            if previous.exists() {
                let _ = std::fs::rename(&previous, target);
            }
            return Err(Error::Io(error));
        }
        if previous.exists() {
            std::fs::remove_file(previous)?;
        }
    }
    if let Some(parent) = target.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn recover_interrupted_release(output: &Path) -> Result<()> {
    let target = output.join("game.haku");
    let previous = output.join("game.haku.previous");
    let mut output_changed = false;
    if !target.exists() && previous.exists() {
        std::fs::rename(previous, target)?;
        output_changed = true;
    }
    for entry in std::fs::read_dir(output)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type()?.is_file() && name.starts_with("game.haku.part-") {
            std::fs::remove_file(entry.path())?;
            output_changed = true;
        }
    }
    if output_changed {
        sync_directory(output)?;
    }

    let data = output.join("data");
    if data.is_dir() {
        let mut data_changed = false;
        for entry in std::fs::read_dir(&data)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if entry.file_type()?.is_file()
                && name.starts_with(".segment-")
                && name.ends_with(&format!(".{SEGMENT_FILE_EXTENSION}.part"))
            {
                std::fs::remove_file(entry.path())?;
                data_changed = true;
            }
        }
        if data_changed {
            sync_directory(&data)?;
        }
    }
    Ok(())
}

fn cleanup_unreferenced_segments(data_directory: &Path, active: &[SegmentId]) -> Result<()> {
    let active_names: HashSet<String> = active.iter().copied().map(segment_file_name).collect();
    let mut removed = false;
    for entry in std::fs::read_dir(data_directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(&format!(".{SEGMENT_FILE_EXTENSION}")) else {
            continue;
        };
        if stem.len() == 64
            && stem.as_bytes().iter().all(u8::is_ascii_hexdigit)
            && !active_names.contains(&name)
        {
            std::fs::remove_file(entry.path())?;
            removed = true;
        }
    }
    if removed {
        sync_directory(data_directory)?;
    }
    Ok(())
}

/// Persist directory-entry changes on Unix. Each file is synchronized before
/// it is renamed; synchronizing the containing directory closes the remaining
/// crash window between a durable file and its durable name. Windows keeps the
/// same verified rename/recovery protocol without adding a platform crate to
/// the publisher-only dependency graph.
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn validate_options(options: &PackOptions) -> Result<()> {
    if !options.input_directory.is_dir() {
        return Err(Error::InvalidInput(format!(
            "input is not a directory: {}",
            options.input_directory.display()
        )));
    }
    if options.segment_target_bytes < 1024 * 1024 {
        return Err(Error::InvalidInput(
            "segment target must be at least 1 MiB".into(),
        ));
    }
    if options.compression_level < -7 || options.compression_level > 22 {
        return Err(Error::InvalidInput(
            "zstd level must be between -7 and 22".into(),
        ));
    }
    let input = options.input_directory.canonicalize()?;
    let output = resolve_candidate_path(&options.output_directory)?;
    if output == input || output.starts_with(&input) {
        return Err(Error::InvalidInput(format!(
            "output directory must be outside the input asset directory: {}",
            options.output_directory.display()
        )));
    }
    for prefix in &options.deferred_prefixes {
        validate_canonical_path(prefix).map_err(|_| {
            Error::InvalidInput(format!("deferred prefix is not canonical: {prefix:?}"))
        })?;
    }
    Ok(())
}

fn validate_identity_location(options: &PackOptions, identity: &Identity) -> Result<()> {
    let Some(identity_path) = identity.source_path() else {
        return Ok(());
    };
    let input = options.input_directory.canonicalize()?;
    if identity_path == input || identity_path.starts_with(&input) {
        return Err(Error::InvalidInput(format!(
            "publisher identity must be outside the input asset directory: {}",
            identity_path.display()
        )));
    }
    Ok(())
}

fn resolve_candidate_path(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    if absolute.exists() {
        return Ok(absolute.canonicalize()?);
    }
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::<OsString>::new();
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| {
            Error::InvalidInput(format!("path has no existing ancestor: {}", path.display()))
        })?;
        suffix.push(name.to_owned());
        ancestor = ancestor.parent().ok_or_else(|| {
            Error::InvalidInput(format!("path has no existing ancestor: {}", path.display()))
        })?;
    }
    let mut resolved = ancestor.canonicalize()?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn random_array<const N: usize>(rng: &dyn SecureRandom) -> Result<[u8; N]> {
    let mut value = [0_u8; N];
    rng.fill(&mut value)
        .map_err(|_| Error::Crypto("secure random generation"))?;
    Ok(value)
}

struct BuildLock {
    path: PathBuf,
}

impl BuildLock {
    fn acquire(output: &Path) -> Result<Self> {
        let path = output.join(".hakutaku.lock");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Error::InvalidInput(format!(
                        "another build may be active; remove stale lock if necessary: {}",
                        path.display()
                    ))
                } else {
                    Error::Io(error)
                }
            })?;
        writeln!(file, "{}", std::process::id())?;
        file.sync_all()?;
        Ok(Self { path })
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{BULK_BLOCK, CONTENT_DEFINED_LIMIT, HOT_FILE_LIMIT};
    use std::collections::VecDeque;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hakutaku-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn compressed_data_must_save_meaningful_space() {
        let mut compressor = zstd::bulk::Compressor::new(3).unwrap();
        let (codec, _) = compress_bytes(&mut compressor, &[0; 4096]).unwrap();
        assert_eq!(codec, Codec::Zstd);
        let randomish: Vec<u8> = (0..4096).map(|index| (index * 73) as u8).collect();
        let (_, encoded) = compress_bytes(&mut compressor, &randomish).unwrap();
        assert!(encoded.len() <= randomish.len());
    }

    #[test]
    fn path_table_keeps_an_empty_sentinel() {
        let paths = vec!["a".to_owned(), "b".to_owned()];
        let files = vec![
            FileRecord {
                path_offset: 0,
                path_len: 1,
                layout: LayoutKind::Fixed,
                access: AccessClass::Normal,
                logical_len: 0,
                first_block: 0,
                block_count: 0,
                fixed_block_len: 1,
            };
            2
        ];
        let slots = build_path_slots(&paths, &files, &[1; 32]).unwrap();
        assert!(slots.iter().any(|slot| slot.file_index == EMPTY_PATH_SLOT));
    }

    #[test]
    fn recovery_restores_snapshot_and_removes_only_temporary_files() {
        let output = scratch("recovery");
        let _ = std::fs::remove_dir_all(&output);
        std::fs::create_dir_all(output.join("data")).unwrap();
        std::fs::write(output.join("game.haku.previous"), b"previous").unwrap();
        std::fs::write(output.join("game.haku.part-7"), b"partial").unwrap();
        std::fs::write(output.join("data/.segment-7-0.taku.part"), b"partial").unwrap();
        std::fs::write(output.join("data/notes.txt"), b"keep").unwrap();

        recover_interrupted_release(&output).unwrap();

        assert_eq!(
            std::fs::read(output.join("game.haku")).unwrap(),
            b"previous"
        );
        assert!(!output.join("game.haku.part-7").exists());
        assert!(!output.join("data/.segment-7-0.taku.part").exists());
        assert!(output.join("data/notes.txt").is_file());
        std::fs::remove_dir_all(output).unwrap();
    }

    #[test]
    fn options_availability_and_classification_cover_all_policies() {
        let root = scratch("options");
        let input = root.join("input");
        std::fs::create_dir_all(&input).unwrap();
        let mut options = PackOptions::new(&input, root.join("output"));
        options.deferred_prefixes = vec!["dlc".into()];
        assert_eq!(options.availability("dlc"), Availability::Deferred);
        assert_eq!(
            options.availability("dlc/movie.mp4"),
            Availability::Deferred
        );
        assert_eq!(
            options.availability("dlc2/movie.mp4"),
            Availability::Required
        );
        validate_options(&options).unwrap();

        let source = |path: &str, len| SourceFile::test(path, len);
        assert_eq!(classify(&source("tiny", 0)).2, AccessClass::Hot);
        assert_eq!(classify(&source("MOVIE.MP4", 1)).2, AccessClass::Streaming);
        assert_eq!(
            classify(&source("voice/line.opus", 12 * 1024)).2,
            AccessClass::Transient
        );
        assert_eq!(
            classify(&source("bgm/theme.opus", 12 * 1024)).2,
            AccessClass::Streaming
        );
        assert_eq!(
            classify(&source("asset.bin", HOT_FILE_LIMIT + 1)).0,
            LayoutKind::ContentDefined
        );
        assert_eq!(
            classify(&source("asset.bin", CONTENT_DEFINED_LIMIT + 1)).1,
            BULK_BLOCK as u32
        );

        let mut invalid = options.clone();
        invalid.input_directory = root.join("missing");
        assert!(validate_options(&invalid).is_err());
        let mut invalid = options.clone();
        invalid.segment_target_bytes = 1;
        assert!(validate_options(&invalid).is_err());
        for level in [-8, 23] {
            let mut invalid = options.clone();
            invalid.compression_level = level;
            assert!(validate_options(&invalid).is_err());
        }
        let mut invalid = options;
        invalid.deferred_prefixes = vec!["/absolute".into()];
        assert!(validate_options(&invalid).is_err());

        let nested_output = PackOptions::new(&input, input.join("release"));
        assert!(validate_options(&nested_output).is_err());
        let same_output = PackOptions::new(&input, &input);
        assert!(validate_options(&same_output).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publisher_keys_are_rejected_inside_the_asset_tree_by_content_and_location() {
        let root = scratch("identity-resource");
        let input = root.join("input");
        let output = root.join("output");
        std::fs::create_dir_all(&input).unwrap();
        let identity_path = input.join("innocent-looking.bin");
        Identity::generate().unwrap().save(&identity_path).unwrap();
        let identity = Identity::load(&identity_path).unwrap();
        let options = PackOptions::new(&input, &output);

        assert!(validate_identity_location(&options, &identity).is_err());
        assert!(collect_files(&input).is_err());
        assert!(pack_directory(&options, &identity).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn development_cache_is_release_bound_and_corruption_falls_back_to_source_reads() {
        fn mutate_cache(path: &Path, mutate: impl FnOnce(&mut [u8])) {
            let mut bytes = std::fs::read(path).unwrap();
            mutate(&mut bytes);
            let payload = bytes.len() - 32;
            let checksum = blake3::hash(&bytes[..payload]);
            bytes[payload..].copy_from_slice(checksum.as_bytes());
            std::fs::write(path, bytes).unwrap();
        }

        let root = scratch("development-cache");
        let input = root.join("input");
        let output = root.join("output");
        std::fs::create_dir_all(&input).unwrap();
        std::fs::write(input.join("voice.opus"), vec![7_u8; 64 * 1024]).unwrap();
        let identity = Identity::generate().unwrap();
        let mut options = PackOptions::new(&input, &output);
        options.development_cache = true;

        let first = pack_directory(&options, &identity).unwrap();
        assert!(first.changed);
        let cache_path = output.join(BUILD_CACHE_FILE);
        assert!(cache_path.is_file());
        let unchanged = pack_directory(&options, &identity).unwrap();
        assert!(!unchanged.changed);

        std::fs::write(&cache_path, b"corrupt local cache").unwrap();
        let recovered = pack_directory(&options, &identity).unwrap();
        assert!(!recovered.changed);
        assert!(cache_path.metadata().unwrap().len() > b"corrupt local cache".len() as u64);

        // These remain structurally valid caches, but each violates a separate
        // source/reuse invariant and must fall back to reading source bytes.
        mutate_cache(&cache_path, |bytes| bytes[54] ^= 1);
        assert!(!pack_directory(&options, &identity).unwrap().changed);
        mutate_cache(&cache_path, |bytes| bytes[108] = 1);
        assert!(!pack_directory(&options, &identity).unwrap().changed);
        mutate_cache(&cache_path, |bytes| bytes[120..152].fill(0));
        assert!(!pack_directory(&options, &identity).unwrap().changed);
        assert!(validate_build_cache_record_count(1, 0).is_err());
        assert!(validate_build_cache_record_count(1, 1).is_ok());
        assert!(
            verify_staged_release(
                &root.join("missing.haku"),
                &output.join("data"),
                &[],
                &identity,
                false,
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn segment_classes_balance_access_patterns_and_configured_limits() {
        let required = Availability::Required;
        assert_eq!(
            SegmentClass::new(required, AccessClass::Hot).target_bytes(DEFAULT_SEGMENT_TARGET),
            HOT_SEGMENT_TARGET
        );
        assert_eq!(
            SegmentClass::new(required, AccessClass::Normal).target_bytes(DEFAULT_SEGMENT_TARGET),
            NORMAL_SEGMENT_TARGET
        );
        assert_eq!(
            SegmentClass::new(required, AccessClass::Streaming)
                .target_bytes(DEFAULT_SEGMENT_TARGET),
            DEFAULT_SEGMENT_TARGET
        );
        assert_eq!(
            SegmentClass::new(required, AccessClass::Transient)
                .target_bytes(DEFAULT_SEGMENT_TARGET),
            TRANSIENT_SEGMENT_TARGET
        );
        assert_eq!(
            SegmentClass::new(required, AccessClass::Streaming).target_bytes(1024 * 1024),
            1024 * 1024
        );
    }

    #[test]
    fn semantic_classes_share_no_new_segments() {
        let root = scratch("semantic-segments");
        let input = root.join("input");
        let output = root.join("output");
        std::fs::create_dir_all(input.join("dlc")).unwrap();
        std::fs::write(input.join("hot.txt"), vec![1; 1024]).unwrap();
        std::fs::write(input.join("normal.bin"), vec![2; 40 * 1024]).unwrap();
        std::fs::write(input.join("stream.mp4"), vec![3; 40 * 1024]).unwrap();
        std::fs::write(input.join("dlc/stream.mp4"), vec![4; 40 * 1024]).unwrap();
        let mut options = PackOptions::new(&input, &output);
        options.deferred_prefixes.push("dlc".into());
        let identity = Identity::generate().unwrap();
        let report = pack_directory(&options, &identity).unwrap();
        assert_eq!(report.new_segments, 4);
        assert_eq!(report.stranded_segment_bytes, 0);
        assert_eq!(
            report.retained_segment_bytes - report.referenced_block_bytes,
            4 * SEGMENT_HEADER_SIZE as u64
        );
        std::fs::write(input.join("hot.txt"), vec![5; 1024]).unwrap();
        let incremental = pack_directory(&options, &identity).unwrap();
        assert_eq!(incremental.new_segments, 1);
        assert_eq!(incremental.stranded_segment_bytes, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_class_rotates_at_its_configured_segment_limit() {
        let root = scratch("segment-rotation");
        let input = root.join("input");
        let output = root.join("output");
        std::fs::create_dir_all(&input).unwrap();
        let mut bytes = vec![0_u8; 2 * 1024 * 1024];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for chunk in bytes.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
        }
        std::fs::write(input.join("movie.mp4"), bytes).unwrap();
        let mut options = PackOptions::new(&input, &output);
        options.segment_target_bytes = 1024 * 1024;
        let report = pack_directory(&options, &Identity::generate().unwrap()).unwrap();
        assert_eq!(report.new_segments, 3);
        assert_eq!(report.stranded_segment_bytes, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn storage_stats_deduplicate_references_and_measure_stranded_payload() {
        let segments = vec![SegmentRecord {
            id: SegmentId([1; 32]),
            uid: [2; 16],
            salt: [3; 16],
            nonce_prefix: [4; 8],
            file_len: SEGMENT_HEADER_SIZE as u64 + 1000,
            payload_len: 1000,
            block_count: 2,
            availability: Availability::Required,
        }];
        let block = BlockRef {
            logical_offset: 0,
            segment_ordinal: 0,
            segment_block_ordinal: 0,
            physical_offset: SEGMENT_HEADER_SIZE as u64,
            stored_len: 400,
            plain_len: 384,
            codec: Codec::Raw,
            cipher_digest: [5; 16],
        };
        assert_eq!(
            storage_stats(&segments, &[block, block]).unwrap(),
            StorageStats {
                retained: SEGMENT_HEADER_SIZE as u64 + 1000,
                referenced: 400,
                stranded: 600,
            }
        );
        let mut inconsistent = block;
        inconsistent.stored_len = 401;
        assert!(storage_stats(&segments, &[block, inconsistent]).is_err());
        let mut missing = block;
        missing.segment_ordinal = 1;
        assert!(storage_stats(&segments, &[missing]).is_err());
        let mut missing = block;
        missing.segment_block_ordinal = 2;
        assert!(storage_stats(&segments, &[missing]).is_err());

        let mut records = vec![None];
        store_segment_record(&mut records, 0, segments[0].clone()).unwrap();
        assert!(store_segment_record(&mut records, 0, segments[0].clone()).is_err());
        assert!(store_segment_record(&mut records, 1, segments[0].clone()).is_err());
    }

    struct ScriptedReader(VecDeque<std::io::Result<Vec<u8>>>);

    impl Read for ScriptedReader {
        fn read(&mut self, destination: &mut [u8]) -> std::io::Result<usize> {
            match self.0.pop_front().unwrap_or(Ok(Vec::new()))? {
                bytes if bytes.is_empty() => Ok(0),
                bytes => {
                    let len = bytes.len().min(destination.len());
                    destination[..len].copy_from_slice(&bytes[..len]);
                    Ok(len)
                }
            }
        }
    }

    #[test]
    fn chunk_reader_retries_interrupts_and_propagates_other_errors() {
        let mut reader = ScriptedReader(VecDeque::from([
            Err(std::io::ErrorKind::Interrupted.into()),
            Ok(vec![1, 2]),
            Ok(vec![3, 4]),
        ]));
        let mut bytes = [0; 4];
        assert_eq!(read_chunk(&mut reader, &mut bytes).unwrap(), 4);
        assert_eq!(bytes, [1, 2, 3, 4]);
        let mut reader = ScriptedReader(VecDeque::from([Err(
            std::io::ErrorKind::PermissionDenied.into(),
        )]));
        assert!(read_chunk(&mut reader, &mut bytes).is_err());
        let mut reader = ScriptedReader(VecDeque::new());
        assert_eq!(read_chunk(&mut reader, &mut bytes).unwrap(), 0);
    }

    #[test]
    fn filesystem_helpers_sort_hash_commit_clean_and_lock() {
        let root = scratch("filesystem");
        let input = root.join("input");
        let output = root.join("output");
        let data = output.join("data");
        std::fs::create_dir_all(input.join("nested")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(input.join("z.bin"), b"z").unwrap();
        std::fs::write(input.join("nested/a.bin"), b"a").unwrap();
        let files = collect_files(&input).unwrap();
        assert_eq!(files[0].logical_path, "nested/a.bin");
        assert_eq!(
            hash_file(&input.join("z.bin")).unwrap(),
            SegmentId(*blake3::hash(b"z").as_bytes())
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(input.join("z.bin"), input.join("link")).unwrap();
            assert!(collect_files(&input).is_err());
            std::fs::remove_file(input.join("link")).unwrap();
        }

        let target = output.join("game.haku");
        let temporary = target.with_extension(format!("haku.part-{}", std::process::id()));
        std::fs::write(&temporary, b"snapshot").unwrap();
        commit_snapshot(&target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"snapshot");

        let active = SegmentId([1; 32]);
        let stale = SegmentId([2; 32]);
        std::fs::write(data.join(segment_file_name(active)), b"active").unwrap();
        std::fs::write(data.join(segment_file_name(stale)), b"stale").unwrap();
        std::fs::write(data.join("notes.txt"), b"keep").unwrap();
        std::fs::create_dir(data.join("directory.taku")).unwrap();
        std::fs::write(data.join("not-a-digest.taku"), b"keep").unwrap();
        cleanup_unreferenced_segments(&data, &[active]).unwrap();
        assert!(data.join(segment_file_name(active)).exists());
        assert!(!data.join(segment_file_name(stale)).exists());
        assert!(data.join("notes.txt").exists());

        let lock = BuildLock::acquire(&output).unwrap();
        assert!(BuildLock::acquire(&output).is_err());
        drop(lock);
        assert!(BuildLock::acquire(&root.join("missing")).is_err());
        recover_interrupted_release(&output).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn packer_rejects_other_publishers_and_staged_source_changes() {
        let root = scratch("publisher-mismatch");
        let input = root.join("input");
        let output = root.join("output");
        std::fs::create_dir_all(&input).unwrap();
        let asset_path = input.join("asset.bin");
        std::fs::write(&asset_path, b"original").unwrap();
        let first = Identity::generate().unwrap();
        let options = PackOptions::new(&input, &output);
        pack_directory(&options, &first).unwrap();
        assert!(pack_directory(&options, &Identity::generate().unwrap()).is_err());

        let files = collect_files(&input).unwrap();
        std::fs::write(&asset_path, b"changed!").unwrap();
        assert!(
            verify_staged_release(
                &output.join("game.haku"),
                &output.join("data"),
                &files,
                &first,
                true,
            )
            .is_err()
        );
        let changed_files = collect_files(&input).unwrap();
        assert!(
            verify_staged_release(
                &output.join("game.haku"),
                &output.join("data"),
                &changed_files,
                &first,
                true,
            )
            .is_err()
        );

        let second_input = root.join("second-input");
        let second_output = root.join("second-output");
        std::fs::create_dir_all(&second_input).unwrap();
        let second_asset = second_input.join("asset.bin");
        std::fs::write(&second_asset, b"before!!").unwrap();
        let second_options = PackOptions::new(&second_input, &second_output);
        let mut changed_during_verification = false;
        assert!(
            pack_directory_with_progress(&second_options, &first, |progress| {
                if progress.phase == "Verifying" && !changed_during_verification {
                    std::fs::write(&second_asset, b"after!!!").unwrap();
                    changed_during_verification = true;
                }
            })
            .is_err()
        );
        assert!(!second_output.join("game.haku").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unfinished_segment_writer_cleans_its_temporary_file() {
        let root = scratch("segment-drop");
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let identity = Identity::generate().unwrap();
        let keys = ProjectKeys::new(identity.root_key(), identity.project_id());
        let segment = NewSegment::create(
            &data,
            0,
            identity.project_id(),
            &keys,
            Availability::Required,
        )
        .unwrap();
        let temporary = segment.temporary_path.clone();
        assert!(temporary.exists());
        drop(segment);
        assert!(!temporary.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_segment_publication_detects_corrupt_existing_content() {
        fn deterministic_segment(
            data: &Path,
            index: usize,
            project_id: ProjectId,
            keys: &ProjectKeys,
        ) -> NewSegment {
            let uid = [7; 16];
            let salt = [8; 16];
            let nonce_prefix = [9; 8];
            let temporary_path = data.join(format!(
                ".segment-{}-{index}.{SEGMENT_FILE_EXTENSION}.part",
                std::process::id()
            ));
            let file = OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .open(&temporary_path)
                .unwrap();
            let mut writer = BufWriter::new(file);
            writer.write_all(&[0; SEGMENT_HEADER_SIZE]).unwrap();
            let header = SegmentHeader {
                project_id,
                segment_uid: uid,
                salt,
                nonce_prefix,
                block_count: 0,
                payload_len: 0,
                file_len: SEGMENT_HEADER_SIZE as u64,
            };
            NewSegment {
                temporary_path,
                data_directory: data.to_path_buf(),
                writer: Some(writer),
                project_id,
                uid,
                salt,
                nonce_prefix,
                key: Aes256Key::new(&keys.segment_key(&header)).unwrap(),
                availability: Availability::Required,
                block_count: 0,
                payload_len: 0,
            }
        }

        let root = scratch("duplicate-segment");
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let identity = Identity::generate().unwrap();
        let keys = ProjectKeys::new(identity.root_key(), identity.project_id());
        let mut first = deterministic_segment(&data, 0, identity.project_id(), &keys);
        first
            .write_block(0, 4, Codec::Raw, b"same".to_vec())
            .unwrap();
        let (record, _) = first.finish().unwrap();
        let final_path = data.join(segment_file_name(record.id));
        std::fs::write(&final_path, b"corrupt").unwrap();

        let mut duplicate = deterministic_segment(&data, 1, identity.project_id(), &keys);
        duplicate
            .write_block(0, 4, Codec::Raw, b"same".to_vec())
            .unwrap();
        assert!(duplicate.finish().is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pack_progress_boundaries_propagate_source_and_commit_failures() {
        let root = scratch("progress-failures");
        let identity = Identity::generate().unwrap();

        let input = root.join("source-change/input");
        let output = root.join("source-change/output");
        std::fs::create_dir_all(&input).unwrap();
        let source = input.join("asset.bin");
        std::fs::write(&source, b"before").unwrap();
        let mut changed = false;
        assert!(
            pack_directory_with_progress(
                &PackOptions::new(&input, &output),
                &identity,
                |progress| {
                    if progress.phase == "Packing" && !changed {
                        std::fs::write(&source, b"after!!").unwrap();
                        changed = true;
                    }
                },
            )
            .is_err()
        );

        let input = root.join("commit/input");
        let output = root.join("commit/output");
        std::fs::create_dir_all(&input).unwrap();
        std::fs::write(input.join("asset.bin"), b"asset").unwrap();
        let target = output.join("game.haku");
        let mut blocked_commit = false;
        assert!(
            pack_directory_with_progress(
                &PackOptions::new(&input, &output),
                &identity,
                |progress| {
                    if progress.phase == "Verifying" && !blocked_commit {
                        std::fs::create_dir(&target).unwrap();
                        blocked_commit = true;
                    }
                },
            )
            .is_err()
        );
        assert!(target.is_dir());

        let input = root.join("segment-open/input");
        let output = root.join("segment-open/output");
        std::fs::create_dir_all(&input).unwrap();
        std::fs::write(input.join("asset.bin"), b"asset").unwrap();
        let collision = output.join("data").join(format!(
            ".segment-{}-0.{SEGMENT_FILE_EXTENSION}.part",
            std::process::id()
        ));
        std::fs::create_dir_all(&collision).unwrap();
        assert!(pack_directory(&PackOptions::new(&input, &output), &identity).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_release_has_no_pages_and_snapshot_length_is_exact() {
        let root = scratch("empty-release");
        let input = root.join("input");
        let output = root.join("output");
        std::fs::create_dir_all(&input).unwrap();
        let identity = Identity::generate().unwrap();
        pack_directory(&PackOptions::new(&input, &output), &identity).unwrap();
        let package = Package::open_directory(
            output.join("game.haku"),
            output.join("data"),
            identity.root_key(),
            identity.public_key(),
            ResourceBudget::cache_disabled(),
        )
        .unwrap();
        assert!(package.list_assets().unwrap().is_empty());

        let snapshot = output.join("game.haku");
        let mut bytes = std::fs::read(&snapshot).unwrap();
        bytes.push(0);
        std::fs::write(&snapshot, bytes).unwrap();
        assert!(
            Package::open_directory(
                &snapshot,
                output.join("data"),
                identity.root_key(),
                identity.public_key(),
                ResourceBudget::cache_disabled(),
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn path_index_rejects_mismatched_inputs() {
        assert!(build_path_slots(&["a".into()], &[], &[0; 32]).is_err());
    }

    #[test]
    fn packer_invariant_validators_cover_success_and_failure_paths() {
        assert_eq!(checked_path_len("asset").unwrap(), 5);
        assert!(checked_path_len(&"a".repeat(u16::MAX as usize + 1)).is_err());
        let source = SourceFile::test("asset", 4);
        validate_source_len(4, &source).unwrap();
        assert!(validate_source_len(3, &source).is_err());
        validate_pair_count(1, 1).unwrap();
        assert!(validate_pair_count(1, 0).is_err());
        validate_catalog_ciphertext_len(16, 16).unwrap();
        assert!(validate_catalog_ciphertext_len(15, 16).is_err());
        assert_eq!(
            segment_physical_offset(1).unwrap(),
            SEGMENT_HEADER_SIZE as u64 + 1
        );
        assert!(segment_physical_offset(u64::MAX).is_err());
    }
}
