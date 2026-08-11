use crate::{Error, Identity, Result};
use hakutaku_core::crypto::{self, Aes256Key, ProjectKeys};
use hakutaku_core::format::{
    AccessClass, Availability, BLOCKS_PER_MAP_PAGE, BlockRef, CatalogData, Codec, EMPTY_PATH_SLOT,
    FileRecord, LayoutKind, PageKind, PageRecord, PathSlot, ProjectId, REUSE_PER_PAGE, ReuseRecord,
    SEGMENT_HEADER_SIZE, SegmentHeader, SegmentId, SegmentRecord, SnapshotHeader, encode_map_page,
    encode_reuse_page, validate_canonical_path,
};
use hakutaku_core::{Package, ResourceBudget};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const STREAM_BLOCK: usize = 256 * 1024;
const BULK_BLOCK: usize = 1024 * 1024;
const HOT_FILE_LIMIT: u64 = 32 * 1024;
const CONTENT_DEFINED_LIMIT: u64 = 64 * 1024 * 1024;
const FASTCDC_MIN: u32 = 32 * 1024;
const FASTCDC_AVG: u32 = 128 * 1024;
const FASTCDC_MAX: u32 = 512 * 1024;
const COMPRESSION_SAVINGS: usize = 64;
const DEFAULT_SEGMENT_TARGET: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct PackOptions {
    pub input_directory: PathBuf,
    pub output_directory: PathBuf,
    pub incremental: bool,
    pub compression_level: i32,
    pub segment_target_bytes: u64,
    /// Canonical asset paths or directory prefixes whose segments may be
    /// installed on demand. Required and deferred blocks are never mixed in
    /// one segment.
    pub deferred_prefixes: Vec<String>,
}

impl PackOptions {
    #[must_use]
    pub fn new(input_directory: impl Into<PathBuf>, output_directory: impl Into<PathBuf>) -> Self {
        Self {
            input_directory: input_directory.into(),
            output_directory: output_directory.into(),
            incremental: true,
            compression_level: 3,
            segment_target_bytes: DEFAULT_SEGMENT_TARGET,
            deferred_prefixes: Vec::new(),
        }
    }

