use hakutaku_core::{OpenPolicy, Package, ResourceBudget};
use hakutaku_pack::{Identity, PackOptions, RuntimeKeyMaterial, pack_directory_with_progress};
use lexopt::prelude::*;
use std::ffi::OsString;
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
    run_from(std::env::args_os().skip(1))
}

fn run_from<I, T>(args: I) -> Result<(), Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut parser = lexopt::Parser::from_args(args);
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
        Some(Value(command)) if command == "export-runtime" => {
            let publisher: PathBuf = parser.value()?.into();
            let output: PathBuf = parser.value()?.into();
            if parser.next()?.is_some() {
                return Err("identity export-runtime accepts publisher and output paths".into());
            }
            let identity = Identity::load(publisher)?;
            identity.runtime_key_material()?.save(&output)?;
            println!("created runtime keys {}", output.display());
            println!("contains decryption material but no signing private key");
            Ok(())
        }
        _ => Err(
            "usage: hakutaku identity create <publisher.hakutaku-key> | identity export-runtime <publisher.hakutaku-key> <game.hakutaku-runtime-key>"
                .into(),
        ),
    }
}

fn pack_command(parser: &mut lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = None;
    let mut output = None;
    let mut identity = None;
    let mut incremental = true;
    let mut development_cache = false;
    let mut level = 3;
    let mut segment_mib = 512_u64;
    let mut deferred_prefixes = Vec::new();
    while let Some(argument) = parser.next()? {
        match argument {
            Long("input") | Short('i') => input = Some(PathBuf::from(parser.value()?)),
            Long("output") | Short('o') => output = Some(PathBuf::from(parser.value()?)),
            Long("identity") | Short('k') => identity = Some(PathBuf::from(parser.value()?)),
            Long("full") => incremental = false,
            Long("dev-cache") => development_cache = true,
            Long("zstd-level") => level = parser.value()?.parse()?,
            Long("segment-mib") => segment_mib = parser.value()?.parse()?,
            Long("deferred-prefix") => {
                deferred_prefixes.push(parser.value()?.to_string_lossy().into_owned());
            }
            Long("help") | Short('h') => {
                println!(
                    "usage: hakutaku pack -i <assets> -o <release> -k <identity> [--full] [--dev-cache] [--zstd-level 3] [--segment-mib 512] [--deferred-prefix PATH]"
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
    if development_cache && !incremental {
        return Err(
            "--dev-cache cannot be combined with --full; full builds always reread sources".into(),
        );
    }
    options.development_cache = development_cache;
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
    let arguments = package_arguments(parser)?;
    if arguments.output.is_some() {
        return Err("list accepts only --package, --keys, and --minimum-release".into());
    }
    let package = open_package(&arguments)?;
    for asset in package.list_assets()? {
        println!("{:>12}  {:?}  {}", asset.len, asset.access, asset.path);
    }
    Ok(())
}

fn segments_command(parser: &mut lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    let arguments = package_arguments(parser)?;
    if arguments.output.is_some() {
        return Err("segments accepts only --package, --keys, and --minimum-release".into());
    }
    let package = open_package(&arguments)?;
    for segment in package.list_segments()? {
        println!(
            "{:>12}  {:?}  {}",
            segment.len, segment.availability, segment.id
        );
    }
    Ok(())
}

fn extract_command(parser: &mut lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    let arguments = package_arguments(parser)?;
    let output = required_path(arguments.output.clone(), "--output")?;
    std::fs::create_dir_all(&output)?;
    let output = output.canonicalize()?;
    let package = open_package(&arguments)?;
    for asset in package.list_assets()? {
        let target = safe_extraction_target(&output, &asset.path)?;
        let mut source = package.asset(&asset.path)?.cursor();
        let mut destination = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        std::io::copy(&mut source, &mut destination)?;
    }
    println!("extracted to {}", output.display());
    Ok(())
}

fn verify_command(parser: &mut lexopt::Parser) -> Result<(), Box<dyn std::error::Error>> {
    let arguments = package_arguments(parser)?;
    if arguments.output.is_some() {
        return Err("verify accepts only --package, --keys, and --minimum-release".into());
    }
    let package = open_package(&arguments)?;
    package.verify_segments()?;
    println!(
        "valid release {} ({} assets)",
        package.release_sequence(),
        package.list_assets()?.len()
    );
    Ok(())
}

struct PackageArguments {
    release: PathBuf,
    keys: PathBuf,
    output: Option<PathBuf>,
    minimum_release: Option<u64>,
}

fn package_arguments(
    parser: &mut lexopt::Parser,
) -> Result<PackageArguments, Box<dyn std::error::Error>> {
    let mut release = None;
    let mut keys = None;
    let mut output = None;
    let mut minimum_release = None;
    while let Some(argument) = parser.next()? {
        match argument {
            Long("package") | Short('p') => release = Some(PathBuf::from(parser.value()?)),
            Long("keys") | Short('k') => keys = Some(PathBuf::from(parser.value()?)),
            Long("output") | Short('o') => output = Some(PathBuf::from(parser.value()?)),
            Long("minimum-release") => minimum_release = Some(parser.value()?.parse()?),
            other => return Err(format!("unknown option: {other:?}").into()),
        }
    }
    Ok(PackageArguments {
        release: required_path(release, "--package")?,
        keys: required_path(keys, "--keys")?,
        output,
        minimum_release,
    })
}

fn open_package(arguments: &PackageArguments) -> Result<Package, Box<dyn std::error::Error>> {
    let keys = RuntimeKeyMaterial::load(&arguments.keys)?;
    let policy = arguments
        .minimum_release
        .map_or(OpenPolicy::TrustFirstRelease, OpenPolicy::requiring);
    let package = Package::open_directory(
        arguments.release.join("game.haku"),
        arguments.release.join("data"),
        keys.root_key(),
        keys.public_key,
        ResourceBudget::default(),
        policy,
    )?;
    if package.project_id() != keys.project_id {
        return Err(hakutaku_core::Error::ProjectMismatch.into());
    }
    Ok(package)
}

fn safe_extraction_target(
    root: &Path,
    logical_path: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    hakutaku_core::format::validate_canonical_path(logical_path)?;
    let mut components = logical_path.split('/').peekable();
    let mut parent = root.to_path_buf();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            let target = parent.join(component);
            if !target.starts_with(root) {
                return Err("extraction target escaped output root".into());
            }
            return Ok(target);
        }
        parent.push(component);
        match std::fs::create_dir(&parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        parent = parent.canonicalize()?;
        if !parent.starts_with(root) {
            return Err("extraction directory escaped output root".into());
        }
    }
    Err("empty extraction path".into())
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
         identity export-runtime <publisher> <runtime-keys>\n  \
         pack -i DIR -o DIR -k ID build or increment a release\n  \
         list -p DIR -k KEYS      list logical assets\n  \
         segments -p DIR -k KEYS  list signed segment inventory\n  \
         extract -p DIR -k KEYS -o DIR\n  \
         verify -p DIR -k KEYS    verify snapshot and complete segments"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hakutaku-cli-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn error(args: &[&str]) -> String {
        run_from(args.iter().copied()).unwrap_err().to_string()
    }

    #[test]
    fn top_level_help_and_empty_invocation_succeed() {
        assert!(run_from(std::iter::empty::<&str>()).is_ok());
        assert!(run_from(["help"]).is_ok());
        assert!(run_from(["--help"]).is_ok());
        assert!(run_from(["-h"]).is_ok());
    }

    #[test]
    fn rejects_unknown_commands_and_options() {
        assert!(error(&["unknown"]).contains("unknown command"));
        assert!(error(&["pack", "--unknown"]).contains("unknown pack option"));
        assert!(error(&["list", "--unknown"]).contains("unknown option"));
    }

    #[test]
    fn commands_require_their_declared_arguments() {
        assert!(error(&["identity", "create"]).contains("missing argument"));
        assert!(error(&["identity", "export-runtime"]).contains("missing argument"));
        assert!(error(&["pack"]).contains("missing required --identity"));
        assert!(error(&["list"]).contains("missing required --package"));
        assert!(error(&["segments"]).contains("missing required --package"));
        assert!(
            error(&["extract", "-p", "release", "-k", "publisher-key"])
                .contains("missing required --output")
        );
        assert!(error(&["verify"]).contains("missing required --package"));
    }

    #[test]
    fn pack_help_does_not_require_publisher_inputs() {
        assert!(run_from(["pack", "--help"]).is_ok());
        assert!(run_from(["pack", "-h"]).is_ok());
    }

    #[test]
    fn extraction_targets_remain_beneath_the_canonical_root() {
        let root = scratch("extract-root");
        let outside = scratch("extract-outside");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let root = root.canonicalize().unwrap();
        let target = safe_extraction_target(&root, "voice/ch01/line.opus").unwrap();
        assert!(target.starts_with(&root));
        assert!(safe_extraction_target(&root, "../escape").is_err());
        assert!(safe_extraction_target(&root, "C:/escape").is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
            assert!(safe_extraction_target(&root, "linked/escape.bin").is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }
}
