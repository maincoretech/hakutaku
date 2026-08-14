use hakutaku_core::{
    LocalFile, Package, PositionedFile, ResourceBudget, Result as CoreResult,
    SEGMENT_FILE_EXTENSION, SegmentId, SegmentSource, segment_file_name,
};
use hakutaku_pack::{Identity, PackOptions, pack_directory};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const ASSET_COUNTS: &[usize] = &[10_000, 50_000, 100_000];
const LOOKUPS: usize = 1_000;
const SCRIPT_BYTES: usize = 4 * 1024;
const IMAGE_BYTES: usize = 512 * 1024;
const BGM_BYTES: usize = 4 * 1024 * 1024;
const VIDEO_BYTES: usize = 8 * 1024 * 1024;
const VOICE_BYTES: usize = 24 * 1024;
const VOICE_ENTRY_BYTES: usize = 64;
const FIXED_ASSETS: usize = 6;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("HAKUTAKU_VN_BENCH").as_deref() != Some("1".as_ref()) {
        println!("Hakutaku VN benchmark skipped");
        println!("run HAKUTAKU_VN_BENCH=1 cargo bench -p hakutaku-pack --bench vn_runtime");
        println!("cold storage requires a fresh process plus an OS-level cache reset or reboot");
        return Ok(());
    }

    let fixture = Fixture::new()?;
    let identity = Identity::generate()?;
    let counts = benchmark_counts()?;
    for asset_count in counts {
        run_scale(&fixture.path, asset_count, &identity)?;
    }
    Ok(())
}

fn benchmark_counts() -> Result<Vec<usize>, Box<dyn std::error::Error>> {
    let Some(value) = std::env::var_os("HAKUTAKU_VN_BENCH_COUNTS") else {
        return Ok(ASSET_COUNTS.to_vec());
    };
    let counts = value
        .to_string_lossy()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::parse)
        .collect::<Result<Vec<usize>, _>>()?;
    if counts.is_empty() || counts.iter().any(|count| *count < FIXED_ASSETS) {
        return Err("benchmark counts must be at least 6".into());
    }
    Ok(counts)
}

fn run_scale(
    root: &Path,
    asset_count: usize,
    identity: &Identity,
) -> Result<(), Box<dyn std::error::Error>> {
    let case = root.join(asset_count.to_string());
    let input = case.join("input");
    let release = case.join("release");
    write_vn_fixture(&input, asset_count)?;

    let options = PackOptions::new(&input, &release);
    let pack_started = Instant::now();
    let report = pack_directory(&options, identity)?;
    let pack_elapsed = pack_started.elapsed();
    let strict_incremental_started = Instant::now();
    let strict_incremental = pack_directory(&options, identity)?;
    let strict_incremental_elapsed = strict_incremental_started.elapsed();
    let mut development_options = options.clone();
    development_options.development_cache = true;
    let development_seed_started = Instant::now();
    let development_seed = pack_directory(&development_options, identity)?;
    let development_seed_elapsed = development_seed_started.elapsed();
    let development_cached_started = Instant::now();
    let development_cached = pack_directory(&development_options, identity)?;
    let development_cached_elapsed = development_cached_started.elapsed();
    if strict_incremental.changed || development_seed.changed || development_cached.changed {
        return Err("unchanged incremental benchmark unexpectedly changed the release".into());
    }
    let counters = Counters::default();

    counters.reset();
    let cold_started = Instant::now();
    let package = open_counted(&release, identity, counters.clone())?;
    let cold_open = cold_started.elapsed();
    let cold_open_backend = counters.total();

    counters.reset();
    let warm_started = Instant::now();
    let warm_package = open_counted(&release, identity, counters.clone())?;
    let warm_open = warm_started.elapsed();
    let warm_open_backend = counters.total();
    std::hint::black_box(&warm_package);

    counters.reset();
    let lookup_started = Instant::now();
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let voice_count = asset_count - FIXED_ASSETS;
    for _ in 0..LOOKUPS {
        state = xorshift(state);
        let index = state as usize % voice_count;
        std::hint::black_box(package.asset(&voice_path(index))?);
    }
    let lookup_elapsed = lookup_started.elapsed();
    let lookup_backend = counters.total();

    let script = first_read(&package, &counters, "script/main.json")?;
    let background = first_read(&package, &counters, "background/room.webp")?;
    let character = first_read(&package, &counters, "character/hero.webp")?;
    let voice = first_read(&package, &counters, "voice/sample.opus")?;
    let voice_sequence = play_voices(&package, &counters, voice_count.min(1_000))?;
    let bgm = sequential_and_seek(&package, &counters, "bgm/theme.opus")?;
    let video = sequential_and_seek(&package, &counters, "video/opening.mp4")?;
    let concurrent = interleaved_cursors(&package, &counters, voice_count)?;

    println!("Hakutaku VN benchmark: assets={asset_count}");
    println!(
        "pack_ms={:.3} blocks={} segments={}",
        milliseconds(pack_elapsed),
        report.block_count,
        report.new_segments
    );
    println!(
        "incremental_strict_ms={:.3} dev_cache_seed_ms={:.3} dev_cache_cached_ms={:.3}",
        milliseconds(strict_incremental_elapsed),
        milliseconds(development_seed_elapsed),
        milliseconds(development_cached_elapsed),
    );
    println!(
        "open_first_ms={:.3} open_first_backend_bytes={cold_open_backend}",
        milliseconds(cold_open)
    );
    println!(
        "open_warm_process_ms={:.3} open_warm_backend_bytes={warm_open_backend}",
        milliseconds(warm_open)
    );
    println!(
        "lookup_1000_ms={:.3} lookup_backend_bytes={lookup_backend}",
        milliseconds(lookup_elapsed)
    );
    print_measurement("first_script", script);
    print_measurement("first_background", background);
    print_measurement("first_character", character);
    print_measurement("first_voice", voice);
    print_measurement("voice_1000_sequential", voice_sequence);
    print_measurement("bgm_sequential_short_seek", bgm);
    print_measurement("video_sequential_short_seek", video);
    print_measurement("interleaved_cursors", concurrent);
    Ok(())
}

