use hakutaku_core::crypto::{self, Aes256Key, ProjectKeys};
use hakutaku_core::format::{Catalog, Codec, SnapshotHeader, map_page_record, validate_map_page};
use hakutaku_core::{
    AccessClass, Availability, Error as CoreError, OpenPolicy, Package, PositionedFile,
    ResourceBudget, SegmentId, SegmentSource,
};
use hakutaku_pack::{Identity, PackOptions, pack_directory};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "hakutaku-roundtrip-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn full_incremental_and_unchanged_roundtrip() {
    let scratch = Scratch::new();
    let input = scratch.0.join("input");
    let output = scratch.0.join("release");
    std::fs::create_dir_all(input.join("script")).unwrap();
    std::fs::create_dir_all(input.join("video")).unwrap();
    std::fs::write(input.join("empty.txt"), []).unwrap();
    std::fs::write(
        input.join("script/main.json"),
        "dialogue line\n".repeat(20_000),
    )
    .unwrap();
    let video = pseudo_random_bytes(700_000);
    std::fs::write(input.join("video/opening.mp4"), &video).unwrap();

    let identity_path = scratch.0.join("publisher.hakutaku-key");
    Identity::generate().unwrap().save(&identity_path).unwrap();
    let identity = Identity::load(&identity_path).unwrap();
    let mut options = PackOptions::new(&input, &output);
    options.segment_target_bytes = 1024 * 1024;
    let first = pack_directory(&options, &identity).unwrap();
    assert!(first.changed);
    assert!(first.new_blocks > 0);
    assert_eq!(first.reused_blocks, 0);
    assert_release_matches(&input, &output, &identity);

    std::fs::write(
        input.join("script/main.json"),
        format!("{}changed\n", "dialogue line\n".repeat(20_000)),
    )
    .unwrap();
    let second = pack_directory(&options, &identity).unwrap();
    assert!(second.changed);
    assert!(second.reused_blocks > 0);
    assert!(second.new_blocks > 0);
    assert_release_matches(&input, &output, &identity);

    let unchanged = pack_directory(&options, &identity).unwrap();
    assert!(!unchanged.changed);
    assert_eq!(unchanged.release_sequence, second.release_sequence);
}

#[test]
fn streaming_cursor_keeps_the_previous_block_for_short_backward_seeks() {
    let scratch = Scratch::new();
    let input = scratch.0.join("input");
    let output = scratch.0.join("release");
    std::fs::create_dir_all(&input).unwrap();
    let video = pseudo_random_bytes(700_000);
    std::fs::write(input.join("opening.mp4"), &video).unwrap();
    let normal = pseudo_random_bytes(700_001);
    std::fs::write(input.join("normal.bin"), &normal).unwrap();

    let identity = Identity::generate().unwrap();
    pack_directory(&PackOptions::new(&input, &output), &identity).unwrap();
    let directory_package = Package::open_directory(
        output.join("game.haku"),
        output.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::cache_disabled(),
    )
    .unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let segments = directory_package
        .segment_ids()
        .unwrap()
        .into_iter()
        .map(|id| {
            let bytes = std::fs::read(output.join("data").join(format!("{id}.taku"))).unwrap();
            (id, Arc::<[u8]>::from(bytes))
        })
        .collect();
    let package = Package::open(
        Arc::new(MemoryFile(Arc::from(
            std::fs::read(output.join("game.haku")).unwrap(),
        ))),
        Arc::new(CountingSegments {
            segments,
            reads: Arc::clone(&reads),
        }),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget {
            prefetch_cache_bytes: 256 * 1024,
            ..ResourceBudget::cache_disabled()
        },
    )
    .unwrap();

    let asset = package.asset("opening.mp4").unwrap();
    let mut normal_cursor = package.asset("normal.bin").unwrap().cursor();
    let mut decoded_normal = Vec::new();
    normal_cursor.read_to_end(&mut decoded_normal).unwrap();
    assert_eq!(decoded_normal, normal);
    let mut normal_session = package.asset("normal.bin").unwrap().read_session();
    let mut normal_byte = [0_u8];
    for offset in [0, 300_000, 600_000, 0] {
        normal_session.read_at(offset, &mut normal_byte).unwrap();
        assert_eq!(normal_byte[0], normal[offset as usize]);
    }
    assert!(asset.prefetch_range(asset.len() + 1, 1).is_err());
    asset.prefetch_range(asset.len(), 1).unwrap();
    asset.prefetch_range(0, 0).unwrap();
    directory_package
        .asset("opening.mp4")
        .unwrap()
        .prefetch_range(0, 1)
        .unwrap();
    let mut cursor = asset.cursor();
    let mut byte = [0_u8];
    cursor.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], video[0]);
    cursor.seek(SeekFrom::Start(256 * 1024)).unwrap();
    cursor.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], video[256 * 1024]);
    let reads_after_two_blocks = reads.load(Ordering::Relaxed);

    cursor.seek(SeekFrom::Start(0)).unwrap();
    cursor.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], video[0]);
    assert_eq!(reads.load(Ordering::Relaxed), reads_after_two_blocks);

    package.trim();
    let mut session = asset.read_session();
    session.read_at(0, &mut byte).unwrap();
    session.read_at(256 * 1024, &mut byte).unwrap();
    let session_reads_after_two_blocks = reads.load(Ordering::Relaxed);
    session.read_at(0, &mut byte).unwrap();
    assert_eq!(byte[0], video[0]);
    assert_eq!(
        reads.load(Ordering::Relaxed),
        session_reads_after_two_blocks
    );

    asset.prefetch_range(512 * 1024, 1).unwrap();
    asset.prefetch_range(512 * 1024, 1).unwrap();
    let reads_after_prefetch = reads.load(Ordering::Relaxed);
    let mut prefetched_cursor = asset.cursor();
    prefetched_cursor.seek(SeekFrom::Start(512 * 1024)).unwrap();
    prefetched_cursor.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], video[512 * 1024]);
    assert_eq!(reads.load(Ordering::Relaxed), reads_after_prefetch);
}

