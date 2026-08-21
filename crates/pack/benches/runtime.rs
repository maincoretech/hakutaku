use hakutaku_core::{Asset, OpenPolicy, Package, ResourceBudget};
use hakutaku_pack::{Identity, PackOptions, pack_directory};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const STREAM_BYTES: usize = 32 * 1024 * 1024;
const RANDOM_REQUESTS: usize = 10_000;
const SHORT_SEEK_REQUESTS: usize = 10_000;
const DEDUP_FILE_BYTES: usize = 4 * 1024 * 1024;
const DEDUP_FILES: usize = 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let input = fixture.path.join("input");
    let release = fixture.path.join("release");
    std::fs::create_dir_all(&input)?;
    write_fixture(&input.join("opening.mp4"), STREAM_BYTES)?;

    let identity = Identity::generate()?;
    let pack_started = Instant::now();
    let report = pack_directory(&PackOptions::new(&input, &release), &identity)?;
    let pack_elapsed = pack_started.elapsed();

    let open_started = Instant::now();
    let package = Package::open_directory(
        release.join("game.haku"),
        release.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::default(),
        OpenPolicy::TrustFirstRelease,
    )?;
    let open_elapsed = open_started.elapsed();
    let asset = package.asset("opening.mp4")?;

    let sequential_256k = sequential_mib_s(&package, &asset, 256 * 1024)?;
    let sequential_128k = sequential_mib_s(&package, &asset, 128 * 1024)?;

    package.trim();
    let random_started = Instant::now();
    let mut cursor = asset.cursor();
    let mut random_buffer = [0_u8; 4096];
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for _ in 0..RANDOM_REQUESTS {
        state = xorshift(state);
        let offset = state % (STREAM_BYTES as u64 - random_buffer.len() as u64);
        cursor.seek(SeekFrom::Start(offset))?;
        cursor.read_exact(&mut random_buffer)?;
        std::hint::black_box(&random_buffer);
    }
    let random_elapsed = random_started.elapsed();

    package.trim();
    let mut cursor = asset.cursor();
    let mut short_seek_buffer = [0_u8; 4096];
    cursor.read_exact(&mut short_seek_buffer)?;
    cursor.seek(SeekFrom::Start(256 * 1024))?;
    cursor.read_exact(&mut short_seek_buffer)?;
    let short_seek_started = Instant::now();
    for request in 0..SHORT_SEEK_REQUESTS {
        let offset = if request & 1 == 0 { 0 } else { 256 * 1024 };
        cursor.seek(SeekFrom::Start(offset))?;
        cursor.read_exact(&mut short_seek_buffer)?;
        std::hint::black_box(&short_seek_buffer);
    }
    let short_seek_elapsed = short_seek_started.elapsed();

    println!("Hakutaku runtime benchmark: stream-32m-v1");
    println!("pack_ms={:.3}", milliseconds(pack_elapsed));
    println!("open_ms={:.3}", milliseconds(open_elapsed));
    println!("sequential_128k_mib_s={sequential_128k:.1}");
    println!("sequential_256k_mib_s={sequential_256k:.1}");
    println!(
        "random_4k_iops={:.0}",
        RANDOM_REQUESTS as f64 / random_elapsed.as_secs_f64()
    );
    println!(
        "short_seek_4k_iops={:.0}",
        SHORT_SEEK_REQUESTS as f64 / short_seek_elapsed.as_secs_f64()
    );
    println!(
        "blocks={} segment_bytes={}",
        report.block_count, report.new_segment_bytes
    );

    let dedup_input = fixture.path.join("dedup-input");
    let dedup_release = fixture.path.join("dedup-release");
    std::fs::create_dir_all(&dedup_input)?;
    let seed = dedup_input.join("copy-0.bin");
    write_fixture(&seed, DEDUP_FILE_BYTES)?;
    for index in 1..DEDUP_FILES {
        std::fs::copy(&seed, dedup_input.join(format!("copy-{index}.bin")))?;
    }
    let dedup = pack_directory(
        &PackOptions::new(&dedup_input, &dedup_release),
        &Identity::generate()?,
    )?;
    println!(
        "dedup_logical_bytes={} dedup_segment_bytes={} dedup_new_blocks={} dedup_reused_blocks={}",
        DEDUP_FILE_BYTES * DEDUP_FILES,
        dedup.new_segment_bytes,
        dedup.new_blocks,
        dedup.reused_blocks,
    );
    Ok(())
}

fn sequential_mib_s(
    package: &Package,
    asset: &Asset,
    request_bytes: usize,
) -> Result<f64, Box<dyn std::error::Error>> {
    package.trim();
    let started = Instant::now();
    let mut cursor = asset.cursor();
    let mut buffer = vec![0_u8; request_bytes];
    let mut bytes = 0_u64;
    loop {
        let read = cursor.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
    }
    Ok(bytes as f64 / (1024.0 * 1024.0) / started.elapsed().as_secs_f64())
}

fn write_fixture(path: &Path, bytes: usize) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    let mut state = 0xd1b5_4a32_d192_ed03_u64;
    let mut buffer = [0_u8; 256 * 1024];
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
    file.sync_all()
}

fn xorshift(mut value: u64) -> u64 {
    value ^= value << 13;
    value ^= value >> 7;
    value ^ (value << 17)
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new() -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "hakutaku-bench-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("runtime")
        ));
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