fn open_counted(
    release: &Path,
    identity: &Identity,
    counters: Counters,
) -> Result<Package, Box<dyn std::error::Error>> {
    Ok(Package::open(
        Arc::new(CountingFile {
            inner: LocalFile::open(release.join("game.haku"))?,
            bytes: Arc::clone(&counters.snapshot),
        }),
        Arc::new(CountingSegments {
            root: release.join("data"),
            bytes: Arc::clone(&counters.segments),
        }),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::default(),
    )?)
}

fn first_read(
    package: &Package,
    counters: &Counters,
    path: &str,
) -> Result<Measurement, Box<dyn std::error::Error>> {
    package.trim();
    counters.reset();
    let started = Instant::now();
    let bytes = package.asset(path)?.read()?;
    std::hint::black_box(&bytes);
    Ok(Measurement::new(
        started.elapsed(),
        bytes.len() as u64,
        counters.total(),
    ))
}

fn play_voices(
    package: &Package,
    counters: &Counters,
    count: usize,
) -> Result<Measurement, Box<dyn std::error::Error>> {
    package.trim();
    counters.reset();
    let started = Instant::now();
    let mut logical = 0_u64;
    for index in 0..count {
        let asset = package.asset(&voice_path(index))?;
        let mut cursor = asset.cursor();
        logical += std::io::copy(&mut cursor, &mut std::io::sink())?;
    }
    Ok(Measurement::new(
        started.elapsed(),
        logical,
        counters.total(),
    ))
}

fn sequential_and_seek(
    package: &Package,
    counters: &Counters,
    path: &str,
) -> Result<Measurement, Box<dyn std::error::Error>> {
    package.trim();
    counters.reset();
    let asset = package.asset(path)?;
    let started = Instant::now();
    let mut cursor = asset.cursor();
    let mut buffer = vec![0_u8; 128 * 1024];
    let mut logical = 0_u64;
    loop {
        let read = cursor.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        logical += read as u64;
    }
    cursor.seek(SeekFrom::Start(256 * 1024))?;
    logical += cursor.read(&mut buffer[..4096])? as u64;
    cursor.seek(SeekFrom::Start(0))?;
    logical += cursor.read(&mut buffer[..4096])? as u64;
    Ok(Measurement::new(
        started.elapsed(),
        logical,
        counters.total(),
    ))
}

fn interleaved_cursors(
    package: &Package,
    counters: &Counters,
    voice_count: usize,
) -> Result<Measurement, Box<dyn std::error::Error>> {
    package.trim();
    counters.reset();
    let mut cursors = Vec::new();
    for index in 0..voice_count.min(16) {
        cursors.push(package.asset(&voice_path(index))?.cursor());
    }
    for path in [
        "script/main.json",
        "background/room.webp",
        "character/hero.webp",
        "bgm/theme.opus",
        "video/opening.mp4",
    ] {
        cursors.push(package.asset(path)?.cursor());
    }
    let started = Instant::now();
    let mut buffer = [0_u8; 4096];
    let mut logical = 0_u64;
    for _ in 0..16 {
        for cursor in &mut cursors {
            logical += cursor.read(&mut buffer)? as u64;
        }
    }
    Ok(Measurement::new(
        started.elapsed(),
        logical,
        counters.total(),
    ))
}