#[test]
fn caller_policy_rejects_a_validly_signed_rollback() {
    let scratch = Scratch::new();
    let input = scratch.0.join("input");
    let output = scratch.0.join("release");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::write(input.join("script.json"), b"version one").unwrap();
    let identity = Identity::generate().unwrap();
    pack_directory(&PackOptions::new(&input, &output), &identity).unwrap();

    let rejected = Package::open_directory_with_policy(
        output.join("game.haku"),
        output.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::cache_disabled(),
        OpenPolicy::requiring(2),
    );
    assert!(matches!(
        rejected,
        Err(CoreError::ReleaseRollback {
            minimum: 2,
            actual: 1
        })
    ));
}

#[test]
fn runtime_material_reconstructs_the_identity_key() {
    let identity = Identity::generate().unwrap();
    let material = identity.runtime_key_material().unwrap();
    let reconstructed =
        std::array::from_fn(|index| material.key_share_a[index] ^ material.key_share_b[index]);
    assert_eq!(reconstructed, identity.root_key());
    assert_eq!(material.public_key, identity.public_key());
}

#[test]
fn concurrent_identity_creation_never_overwrites_a_winner() {
    let scratch = Scratch::new();
    let path = scratch.0.join("publisher.hakutaku-key");
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            std::thread::spawn(move || {
                let identity = Identity::generate().unwrap();
                barrier.wait();
                identity.save(path)
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    Identity::load(path).unwrap();
}

#[test]
fn committed_snapshot_removes_only_unreferenced_segment_files() {
    let scratch = Scratch::new();
    let input = scratch.0.join("input");
    let output = scratch.0.join("release");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::write(input.join("asset.bin"), vec![1_u8; 100_000]).unwrap();
    let identity = Identity::generate().unwrap();
    let options = PackOptions::new(&input, &output);
    pack_directory(&options, &identity).unwrap();

    let data = output.join("data");
    let old_segments = segment_files(&data);
    assert_eq!(old_segments.len(), 1);
    std::fs::write(data.join("notes.txt"), "keep me").unwrap();

    std::fs::write(input.join("asset.bin"), vec![2_u8; 100_000]).unwrap();
    pack_directory(&options, &identity).unwrap();

    let new_segments = segment_files(&data);
    assert_eq!(new_segments.len(), 1);
    assert_ne!(old_segments, new_segments);
    assert!(data.join("notes.txt").is_file());
}

#[test]
fn first_release_deduplicates_identical_chunks() {
    let scratch = Scratch::new();
    let input = scratch.0.join("input");
    let output = scratch.0.join("release");
    std::fs::create_dir_all(&input).unwrap();
    let repeated = pseudo_random_bytes(100_000);
    std::fs::write(input.join("a.bin"), &repeated).unwrap();
    std::fs::write(input.join("b.bin"), &repeated).unwrap();
    let identity = Identity::generate().unwrap();

    let report = pack_directory(&PackOptions::new(&input, &output), &identity).unwrap();

    assert_eq!(report.block_count, 2);
    assert_eq!(report.new_blocks, 1);
    assert_eq!(report.reused_blocks, 1);
    let package = Package::open_directory(
        output.join("game.haku"),
        output.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::memory_constrained(),
    )
    .unwrap();
    assert_eq!(package.asset("a.bin").unwrap().read().unwrap(), repeated);
    assert_eq!(
        package.asset("b.bin").unwrap().read().unwrap(),
        package.asset("a.bin").unwrap().read().unwrap()
    );
}

#[test]
fn deferred_segments_are_isolated_and_can_arrive_after_snapshot_open() {
    let scratch = Scratch::new();
    let input = scratch.0.join("input");
    let output = scratch.0.join("release");
    std::fs::create_dir_all(input.join("core")).unwrap();
    std::fs::create_dir_all(input.join("dlc")).unwrap();
    std::fs::write(input.join("core/config.json"), b"{\"start\":\"main\"}").unwrap();
    std::fs::write(input.join("dlc/movie.mp4"), pseudo_random_bytes(700_000)).unwrap();
    let identity = Identity::generate().unwrap();
    let mut options = PackOptions::new(&input, &output);
    options.deferred_prefixes.push("dlc".into());
    pack_directory(&options, &identity).unwrap();

    let local = Package::open_directory(
        output.join("game.haku"),
        output.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::memory_constrained(),
    )
    .unwrap();
    let assets = local.list_assets().unwrap();
    assert_eq!(assets[0].access, AccessClass::Hot);
    assert_eq!(assets[1].access, AccessClass::Streaming);
    let segments = local.list_segments().unwrap();
    assert!(
        segments
            .iter()
            .any(|segment| segment.availability == Availability::Required)
    );
    let deferred = segments
        .iter()
        .find(|segment| segment.availability == Availability::Deferred)
        .copied()
        .unwrap();

    let snapshot: Arc<[u8]> = std::fs::read(output.join("game.haku")).unwrap().into();
    let required_segments = segments
        .iter()
        .filter(|segment| segment.availability == Availability::Required)
        .map(|segment| {
            let bytes: Arc<[u8]> =
                std::fs::read(output.join("data").join(format!("{}.taku", segment.id)))
                    .unwrap()
                    .into();
            (segment.id, bytes)
        })
        .collect();
    let package = Package::open(
        Arc::new(MemoryFile(snapshot)),
        Arc::new(MemorySegments(required_segments)),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::memory_constrained(),
    )
    .unwrap();
    assert_eq!(
        package.asset("core/config.json").unwrap().read().unwrap(),
        b"{\"start\":\"main\"}"
    );
    assert!(matches!(
        package.asset("dlc/movie.mp4").unwrap().read(),
        Err(CoreError::SegmentUnavailable(id)) if id == deferred.id
    ));
}

#[test]
fn snapshot_signature_rejects_tampering() {
    let scratch = Scratch::new();
    let input = scratch.0.join("input");
    let output = scratch.0.join("release");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::write(input.join("asset.bin"), pseudo_random_bytes(100_000)).unwrap();
    let identity = Identity::generate().unwrap();
    pack_directory(&PackOptions::new(&input, &output), &identity).unwrap();

    let snapshot = output.join("game.haku");
    let mut bytes = std::fs::read(&snapshot).unwrap();
    bytes[220] ^= 0x80;
    std::fs::write(&snapshot, bytes).unwrap();
    let result = Package::open_directory(
        &snapshot,
        output.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::cache_disabled(),
    );
    assert!(result.is_err());
}

fn segment_files(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            name.ends_with(".taku").then_some(name)
        })
        .collect();
    names.sort();
    names
}

