use hakutaku_core::{Package, ResourceBudget};
use hakutaku_pack::{Identity, PackOptions, pack_directory_with_progress};
use lexopt::prelude::*;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut parser = lexopt::Parser::from_env();
    let Some(command) = parser.next()? else {
        print_help();
        return Ok(());
    };
    match command {
        Value(command) if command == "identity" => identity_command(&mut parser),
        Value(command) if command == "pack" => pack_command(&mut parser),
        Value(command) if command == "list" => list_command(&mut parser),
        Value(command) if command == "segments" => segments_command(&mut parser),
        Value(command) if command == "extract" => extract_command(&mut parser),
        Value(command) if command == "verify" => verify_command(&mut parser),
        Value(command) if command == "help" => {
            print_help();
            Ok(())
        }
        Long("help") | Short('h') => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command: {other:?}").into()),
    }
}

fn identity_command(parser: &mut lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    match parser.next()? {
        Some(Value(command)) if command == "create" => {
            let path: PathBuf = parser.value()?.into();
            if parser.next()?.is_some() {
                return Err("identity create accepts one output path".into());
            }
            let identity = Identity::generate()?;
            identity.save(&path)?;
            println!("created {}", path.display());
            println!("project  {}", encode_hex(&identity.project_id().0));
            println!("public   {}", encode_hex(&identity.public_key()));
            println!("do not ship this identity file with the game");
            Ok(())
        }
        _ => Err("usage: hakutaku identity create <publisher.hakutaku-key>".into()),
    }
}

fn pack_command(parser: &mut lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = None;
    let mut output = None;
    let mut identity = None;
    let mut incremental = true;
    let mut level = 3;
    let mut segment_mib = 512_u64;
    let mut deferred_prefixes = Vec::new();
    while let Some(argument) = parser.next()? {
        match argument {
            Long("input") | Short('i') => input = Some(PathBuf::from(parser.value()?)),
            Long("output") | Short('o') => output = Some(PathBuf::from(parser.value()?)),
            Long("identity") | Short('k') => identity = Some(PathBuf::from(parser.value()?)),
            Long("full") => incremental = false,
            Long("zstd-level") => level = parser.value()?.parse()?,
            Long("segment-mib") => segment_mib = parser.value()?.parse()?,
            Long("deferred-prefix") => {
                deferred_prefixes.push(parser.value()?.to_string_lossy().into_owned());
            }
            Long("help") | Short('h') => {
                println!(
                    "usage: hakutaku pack -i <assets> -o <release> -k <identity> [--full] [--zstd-level 3] [--segment-mib 512] [--deferred-prefix PATH]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown pack option: {other:?}").into()),
        }
    }
    let identity = Identity::load(required_path(identity, "--identity")?)?;
    let mut options = PackOptions::new(
        required_path(input, "--input")?,
        required_path(output, "--output")?,
    );
    options.incremental = incremental;
    options.compression_level = level;
    options.segment_target_bytes = segment_mib
        .checked_mul(1024 * 1024)
        .ok_or("segment target overflow")?;
    options.deferred_prefixes = deferred_prefixes;
    let mut last_phase = "";
    let report = pack_directory_with_progress(&options, &identity, |progress| {
        if progress.phase != last_phase {
            eprintln!("{}…", progress.phase);
            last_phase = progress.phase;
        }
    })?;
    if report.changed {
        println!(
            "release {}: {} files, {} blocks ({} reused, {} new), {} new segment(s), {} MiB written; {} MiB retained, {} MiB stranded",
            report.release_sequence,
            report.file_count,
            report.block_count,
            report.reused_blocks,
            report.new_blocks,
            report.new_segments,
            report.new_segment_bytes / (1024 * 1024),
            report.retained_segment_bytes / (1024 * 1024),
            report.stranded_segment_bytes / (1024 * 1024),
        );
    } else {
        println!(
            "unchanged; release {} remains active",
            report.release_sequence
        );
    }
    Ok(())
}

fn list_command(parser: &mut lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    let (release, identity, extra) = package_arguments(parser)?;
    if extra.is_some() {
        return Err("list accepts only --package and --identity".into());
    }
    let package = open_package(&release, &identity)?;
    for asset in package.list_assets()? {
        println!("{:>12}  {:?}  {}", asset.len, asset.access, asset.path);
    }
    Ok(())
}

fn segments_command(parser: &mut lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    let (release, identity, extra) = package_arguments(parser)?;
    if extra.is_some() {
        return Err("segments accepts only --package and --identity".into());
    }
    let package = open_package(&release, &identity)?;
    for segment in package.list_segments()? {
        println!(
            "{:>12}  {:?}  {}",
            segment.len, segment.availability, segment.id
        );
    }
    Ok(())
}

fn extract_command(parser: &mut lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    let (release, identity, output) = package_arguments(parser)?;
    let output = required_path(output, "--output")?;
    let package = open_package(&release, &identity)?;
    for asset in package.list_assets()? {
        let target = output.join(&asset.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut source = package.asset(&asset.path)?.cursor();
        let mut destination = std::fs::File::create(&target)?;
        std::io::copy(&mut source, &mut destination)?;
    }
    println!("extracted to {}", output.display());
    Ok(())
}

fn verify_command(parser: &mut lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    let (release, identity, extra) = package_arguments(parser)?;
    if extra.is_some() {
        return Err("verify accepts only --package and --identity".into());
    }
    let package = open_package(&release, &identity)?;
    package.verify_segments()?;
    println!(
        "valid release {} ({} assets)",
        package.release_sequence(),
        package.list_assets()?.len()
    );
    Ok(())
}

fn package_arguments(
    parser: &mut lexopt::Parser,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>), Box<dyn std::error::Error>> {
    let mut release = None;
    let mut identity = None;
    let mut output = None;
    while let Some(argument) = parser.next()? {
        match argument {
            Long("package") | Short('p') => release = Some(PathBuf::from(parser.value()?)),
            Long("identity") | Short('k') => identity = Some(PathBuf::from(parser.value()?)),
            Long("output") | Short('o') => output = Some(PathBuf::from(parser.value()?)),
            other => return Err(format!("unknown option: {other:?}").into()),
        }
    }
    Ok((
        required_path(release, "--package")?,
        required_path(identity, "--identity")?,
        output,
    ))
}

fn open_package(
    release: &Path,
    identity_path: &Path,
) -> Result<Package, Box<dyn std::error::Error>> {
    let identity = Identity::load(identity_path)?;
    Ok(Package::open_directory(
        release.join("game.haku"),
        release.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::default(),
    )?)
}

fn required_path(
    value: Option<PathBuf>,
    name: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    value.ok_or_else(|| format!("missing required {name}").into())
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(result, "{byte:02x}").expect("writing to String cannot fail");
    }
    result
}

fn print_help() {
    println!(
        "Hakutaku authenticated game resources\n\n\
         commands:\n  \
         identity create <file>   create a publisher-only identity\n  \
         pack -i DIR -o DIR -k ID build or increment a release\n  \
         list -p DIR -k ID        list logical assets\n  \
         segments -p DIR -k ID    list signed segment inventory\n  \
         extract -p DIR -k ID -o DIR\n  \
         verify -p DIR -k ID      verify snapshot and complete segments"
    );
}
