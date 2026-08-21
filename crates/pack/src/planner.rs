use crate::packer::{PackOptions, validate_options};
use crate::source::{SourceFile, classify, collect_files};
use crate::{Result, RuntimeKeyMaterial};
use hakutaku_core::{AccessClass, AssetInfo, Availability, OpenPolicy, Package, ResourceBudget};
use std::collections::BTreeMap;
use std::io::{BufReader, Read};

const COMPARE_BUFFER_BYTES: usize = 1024 * 1024;

/// Relationship between one source asset and the active packaged release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetChange {
    /// The source asset is absent from the active release.
    Added,
    /// The logical path exists, but its plaintext content changed.
    Modified,
    /// The source and packaged plaintext are byte-for-byte identical.
    Unchanged,
    /// The active release contains an asset that is absent from the source tree.
    Removed,
}

/// One resource shown in a publisher build preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedAsset {
    /// Canonical package-relative UTF-8 path.
    pub path: String,
    /// Current source length, or `None` when the asset was removed.
    pub source_len: Option<u64>,
    /// Active-release length, or `None` for a newly added asset.
    pub released_len: Option<u64>,
    /// Runtime cache and access policy selected by the packer.
    pub access: AccessClass,
    /// Installation policy selected by the current pack options.
    pub availability: Availability,
    /// Exact source-to-release relationship.
    pub change: AssetChange,
}

/// Exact source inventory and active-release comparison used by publisher UIs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReleasePlan {
    /// Sequence of the compared release, or `None` before the first build.
    pub previous_release: Option<u64>,
    /// Resources in canonical path order, including removed release entries.
    pub assets: Vec<PlannedAsset>,
    /// Total bytes currently present in the source tree.
    pub source_bytes: u64,
    /// Plaintext bytes belonging to added or modified source assets.
    pub changed_source_bytes: u64,
}

/// Scans a source tree and compares it exactly with the active release.
///
/// The comparison authenticates and reads matching packaged assets so same-size
/// edits are reported correctly. It never writes the source or release.
///
/// # Errors
///
/// Returns an error for invalid options, inaccessible source files, an invalid
/// identity, or a corrupt active release.
pub fn plan_directory(options: &PackOptions, keys: &RuntimeKeyMaterial) -> Result<ReleasePlan> {
    validate_options(options)?;
    let sources = collect_files(&options.input_directory)?;
    let source_bytes = sources.iter().try_fold(0_u64, |total, source| {
        total
            .checked_add(source.len)
            .ok_or_else(|| crate::Error::InvalidInput("input size overflow".into()))
    })?;
    let snapshot = options.output_directory.join("game.haku");
    let package = if options.incremental && snapshot.is_file() {
        let package = Package::open_directory(
            snapshot,
            options.output_directory.join("data"),
            keys.root_key(),
            keys.public_key,
            ResourceBudget::memory_constrained(),
            OpenPolicy::TrustFirstRelease,
        )?;
        if package.project_id() != keys.project_id {
            return Err(hakutaku_core::Error::ProjectMismatch.into());
        }
        Some(package)
    } else {
        None
    };
    let previous_release = package.as_ref().map(Package::release_sequence);
    let mut released = package
        .as_ref()
        .map(Package::list_assets)
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .map(|asset| (asset.path.clone(), asset))
        .collect::<BTreeMap<_, _>>();
    let mut source_buffer = vec![0_u8; COMPARE_BUFFER_BYTES];
    let mut release_buffer = vec![0_u8; COMPARE_BUFFER_BYTES];
    let mut assets = Vec::with_capacity(sources.len().saturating_add(released.len()));
    let mut changed_source_bytes = 0_u64;

    for source in &sources {
        let previous = released.remove(&source.logical_path);
        let current_access = classify(source).access;
        let change = match (&package, &previous) {
            (Some(package), Some(previous))
                if previous.len == source.len
                    && asset_matches_source(
                        package,
                        source,
                        &mut source_buffer,
                        &mut release_buffer,
                    )? =>
            {
                AssetChange::Unchanged
            }
            (_, Some(_)) => AssetChange::Modified,
            _ => AssetChange::Added,
        };
        if matches!(change, AssetChange::Added | AssetChange::Modified) {
            changed_source_bytes = changed_source_bytes
                .checked_add(source.len)
                .ok_or_else(|| crate::Error::InvalidInput("changed size overflow".into()))?;
        }
        assets.push(PlannedAsset {
            path: source.logical_path.clone(),
            source_len: Some(source.len),
            released_len: previous.as_ref().map(|asset| asset.len),
            access: current_access,
            availability: options.availability(&source.logical_path),
            change,
        });
    }

    assets.extend(
        released
            .into_values()
            .map(|asset| removed_asset(options, asset)),
    );
    assets.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(ReleasePlan {
        previous_release,
        assets,
        source_bytes,
        changed_source_bytes,
    })
}

