//! Opt-in design experiments; timings use warm host filesystem caches.
use super::*;
use hakutaku_core::format::{SNAPSHOT_HEADER_SIZE, SnapshotHeader};

pub(super) fn run(mode: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "design_layout snapshot_header={} segment_header={} path_slot={} block_map_records={} max_segments={}",
        hakutaku_core::format::SNAPSHOT_HEADER_SIZE,
        hakutaku_core::format::SEGMENT_HEADER_SIZE,
        hakutaku_core::format::PATH_SLOT_SIZE,
        hakutaku_core::format::BLOCKS_PER_MAP_PAGE,
        hakutaku_core::format::MAX_SEGMENTS
    );
    let fixture = Fixture::new()?;
    let identity = Identity::generate()?;
    match mode {
        "metadata" => {
            for count in benchmark_counts()? {
                run_scale(&fixture.path, count, &identity)?;
            }
        }
        "manifest" => manifest(&fixture.path, &identity)?,
        "real" => real_assets(&fixture.path, &identity)?,
        "patches" => patch_history(&fixture.path, &identity)?,
        "stress" => concurrent_streams(&fixture.path, &identity)?,
        _ => {
            return Err(
                "HAKUTAKU_DESIGN_BENCH must be metadata, manifest, real, patches, or stress".into(),
            );
        }
    }
    Ok(())
}

pub(super) fn metadata_metrics(
    release: &Path,
    identity: &Identity,
    package: &Package,
    count: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = vec![0; SNAPSHOT_HEADER_SIZE];
    File::open(release.join("game.haku"))?.read_exact(&mut bytes)?;
    let header = SnapshotHeader::parse(&bytes)?;
    println!(
        "metadata assets={count} catalog_plain_bytes={} catalog_stored_bytes={} snapshot_bytes={} pages={}",
        header.catalog_plain_len,
        header.catalog_stored_len,
        std::fs::metadata(release.join("game.haku"))?.len(),
        header.page_count,
    );
    let paths: Vec<_> = (0..100_000)
        .map(|index| voice_path(index * 7919 % (count - FIXED_ASSETS)))
        .collect();
    let missing: Vec<_> = paths.iter().map(|path| format!("{path}.missing")).collect();
    for pass in 0..7 {
        let started = Instant::now();
        let opened = open_counted(release, identity, Counters::default())?;
        println!(
            "metadata_open pass={pass} ms={:.6}",
            milliseconds(started.elapsed())
        );
        std::hint::black_box(opened);
        for (kind, paths) in [("hit", &paths), ("miss", &missing)] {
            let started = Instant::now();
            for path in paths {
                let found = package.contains_asset(path)?;
                assert_eq!(found, kind == "hit");
            }
            println!(
                "metadata_lookup kind={kind} pass={pass} count={} ms={:.6}",
                paths.len(),
                milliseconds(started.elapsed())
            );
        }
    }
    Ok(())
}