#[test]
fn extracted_content_key_cannot_forge_the_signed_block_commitment() {
    let scratch = Scratch::new();
    let input = scratch.0.join("input");
    let output = scratch.0.join("release");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::write(input.join("asset.bin"), pseudo_random_bytes(100_000)).unwrap();
    let identity = Identity::generate().unwrap();
    pack_directory(&PackOptions::new(&input, &output), &identity).unwrap();

    let snapshot_bytes = std::fs::read(output.join("game.haku")).unwrap();
    let header = SnapshotHeader::parse(&snapshot_bytes[..4096]).unwrap();
    let keys = ProjectKeys::new(identity.root_key(), identity.project_id());
    let snapshot_key_bytes = keys.snapshot_key(&header.snapshot_salt);
    let snapshot_key = Aes256Key::new(&snapshot_key_bytes).unwrap();
    let catalog_end = 4096 + header.catalog_stored_len as usize;
    let mut catalog_ciphertext = snapshot_bytes[4096..catalog_end].to_vec();
    snapshot_key
        .open(
            crypto::nonce(header.nonce_prefix, 0),
            &crypto::catalog_aad(&header),
            &mut catalog_ciphertext,
            "test catalog",
        )
        .unwrap();
    let catalog_plain =
        zstd::bulk::decompress(&catalog_ciphertext, header.catalog_plain_len as usize).unwrap();
    let catalog = Catalog::parse(Arc::from(catalog_plain)).unwrap();
    let page = catalog.page(0).unwrap();
    let page_start = header.page_region_offset as usize + page.relative_offset as usize;
    let mut page_ciphertext =
        snapshot_bytes[page_start..page_start + page.stored_len as usize].to_vec();
    snapshot_key
        .open(
            crypto::nonce(header.nonce_prefix, page.nonce_ordinal),
            &crypto::page_aad(
                header.project_id,
                header.release_sequence,
                page.kind,
                page.codec,
                page.nonce_ordinal,
                page.stored_len,
                page.plain_len,
            ),
            &mut page_ciphertext,
            "test page",
        )
        .unwrap();
    let page_plain = match page.codec {
        Codec::Raw => page_ciphertext,
        Codec::Zstd => zstd::bulk::decompress(&page_ciphertext, page.plain_len as usize).unwrap(),
    };
    validate_map_page(&page_plain, 0).unwrap();
    let block = map_page_record(&page_plain, 0).unwrap();
    assert_eq!(block.codec, Codec::Raw);
    let segment = catalog.segment(u32::from(block.segment_ordinal)).unwrap();
    let segment_path = output.join("data").join(format!("{}.taku", segment.id));
    let mut segment_bytes = std::fs::read(&segment_path).unwrap();
    let segment_header =
        hakutaku_core::format::SegmentHeader::parse(&segment_bytes[..4096]).unwrap();
    let segment_key_bytes = keys.segment_key(&segment_header);
    let segment_key = Aes256Key::new(&segment_key_bytes).unwrap();
    let start = block.physical_offset as usize;
    let end = start + block.stored_len as usize;
    let mut ciphertext = segment_bytes[start..end].to_vec();
    let aad = crypto::block_aad(
        header.project_id,
        &segment.uid,
        block.segment_block_ordinal,
        block.codec,
        block.stored_len,
        block.plain_len,
    );
    segment_key
        .open(
            crypto::nonce(segment.nonce_prefix, block.segment_block_ordinal),
            &aad,
            &mut ciphertext,
            "test block",
        )
        .unwrap();
    ciphertext[0] ^= 1;
    segment_key
        .seal(
            crypto::nonce(segment.nonce_prefix, block.segment_block_ordinal),
            &aad,
            &mut ciphertext,
        )
        .unwrap();
    segment_bytes[start..end].copy_from_slice(&ciphertext);
    std::fs::write(&segment_path, segment_bytes).unwrap();

    let package = Package::open_directory(
        output.join("game.haku"),
        output.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::cache_disabled(),
    )
    .unwrap();
    assert!(package.verify_segments().is_err());
    assert!(package.asset("asset.bin").unwrap().read().is_err());
}