    fn availability(&self, path: &str) -> Availability {
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
pub struct PackProgress {
    pub phase: &'static str,
    pub current_path: Option<String>,
    pub completed_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackReport {
    pub changed: bool,
    pub release_sequence: u64,
    pub file_count: u32,
    pub block_count: u32,
    pub reused_blocks: u32,
    pub new_blocks: u32,
    pub new_segments: u32,
    pub new_segment_bytes: u64,
}

pub fn pack_directory(options: &PackOptions, identity: &Identity) -> Result<PackReport> {
    pack_directory_with_progress(options, identity, |_| {})
}

pub fn pack_directory_with_progress<F>(
    options: &PackOptions,
    identity: &Identity,
    mut progress: F,
) -> Result<PackReport>
where
    F: FnMut(PackProgress),
{
    validate_options(options)?;
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
    if let Some(old) = &old_package
        && old.project_id() != identity.project_id()
    {
        return Err(Error::InvalidInput(
            "output contains a snapshot for another publisher identity".into(),
        ));
    }
    let reuse_index = load_reuse_index(old_package.as_ref())?;
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
        reuse_index,
        current_reuse: HashMap::new(),
        used_existing_segments: BTreeMap::new(),
        new_segment_records: Vec::new(),
        new_segment_ids: Vec::new(),
        current_segment: None,
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
        context.add_file(source)?;
    }
    context.finish_current_segment()?;
    if !context.new_segment_ids.is_empty() {
        sync_directory(&data_directory)?;
    }
    let source_fingerprint = *context.fingerprint.finalize().as_bytes();

    if old_package
        .as_ref()
        .is_some_and(|old| old.source_fingerprint() == source_fingerprint)
    {
        let active_segments = old_package.as_ref().expect("checked above").segment_ids()?;
        let active_release_sequence = old_package
            .as_ref()
            .expect("checked above")
            .release_sequence();
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
    if let Err(error) = verify_staged_release(&staged_snapshot, &data_directory, &files, identity) {
        let _ = std::fs::remove_file(&staged_snapshot);
        return Err(error);
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

struct SourceFile {
    host_path: PathBuf,
    logical_path: String,
    len: u64,
}

#[derive(Clone)]
struct ReusableBlock {
    block: BlockRef,
    segment: SegmentRecord,
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

struct BuildContext<'a> {
    options: &'a PackOptions,
    identity: &'a Identity,
    keys: &'a ProjectKeys,
    reuse_index: HashMap<([u8; 32], Availability), ReusableBlock>,
    current_reuse: HashMap<([u8; 32], Availability), PendingBlock>,
    used_existing_segments: BTreeMap<SegmentId, SegmentRecord>,
    new_segment_records: Vec<SegmentRecord>,
    new_segment_ids: Vec<SegmentId>,
    current_segment: Option<NewSegment>,
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
            .chain(self.new_segment_records.iter().map(|record| record.id))
            .collect()
    }

    fn add_file(&mut self, source: &SourceFile) -> Result<()> {
        let (layout, fixed_block_len, access) = classify(source);
        let availability = self.options.availability(&source.logical_path);
        let first_block = u32::try_from(self.pending_blocks.len())
            .map_err(|_| Error::InvalidInput("too many blocks".into()))?;
        let path_offset = u32::try_from(self.path_pool.len())
            .map_err(|_| Error::InvalidInput("path pool is too large".into()))?;
        let path_len = u16::try_from(source.logical_path.len()).map_err(|_| {
            Error::InvalidInput(format!("path is too long: {}", source.logical_path))
        })?;
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
            match layout {
                LayoutKind::Fixed => {
                    self.add_fixed_file(source, fixed_block_len as usize, availability)?
                }
                LayoutKind::ContentDefined => {
                    self.add_content_defined_file(source, availability)?
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

    fn add_fixed_file(
        &mut self,
        source: &SourceFile,
        block_size: usize,
        availability: Availability,
    ) -> Result<()> {
        let mut reader =
            BufReader::with_capacity(block_size.min(1024 * 1024), File::open(&source.host_path)?);
        let mut buffer = vec![0_u8; block_size];
        let mut logical_offset = 0_u64;
        loop {
            let read = read_chunk(&mut reader, &mut buffer)?;
            if read == 0 {
                break;
            }
            self.add_chunk(logical_offset, &buffer[..read], availability)?;
            logical_offset = logical_offset
                .checked_add(read as u64)
                .ok_or_else(|| Error::InvalidInput("file offset overflow".into()))?;
        }
        if logical_offset != source.len {
            return Err(Error::InvalidInput(format!(
                "file changed while packing: {}",
                source.logical_path
            )));
        }
        Ok(())
    }

    fn add_content_defined_file(
        &mut self,
        source: &SourceFile,
        availability: Availability,
    ) -> Result<()> {
        let bytes = std::fs::read(&source.host_path)?;
        if bytes.len() as u64 != source.len {
            return Err(Error::InvalidInput(format!(
                "file changed while packing: {}",
                source.logical_path
            )));
        }
        for chunk in fastcdc::v2020::FastCDC::new(&bytes, FASTCDC_MIN, FASTCDC_AVG, FASTCDC_MAX) {
            let end = chunk
                .offset
                .checked_add(chunk.length)
                .ok_or_else(|| Error::InvalidInput("FastCDC range overflow".into()))?;
            self.add_chunk(chunk.offset as u64, &bytes[chunk.offset..end], availability)?;
        }
        Ok(())
    }

    fn add_chunk(
        &mut self,
        logical_offset: u64,
        plaintext: &[u8],
        availability: Availability,
    ) -> Result<()> {
        let chunk_id = *blake3::hash(plaintext).as_bytes();
        self.fingerprint.update(&chunk_id);
        self.chunk_ids.push(chunk_id);
        let reuse_key = (chunk_id, availability);
        if let Some(reused) = self.current_reuse.get(&reuse_key).copied() {
            let mut reused = reused;
            reused.reference.logical_offset = logical_offset;
            self.pending_blocks.push(reused);
            self.reused_blocks = self.reused_blocks.saturating_add(1);
            return Ok(());
        }
        if let Some(reused) = self.reuse_index.get(&reuse_key)
            && reused.block.plain_len as usize == plaintext.len()
        {
            let mut reference = reused.block;
            reference.logical_offset = logical_offset;
            self.used_existing_segments
                .entry(reused.segment.id)
                .or_insert_with(|| reused.segment.clone());
            let pending = PendingBlock {
                locator: SegmentLocator::Existing(reused.segment.id),
                reference,
            };
            self.pending_blocks.push(pending);
            self.current_reuse.insert(reuse_key, pending);
            self.reused_blocks = self.reused_blocks.saturating_add(1);
            return Ok(());
        }

        let (codec, encoded) = compress_block(&mut self.compressor, plaintext)?;
        let estimated_stored = encoded
            .len()
            .checked_add(16)
            .ok_or_else(|| Error::InvalidInput("block length overflow".into()))?
            as u64;
        let rotate = self.current_segment.as_ref().is_some_and(|segment| {
            segment.availability != availability
                || (segment.block_count > 0
                    && segment.payload_len.saturating_add(estimated_stored)
                        > self.options.segment_target_bytes)
        });
        if rotate {
            self.finish_current_segment()?;
        }
        if self.current_segment.is_none() {
            let index = self.new_segment_records.len();
            self.current_segment = Some(NewSegment::create(
                &self.options.output_directory.join("data"),
                index,
                self.identity.project_id(),
                self.keys,
                availability,
            )?);
        }
        let segment_index = self.new_segment_records.len();
        let reference = self
            .current_segment
            .as_mut()
            .expect("segment initialized")
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

    fn finish_current_segment(&mut self) -> Result<()> {
        let Some(segment) = self.current_segment.take() else {
            return Ok(());
        };
        let (record, bytes) = segment.finish()?;
        self.new_segment_bytes = self.new_segment_bytes.saturating_add(bytes);
        self.new_segment_ids.push(record.id);
        self.new_segment_records.push(record);
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
        segments.extend(self.new_segment_records.iter().cloned());
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
                SegmentLocator::New(index) => *self
                    .new_segment_ids
                    .get(index)
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
        let reuse_records: Vec<ReuseRecord> = self
            .chunk_ids
            .iter()
            .copied()
            .zip(block_refs.iter().copied())
            .map(|(chunk_id, block)| ReuseRecord { chunk_id, block })
            .collect();
        if block_refs.len() != reuse_records.len() {
            return Err(Error::InvalidInput("block/reuse record mismatch".into()));
        }

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
            &reuse_records,
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
        if catalog_ciphertext.len() as u64 != catalog_stored_len {
            return Err(Error::InvalidInput("catalog ciphertext length".into()));
        }
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
            new_segments: u32::try_from(self.new_segment_records.len())
                .map_err(|_| Error::InvalidInput("too many new segments".into()))?,
            new_segment_bytes: self.new_segment_bytes,
        })
    }
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
        let temporary_path =
            data_directory.join(format!(".segment-{}-{index}.hks.part", std::process::id()));
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
        let physical_offset = (SEGMENT_HEADER_SIZE as u64)
            .checked_add(self.payload_len)
            .ok_or_else(|| Error::InvalidInput("segment offset overflow".into()))?;
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
        let final_path = self.data_directory.join(format!("{id}.hks"));
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

fn load_reuse_index(
    old: Option<&Package>,
) -> Result<HashMap<([u8; 32], Availability), ReusableBlock>> {
    let Some(old) = old else {
        return Ok(HashMap::new());
    };
    let mut index = HashMap::new();
    for reuse in old.reuse_records()? {
        let segment = old.segment_record(reuse.block.segment_ordinal)?;
        index
            .entry((reuse.chunk_id, segment.availability))
            .or_insert(ReusableBlock {
                block: reuse.block,
                segment,
            });
    }
    Ok(index)
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
    reuse_records: &[ReuseRecord],
    compression_level: i32,
) -> Result<EncryptedPages> {
    let mut plaintext_pages = Vec::new();
    for (page_index, records) in block_refs.chunks(BLOCKS_PER_MAP_PAGE).enumerate() {
        let first = u32::try_from(page_index * BLOCKS_PER_MAP_PAGE)
            .map_err(|_| Error::InvalidInput("map page index".into()))?;
        plaintext_pages.push((PageKind::BlockMap, encode_map_page(first, records)?));
    }
    let map_page_count = u32::try_from(plaintext_pages.len())
        .map_err(|_| Error::InvalidInput("too many map pages".into()))?;
    for (page_index, records) in reuse_records.chunks(REUSE_PER_PAGE).enumerate() {
        let first = u32::try_from(page_index * REUSE_PER_PAGE)
            .map_err(|_| Error::InvalidInput("reuse page index".into()))?;
        plaintext_pages.push((PageKind::Reuse, encode_reuse_page(first, records)?));
    }
    let reuse_page_count = u32::try_from(plaintext_pages.len())
        .map_err(|_| Error::InvalidInput("too many pages".into()))?
        .checked_sub(map_page_count)
        .ok_or_else(|| Error::InvalidInput("page count".into()))?;

    let mut compressor = zstd::bulk::Compressor::new(compression_level)?;
    compressor.include_checksum(false)?;
    compressor.include_dictid(false)?;
    let mut directory = Vec::with_capacity(plaintext_pages.len());
    let mut ciphertexts = Vec::with_capacity(plaintext_pages.len());
    let mut relative_offset = 0_u64;
    for (index, (kind, plaintext)) in plaintext_pages.into_iter().enumerate() {
        let plain_len = u32::try_from(plaintext.len())
            .map_err(|_| Error::InvalidInput("page plaintext length".into()))?;
        let (codec, mut encoded) = compress_bytes(&mut compressor, &plaintext)?;
        let stored_len = u32::try_from(encoded.len() + 16)
            .map_err(|_| Error::InvalidInput("page ciphertext length".into()))?;
        let nonce_ordinal = u32::try_from(index + 1)
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
            relative_offset,
            stored_len,
            plain_len,
            digest,
        });
        relative_offset = relative_offset
            .checked_add(u64::from(stored_len))
            .ok_or_else(|| Error::InvalidInput("page region overflow".into()))?;
        ciphertexts.push(encoded);
    }
    Ok(EncryptedPages {
        directory,
        ciphertexts,
        map_count: map_page_count,
        reuse_count: reuse_page_count,
    })
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

fn classify(source: &SourceFile) -> (LayoutKind, u32, AccessClass) {
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

fn collect_files(root: &Path) -> Result<Vec<SourceFile>> {
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
) -> Result<()> {
    let package = Package::open_directory(
        snapshot,
        data_directory,
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::memory_constrained(),
    )?;
    package.verify_segments()?;
    let mut expected = vec![0_u8; 1024 * 1024];
    let mut actual = vec![0_u8; 1024 * 1024];
    for source in files {
        let mut source_file = File::open(&source.host_path)?;
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
                && name.ends_with(".hks.part")
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
    let active_names: HashSet<String> = active.iter().map(|id| format!("{id}.hks")).collect();
    let mut removed = false;
    for entry in std::fs::read_dir(data_directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let bytes = name.as_bytes();
        if bytes.len() == 68
            && &bytes[64..] == b".hks"
            && bytes[..64].iter().all(u8::is_ascii_hexdigit)
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

fn validate_options(options: &PackOptions) -> Result<()> {
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
    for prefix in &options.deferred_prefixes {
        validate_canonical_path(prefix).map_err(|_| {
            Error::InvalidInput(format!("deferred prefix is not canonical: {prefix:?}"))
        })?;
    }
    Ok(())
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
        std::fs::write(output.join("data/.segment-7-0.hks.part"), b"partial").unwrap();
        std::fs::write(output.join("data/notes.txt"), b"keep").unwrap();

        recover_interrupted_release(&output).unwrap();

        assert_eq!(
            std::fs::read(output.join("game.haku")).unwrap(),
            b"previous"
        );
        assert!(!output.join("game.haku.part-7").exists());
        assert!(!output.join("data/.segment-7-0.hks.part").exists());
        assert!(output.join("data/notes.txt").is_file());
        std::fs::remove_dir_all(output).unwrap();
    }
}