fn copy_shipping_files(source: &Path, destination: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_shipping_files(&entry.path(), &target)?;
        } else if kind.is_file()
            && matches!(
                entry.path().extension().and_then(|ext| ext.to_str()),
                Some("opus" | "webp" | "shou")
            )
        {
            std::fs::create_dir_all(destination)?;
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn real_assets(root: &Path, identity: &Identity) -> Result<(), Box<dyn std::error::Error>> {
    let source = std::env::var_os("HAKUTAKU_REAL_ASSETS")
        .ok_or("set HAKUTAKU_REAL_ASSETS to the source project; only Opus/WebP/shou are copied")?;
    let input = root.join("real-input");
    let release = root.join("real-release");
    copy_shipping_files(Path::new(&source), &input)?;
    let started = Instant::now();
    let report = pack_directory(&PackOptions::new(&input, &release), identity)?;
    println!(
        "real_pack ms={:.3} files={} blocks={} reused={} segment_bytes={} snapshot_bytes={}",
        milliseconds(started.elapsed()),
        report.file_count,
        report.block_count,
        report.reused_blocks,
        report.retained_segment_bytes,
        std::fs::metadata(release.join("game.haku"))?.len()
    );
    let counters = Counters::default();
    let package = open_counted(&release, identity, counters.clone())?;
    let assets = package.list_assets()?;
    let mut expected = Vec::new();
    for asset in assets {
        expected.push((
            package.asset(&asset.path)?,
            asset.path.clone(),
            std::fs::read(input.join(&asset.path))?,
        ));
    }
    println!(
        "real_fixture files={} logical_bytes={}",
        expected.len(),
        expected.iter().map(|entry| entry.2.len()).sum::<usize>()
    );
    for pass in 0..5 {
        for workload in ["head4k", "sequential64k", "audio_seek4k", "image_seek4k"] {
            package.trim();
            counters.reset();
            let started = Instant::now();
            let mut logical = 0_u64;
            let mut buffer = vec![0_u8; 64 * 1024];
            for (asset, path, bytes) in &expected {
                let mut reader = asset.read_session();
                match workload {
                    "head4k" => {
                        let read = reader.read_at(0, &mut buffer[..4096])?;
                        assert_eq!(&buffer[..read], &bytes[..read]);
                        logical += read as u64;
                    }
                    "sequential64k" => {
                        let mut offset = 0;
                        while offset < bytes.len() {
                            let read = reader.read_at(offset as u64, &mut buffer)?;
                            assert!(read > 0);
                            assert_eq!(&buffer[..read], &bytes[offset..offset + read]);
                            logical += read as u64;
                            offset += read;
                        }
                    }
                    _ => {
                        let extension = if workload == "audio_seek4k" {
                            ".opus"
                        } else {
                            ".webp"
                        };
                        if !path.ends_with(extension) || bytes.len() <= 4096 {
                            continue;
                        }
                        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
                        for _ in 0..128 {
                            state = xorshift(state);
                            let offset = state as usize % (bytes.len() - 4096);
                            let read = reader.read_at(offset as u64, &mut buffer[..4096])?;
                            assert_eq!(read, 4096);
                            assert_eq!(&buffer[..read], &bytes[offset..offset + read]);
                            logical += read as u64;
                        }
                    }
                }
            }
            println!(
                "real_read kind={workload} pass={pass} ms={:.6} logical_bytes={logical} snapshot_bytes={} segment_bytes={} calls={}",
                milliseconds(started.elapsed()),
                counters.snapshot.load(Ordering::Relaxed),
                counters.segments.load(Ordering::Relaxed),
                counters.calls.load(Ordering::Relaxed)
            );
        }
    }
    Ok(())
}

fn patch_history(root: &Path, identity: &Identity) -> Result<(), Box<dyn std::error::Error>> {
    const FILES: usize = 200;
    let input = root.join("patch-input");
    let release = root.join("patch-release");
    std::fs::create_dir_all(&input)?;
    for index in 0..FILES {
        write_fixture(
            &input.join(format!("voice-{index:04}.opus")),
            64,
            index as u64 + 7,
        )?;
    }
    let options = PackOptions::new(&input, &release);
    pack_directory(&options, identity)?;
    let updates =
        std::env::var("HAKUTAKU_PATCH_UPDATES").map_or(Ok(140), |value| value.parse::<usize>())?;
    let mut complete = 0;
    let mut peak_segments = 0;
    let mut peak_retained = 0;
    let mut consolidations = 0;
    for index in 0..updates {
        write_fixture(
            &input.join(format!("voice-{:04}.opus", index % FILES)),
            64,
            index as u64 + 1000,
        )?;
        let started = Instant::now();
        match pack_directory(&options, identity) {
            Ok(report) => {
                complete += 1;
                let package = Package::open_directory(
                    release.join("game.haku"),
                    release.join("data"),
                    identity.root_key(),
                    identity.public_key(),
                    ResourceBudget::default(),
                    OpenPolicy::TrustFirstRelease,
                )?;
                let segment_count = package.list_segments()?.len();
                peak_segments = peak_segments.max(segment_count);
                peak_retained = peak_retained.max(report.retained_segment_bytes);
                consolidations += usize::from(report.new_blocks > 1);
                assert_eq!(package.release_sequence(), index as u64 + 2);
                assert_eq!(package.list_assets()?.len(), FILES);
                if index % 64 == 0 || index + 1 == updates {
                    println!(
                        "patch step={} ms={:.3} logical_bytes={} segments={segment_count} retained_bytes={} stranded_bytes={} snapshot_bytes={}",
                        index + 1,
                        milliseconds(started.elapsed()),
                        FILES * 64,
                        report.retained_segment_bytes,
                        report.stranded_segment_bytes,
                        std::fs::metadata(release.join("game.haku"))?.len()
                    );
                }
            }
            Err(error) => {
                let package = Package::open_directory(
                    release.join("game.haku"),
                    release.join("data"),
                    identity.root_key(),
                    identity.public_key(),
                    ResourceBudget::default(),
                    OpenPolicy::TrustFirstRelease,
                )?;
                package.verify_segments()?;
                println!(
                    "patch_failure step={} completed={complete} active_release={} error={error}",
                    index + 1,
                    package.release_sequence()
                );
                return Err(error.into());
            }
        }
    }
    let package = open_counted(&release, identity, Counters::default())?;
    for asset in package.list_assets()? {
        assert_eq!(
            package.asset(&asset.path)?.read()?,
            std::fs::read(input.join(&asset.path))?
        );
    }
    assert!(peak_segments <= 64);
    println!(
        "patch_complete updates={complete} peak_segments={peak_segments} peak_retained_bytes={peak_retained} consolidations={consolidations}"
    );
    let previous = open_counted(&release, identity, Counters::default())?.release_sequence();
    let mut rebuild = options;
    rebuild.incremental = false;
    let started = Instant::now();
    let report = pack_directory(&rebuild, identity)?;
    let package = open_counted(&release, identity, Counters::default())?;
    package.verify_segments()?;
    let rollback_accepts = Package::open_directory(
        release.join("game.haku"),
        release.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::default(),
        OpenPolicy::RequireAtLeast(previous),
    )
    .is_ok();
    assert!(rollback_accepts);
    println!(
        "patch_compact ms={:.3} segments={} retained_bytes={} stranded_bytes={} previous_release={previous} release={} rollback_accepts={rollback_accepts}",
        milliseconds(started.elapsed()),
        package.list_segments()?.len(),
        report.retained_segment_bytes,
        report.stranded_segment_bytes,
        report.release_sequence
    );
    Ok(())
}

fn manifest(root: &Path, identity: &Identity) -> Result<(), Box<dyn std::error::Error>> {
    use hakutaku_core::crypto::{self, Aes256Key, ProjectKeys};
    use hakutaku_core::format::{Catalog, PageKind};
    for count in benchmark_counts()? {
        let input = root.join(format!("manifest-{count}-input"));
        let release = root.join(format!("manifest-{count}-release"));
        write_vn_fixture(&input, count)?;
        let options = PackOptions::new(&input, &release);
        pack_directory(&options, identity)?;
        let package = open_counted(&release, identity, Counters::default())?;
        let bytes = std::fs::read(release.join("game.haku"))?;
        let header = SnapshotHeader::parse(&bytes[..SNAPSHOT_HEADER_SIZE])?;
        let keys = ProjectKeys::new(identity.root_key(), identity.project_id());
        let mut cipher = bytes[SNAPSHOT_HEADER_SIZE..header.page_region_offset as usize].to_vec();
        crypto::verify_snapshot_signature(&header, &cipher, &identity.public_key())?;
        Aes256Key::new(&keys.snapshot_key(&header.snapshot_salt))?.open(
            crypto::nonce(header.nonce_prefix, 0),
            &crypto::catalog_aad(&header),
            &mut cipher,
            "benchmark catalog",
        )?;
        let catalog = Catalog::parse(
            zstd::bulk::decompress(&cipher, header.catalog_plain_len as usize)?.into(),
        )?;
        let mut map_bytes = 0_u64;
        let mut reuse_bytes = 0_u64;
        for index in 0..header.page_count {
            let page = catalog.page(index)?;
            match page.kind {
                PageKind::BlockMap => map_bytes += u64::from(page.stored_len),
                PageKind::Reuse => reuse_bytes += u64::from(page.stored_len),
            }
        }
        println!(
            "manifest assets={count} snapshot_bytes={} catalog_stored_bytes={} map_stored_bytes={map_bytes} reuse_stored_bytes={reuse_bytes}",
            bytes.len(),
            header.catalog_stored_len
        );
        // Size projection only: positional chunk IDs, with BlockRef recovered
        // from the already-present block map. This is not a new wire encoder.
        let reuse = package.reuse_records()?;
        let mut projected = 0;
        for records in reuse.chunks((16 * 1024 - 16) / 32) {
            let mut plain = vec![0; 16];
            for record in records {
                plain.extend_from_slice(&record.chunk_id);
            }
            let compressed = zstd::bulk::compress(&plain, 3)?;
            projected += if compressed.len() + 64 < plain.len() {
                compressed.len()
            } else {
                plain.len()
            } + 16;
        }
        println!(
            "reuse_projection assets={count} records={} stored_bytes={projected}",
            reuse.len()
        );
        drop(package);
        write_fixture(&input.join(voice_path(0)), VOICE_ENTRY_BYTES, 0xaabb_ccdd)?;
        let started = Instant::now();
        let report = pack_directory(&options, identity)?;
        let changed = open_counted(&release, identity, Counters::default())?;
        assert_eq!(
            changed.asset(&voice_path(0))?.read()?,
            std::fs::read(input.join(voice_path(0)))?
        );
        println!(
            "manifest_patch assets={count} changed_bytes={VOICE_ENTRY_BYTES} ms={:.3} new_segment_bytes={} snapshot_bytes={} release={}",
            milliseconds(started.elapsed()),
            report.new_segment_bytes,
            std::fs::metadata(release.join("game.haku"))?.len(),
            report.release_sequence
        );
    }
    Ok(())
}

fn concurrent_streams(root: &Path, identity: &Identity) -> Result<(), Box<dyn std::error::Error>> {
    const BYTES: usize = 64 * 1024 * 1024;
    const WORKERS: usize = 8;
    const READS: usize = 8192;
    let input = root.join("concurrent-input");
    let release = root.join("concurrent-release");
    std::fs::create_dir_all(&input)?;
    write_fixture(&input.join("voice.opus"), BYTES, 0xabc123)?;
    pack_directory(&PackOptions::new(&input, &release), identity)?;
    let expected = std::fs::read(input.join("voice.opus"))?;
    let counters = Counters::default();
    let package = open_counted(&release, identity, counters.clone())?;
    let started = Instant::now();
    std::thread::scope(|scope| -> CoreResult<()> {
        let mut workers = Vec::new();
        for worker in 0..WORKERS {
            let package = &package;
            let expected = &expected;
            workers.push(scope.spawn(move || -> CoreResult<()> {
                let asset = package.asset("voice.opus")?;
                let mut session = asset.read_session();
                let mut random = worker as u64 + 31;
                let mut bytes = [0_u8; 8192];
                for index in 0..READS {
                    random = xorshift(random);
                    let offset = random as usize % (BYTES - bytes.len());
                    let count = session.read_at(offset as u64, &mut bytes)?;
                    assert_eq!(count, bytes.len());
                    assert_eq!(&bytes, &expected[offset..offset + count]);
                    if index % 127 == 0 {
                        package.trim();
                    }
                }
                Ok(())
            }));
        }
        for worker in workers {
            worker.join().expect("stress reader panicked")?;
        }
        Ok(())
    })?;
    println!(
        "concurrent_stress workers={WORKERS} reads={} logical_bytes={} ms={:.3} backend_bytes={} calls={}",
        WORKERS * READS,
        WORKERS * READS * 8192,
        milliseconds(started.elapsed()),
        counters.total(),
        counters.calls.load(Ordering::Relaxed)
    );
    Ok(())
}