fn assert_release_matches(input: &Path, output: &Path, identity: &Identity) {
    let package = Package::open_directory(
        output.join("game.haku"),
        output.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::default(),
    )
    .unwrap();
    assert_eq!(package.project_id(), identity.project_id());
    assert!(package.release_sequence() > 0);
    assert_ne!(package.source_fingerprint(), [0; 32]);
    package.verify_segments().unwrap();
    assert!(package.contains_asset("empty.txt").unwrap());
    assert!(!package.contains_asset("missing.txt").unwrap());
    assert!(matches!(
        package.asset("missing.txt"),
        Err(CoreError::AssetNotFound)
    ));
    let empty = package.asset("empty.txt").unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert_eq!(empty.file_index(), 0);
    assert_eq!(empty.read().unwrap(), []);
    assert_eq!(empty.read_at(0, &mut [0; 1]).unwrap(), 0);
    assert!(empty.read_at(1, &mut [0; 1]).is_err());
    assert!(empty.cursor().is_empty());
    assert_eq!(
        package.asset("script/main.json").unwrap().read().unwrap(),
        std::fs::read(input.join("script/main.json")).unwrap()
    );
    let mut script_cursor = package.asset("script/main.json").unwrap().cursor();
    let mut streamed_script = Vec::new();
    script_cursor.read_to_end(&mut streamed_script).unwrap();
    assert_eq!(
        streamed_script,
        std::fs::read(input.join("script/main.json")).unwrap()
    );
    assert_eq!(
        package.asset("video/opening.mp4").unwrap().read().unwrap(),
        std::fs::read(input.join("video/opening.mp4")).unwrap()
    );
    let expected_video = std::fs::read(input.join("video/opening.mp4")).unwrap();
    let video_asset = package.asset("video/opening.mp4").unwrap();
    assert!(!video_asset.is_empty());
    let mut cursor = video_asset.cursor();
    assert_eq!(cursor.len(), expected_video.len() as u64);
    assert_eq!(cursor.position(), 0);
    assert!(cursor.seek(SeekFrom::Current(-1)).is_err());
    assert_eq!(
        cursor.seek(SeekFrom::End(-32)).unwrap(),
        expected_video.len() as u64 - 32
    );
    assert_eq!(
        cursor.seek(SeekFrom::Current(16)).unwrap(),
        expected_video.len() as u64 - 16
    );
    assert_eq!(cursor.seek(SeekFrom::Start(0)).unwrap(), 0);
    assert!(
        cursor
            .seek(SeekFrom::Start(expected_video.len() as u64 + 1))
            .is_err()
    );
    assert_eq!(cursor.position(), 0);
    let mut actual_video = Vec::new();
    let mut small = [0_u8; 32 * 1024];
    loop {
        let read = cursor.read(&mut small).unwrap();
        if read == 0 {
            break;
        }
        actual_video.extend_from_slice(&small[..read]);
    }
    assert_eq!(actual_video, expected_video);
    let segments = package.segment_ids().unwrap();
    assert!(!segments.is_empty());
    assert_eq!(package.segment_record(0).unwrap().id, segments[0]);
    assert!(package.segment_record(u16::MAX).is_err());
    assert_eq!(package.list_segments().unwrap().len(), segments.len());
    package.trim();
}