/// Rebuilds a UI plan after a successful pack without re-reading asset bodies.
///
/// This metadata-only fast path is intentionally valid only immediately after
/// [`crate::pack_directory`] or [`crate::pack_directory_with_progress`] has
/// returned successfully for the same options. General previews must use
/// [`plan_directory`] so same-size edits are compared byte-for-byte.
///
/// # Errors
///
/// Returns an error if the source inventory no longer matches the built release.
pub fn plan_directory_after_pack(
    options: &PackOptions,
    keys: &RuntimeKeyMaterial,
) -> Result<ReleasePlan> {
    validate_options(options)?;
    let sources = collect_files(&options.input_directory)?;
    let source_bytes = sources.iter().try_fold(0_u64, |total, source| {
        total
            .checked_add(source.len)
            .ok_or_else(|| crate::Error::InvalidInput("input size overflow".into()))
    })?;
    let package = Package::open_directory(
        options.output_directory.join("game.haku"),
        options.output_directory.join("data"),
        keys.root_key(),
        keys.public_key,
        ResourceBudget::memory_constrained(),
        OpenPolicy::TrustFirstRelease,
    )?;
    if package.project_id() != keys.project_id {
        return Err(hakutaku_core::Error::ProjectMismatch.into());
    }
    let mut released = package
        .list_assets()?
        .into_iter()
        .map(|asset| (asset.path.clone(), asset))
        .collect::<BTreeMap<_, _>>();
    let mut assets = Vec::with_capacity(sources.len());
    for source in &sources {
        let released = released.remove(&source.logical_path).ok_or_else(|| {
            crate::Error::InvalidInput(format!(
                "built release is missing source asset: {}",
                source.logical_path
            ))
        })?;
        let access = classify(source).access;
        if released.len != source.len || released.access != access {
            return Err(crate::Error::InvalidInput(format!(
                "source changed after pack: {}",
                source.logical_path
            )));
        }
        assets.push(PlannedAsset {
            path: source.logical_path.clone(),
            source_len: Some(source.len),
            released_len: Some(released.len),
            access,
            availability: options.availability(&source.logical_path),
            change: AssetChange::Unchanged,
        });
    }
    if let Some(removed) = released.into_values().next() {
        return Err(crate::Error::InvalidInput(format!(
            "built release contains stale asset: {}",
            removed.path
        )));
    }
    Ok(ReleasePlan {
        previous_release: Some(package.release_sequence()),
        assets,
        source_bytes,
        changed_source_bytes: 0,
    })
}

fn asset_matches_source(
    package: &Package,
    source: &SourceFile,
    source_buffer: &mut [u8],
    release_buffer: &mut [u8],
) -> Result<bool> {
    let mut source_file = BufReader::with_capacity(COMPARE_BUFFER_BYTES, source.open_verified()?);
    let mut released = package.asset(&source.logical_path)?.cursor();
    let mut remaining = source.len;
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(source_buffer.len() as u64))
            .map_err(|_| crate::Error::InvalidInput("comparison length overflow".into()))?;
        source_file.read_exact(&mut source_buffer[..chunk])?;
        released.read_exact(&mut release_buffer[..chunk])?;
        if source_buffer[..chunk] != release_buffer[..chunk] {
            return Ok(false);
        }
        remaining -= chunk as u64;
    }
    source.validate_open_file(source_file.get_ref())?;
    Ok(true)
}

