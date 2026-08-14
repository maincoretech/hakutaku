#![deny(missing_docs)]

//! Publisher-side Hakutaku package construction.

mod build_cache;
mod error;
mod identity;
mod packer;
mod planner;
mod source;

pub use error::{Error, Result};
pub use identity::{Identity, RuntimeKeyMaterial, is_hakutaku_key_file};
pub use packer::{
    PackOptions, PackProgress, PackReport, pack_directory, pack_directory_with_progress,
};
pub use planner::{
    AssetChange, PlannedAsset, ReleasePlan, plan_directory, plan_directory_after_pack,
};