fn pseudo_random_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x4d59_5df4_d0f3_3173_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

struct MemoryFile(Arc<[u8]>);

impl PositionedFile for MemoryFile {
    fn len(&self) -> hakutaku_core::Result<u64> {
        Ok(self.0.len() as u64)
    }

    fn read_exact_at(&self, offset: u64, destination: &mut [u8]) -> hakutaku_core::Result<()> {
        let start = usize::try_from(offset).map_err(|_| CoreError::InvalidRange)?;
        let end = start
            .checked_add(destination.len())
            .ok_or(CoreError::InvalidRange)?;
        let source = self.0.get(start..end).ok_or(CoreError::InvalidRange)?;
        destination.copy_from_slice(source);
        Ok(())
    }
}

struct MemorySegments(HashMap<SegmentId, Arc<[u8]>>);

impl SegmentSource for MemorySegments {
    fn open(&self, id: SegmentId) -> hakutaku_core::Result<Arc<dyn PositionedFile>> {
        self.0
            .get(&id)
            .cloned()
            .map(|bytes| Arc::new(MemoryFile(bytes)) as Arc<dyn PositionedFile>)
            .ok_or(CoreError::SegmentUnavailable(id))
    }
}

struct CountingSegments {
    segments: HashMap<SegmentId, Arc<[u8]>>,
    reads: Arc<AtomicUsize>,
}

impl SegmentSource for CountingSegments {
    fn open(&self, id: SegmentId) -> hakutaku_core::Result<Arc<dyn PositionedFile>> {
        self.segments
            .get(&id)
            .cloned()
            .map(|bytes| {
                Arc::new(CountingFile {
                    bytes,
                    reads: Arc::clone(&self.reads),
                }) as Arc<dyn PositionedFile>
            })
            .ok_or(CoreError::SegmentUnavailable(id))
    }
}

struct CountingFile {
    bytes: Arc<[u8]>,
    reads: Arc<AtomicUsize>,
}

impl PositionedFile for CountingFile {
    fn len(&self) -> hakutaku_core::Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn read_exact_at(&self, offset: u64, destination: &mut [u8]) -> hakutaku_core::Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let start = usize::try_from(offset).map_err(|_| CoreError::InvalidRange)?;
        let end = start
            .checked_add(destination.len())
            .ok_or(CoreError::InvalidRange)?;
        destination.copy_from_slice(self.bytes.get(start..end).ok_or(CoreError::InvalidRange)?);
        Ok(())
    }
}