fn removed_asset(options: &PackOptions, asset: AssetInfo) -> PlannedAsset {
    PlannedAsset {
        availability: options.availability(&asset.path),
        path: asset.path,
        source_len: None,
        released_len: Some(asset.len),
        access: asset.access,
        change: AssetChange::Removed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Identity, pack_directory};
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hakutaku-plan-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn plan_reports_exact_add_modify_remove_and_unchanged_states() {
        let root = scratch("changes");
        let input = root.join("input");
        let output = root.join("output");
        std::fs::create_dir_all(input.join("dlc")).unwrap();
        std::fs::write(input.join("same.txt"), b"same").unwrap();
        std::fs::write(input.join("edit.txt"), b"old!").unwrap();
        std::fs::write(input.join("remove.txt"), b"remove").unwrap();
        std::fs::write(input.join("dlc/later.bin"), b"later").unwrap();
        let identity = Identity::generate().unwrap();
        let keys = identity.runtime_key_material().unwrap();
        let mut options = PackOptions::new(&input, &output);
        options.deferred_prefixes.push("dlc".into());

        let initial = plan_directory(&options, &keys).unwrap();
        assert_eq!(initial.previous_release, None);
        assert!(
            initial
                .assets
                .iter()
                .all(|asset| asset.change == AssetChange::Added)
        );
        assert_eq!(initial.source_bytes, initial.changed_source_bytes);
        assert_eq!(
            initial
                .assets
                .iter()
                .find(|asset| asset.path == "dlc/later.bin")
                .unwrap()
                .availability,
            Availability::Deferred
        );

        pack_directory(&options, &identity).unwrap();
        let unchanged = plan_directory(&options, &keys).unwrap();
        assert_eq!(unchanged.previous_release, Some(1));
        assert!(
            unchanged
                .assets
                .iter()
                .all(|asset| asset.change == AssetChange::Unchanged)
        );
        assert_eq!(unchanged.changed_source_bytes, 0);
        assert_eq!(
            plan_directory_after_pack(&options, &keys).unwrap(),
            unchanged
        );

        std::fs::write(input.join("edit.txt"), b"new!").unwrap();
        std::fs::remove_file(input.join("remove.txt")).unwrap();
        std::fs::write(input.join("added.txt"), b"added").unwrap();
        let changed = plan_directory(&options, &keys).unwrap();
        let states = changed
            .assets
            .iter()
            .map(|asset| (asset.path.as_str(), asset.change))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(states["same.txt"], AssetChange::Unchanged);
        assert_eq!(states["edit.txt"], AssetChange::Modified);
        assert_eq!(states["remove.txt"], AssetChange::Removed);
        assert_eq!(states["added.txt"], AssetChange::Added);
        assert_eq!(changed.changed_source_bytes, 9);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_only_post_pack_plan_rejects_every_stale_inventory_shape() {
        let root = scratch("post-pack-stale");
        let input = root.join("input");
        let output = root.join("output");
        std::fs::create_dir_all(&input).unwrap();
        std::fs::write(input.join("asset.bin"), b"four").unwrap();
        let identity = Identity::generate().unwrap();
        let mut keys = identity.runtime_key_material().unwrap();
        let options = PackOptions::new(&input, &output);
        pack_directory(&options, &identity).unwrap();

        let snapshot = output.join("game.haku");
        let valid_snapshot = std::fs::read(&snapshot).unwrap();
        let mut damaged_snapshot = valid_snapshot.clone();
        damaged_snapshot[0] ^= 1;
        std::fs::write(&snapshot, damaged_snapshot).unwrap();
        assert!(plan_directory(&options, &keys).is_err());
        assert!(plan_directory_after_pack(&options, &keys).is_err());
        std::fs::write(&snapshot, valid_snapshot).unwrap();

        keys.project_id.0[0] ^= 1;
        assert!(plan_directory(&options, &keys).is_err());
        assert!(plan_directory_after_pack(&options, &keys).is_err());
        keys.project_id = identity.project_id();

        std::fs::write(input.join("added.bin"), b"added").unwrap();
        assert!(plan_directory_after_pack(&options, &keys).is_err());
        std::fs::remove_file(input.join("added.bin")).unwrap();

        std::fs::write(input.join("asset.bin"), b"different length").unwrap();
        assert!(plan_directory_after_pack(&options, &keys).is_err());
        std::fs::remove_file(input.join("asset.bin")).unwrap();
        assert!(plan_directory_after_pack(&options, &keys).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