fn write_vn_fixture(root: &Path, count: usize) -> std::io::Result<()> {
    std::fs::create_dir_all(root.join("script"))?;
    std::fs::create_dir_all(root.join("background"))?;
    std::fs::create_dir_all(root.join("character"))?;
    std::fs::create_dir_all(root.join("bgm"))?;
    std::fs::create_dir_all(root.join("video"))?;
    std::fs::create_dir_all(root.join("voice"))?;
    write_fixture(&root.join("script/main.json"), SCRIPT_BYTES, 1)?;
    write_fixture(&root.join("background/room.webp"), IMAGE_BYTES, 2)?;
    write_fixture(&root.join("character/hero.webp"), IMAGE_BYTES, 3)?;
    write_fixture(&root.join("bgm/theme.opus"), BGM_BYTES, 4)?;
    write_fixture(&root.join("video/opening.mp4"), VIDEO_BYTES, 5)?;
    write_fixture(&root.join("voice/sample.opus"), VOICE_BYTES, 6)?;
    for index in 0..count - FIXED_ASSETS {
        let path = root.join(voice_path(index));
        std::fs::create_dir_all(path.parent().expect("voice path has parent"))?;
        write_fixture(&path, VOICE_ENTRY_BYTES, index as u64 + 7)?;
    }
    Ok(())
}

fn voice_path(index: usize) -> String {
    format!("voice/{:03}/line-{index:06}.opus", index / 1000)
}

fn write_fixture(path: &Path, bytes: usize, mut state: u64) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut remaining = bytes;
    while remaining > 0 {
        let amount = remaining.min(buffer.len());
        for chunk in buffer[..amount].chunks_mut(8) {
            state = xorshift(state);
            chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
        }
        file.write_all(&buffer[..amount])?;
        remaining -= amount;
    }
    Ok(())
}

fn xorshift(mut value: u64) -> u64 {
    value ^= value << 13;
    value ^= value >> 7;
    value ^ (value << 17)
}

#[derive(Clone, Default)]
struct Counters {
    snapshot: Arc<AtomicU64>,
    segments: Arc<AtomicU64>,
}

impl Counters {
    fn reset(&self) {
        self.snapshot.store(0, Ordering::Relaxed);
        self.segments.store(0, Ordering::Relaxed);
    }

    fn total(&self) -> u64 {
        self.snapshot.load(Ordering::Relaxed) + self.segments.load(Ordering::Relaxed)
    }
}

struct CountingFile {
    inner: LocalFile,
    bytes: Arc<AtomicU64>,
}

impl PositionedFile for CountingFile {
    fn len(&self) -> CoreResult<u64> {
        self.inner.len()
    }

    fn read_exact_at(&self, offset: u64, destination: &mut [u8]) -> CoreResult<()> {
        self.inner.read_exact_at(offset, destination)?;
        self.bytes
            .fetch_add(destination.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

struct CountingSegments {
    root: PathBuf,
    bytes: Arc<AtomicU64>,
}

impl SegmentSource for CountingSegments {
    fn open(&self, id: SegmentId) -> CoreResult<Arc<dyn PositionedFile>> {
        let path = self
            .root
            .join(segment_file_name(id))
            .with_extension(SEGMENT_FILE_EXTENSION);
        Ok(Arc::new(CountingFile {
            inner: LocalFile::open(path)?,
            bytes: Arc::clone(&self.bytes),
        }))
    }
}

#[derive(Clone, Copy)]
struct Measurement {
    elapsed: Duration,
    logical: u64,
    backend: u64,
}

impl Measurement {
    fn new(elapsed: Duration, logical: u64, backend: u64) -> Self {
        Self {
            elapsed,
            logical,
            backend,
        }
    }
}

fn print_measurement(name: &str, value: Measurement) {
    let amplification = if value.logical == 0 {
        0.0
    } else {
        value.backend as f64 / value.logical as f64
    };
    println!(
        "{name}_ms={:.3} {name}_logical_bytes={} {name}_backend_bytes={} {name}_read_amplification={amplification:.3}",
        milliseconds(value.elapsed),
        value.logical,
        value.backend,
    );
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new() -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("hakutaku-vn-bench-{}", std::process::id()));
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::create_dir(&path)?;
        Ok(Self { path })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
