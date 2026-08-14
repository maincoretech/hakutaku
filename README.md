# Hakutaku

<p align="center">
  <img src="assets/icons/hakutaku.png" width="160" alt="Hakutaku project icon">
</p>

Hakutaku is an authenticated, random-access resource package designed for
offline games and optimized for visual-novel content. Its packing and runtime
policies account for many small scripts and UI assets, seekable voice, music,
and video streams, large backgrounds and character images, scene-local
lookahead, and patch-friendly resource replacement. A release consists of one
signed `game.haku` snapshot and a small set of immutable, content-addressed
`.taku` segments. The wire format remains general enough for other offline
games; the reference policy is tuned for visual novels.

The repository is intentionally split by responsibility:

- `hakutaku-core`: the only crate linked by a game runtime;
- `hakutaku-pack`: publisher identity, full/incremental bundle construction;
- `hakutaku`: command-line pack/list/extract/verify tool;
- `hakutaku-gui`: optional resource, release, and publisher-identity manager.

The v1 wire format has one cryptographic suite: AES-256-GCM for encrypted
catalogs, pages, and blocks, Ed25519 for publisher identity, and BLAKE3 for
content addresses and signed commitments. It does not contain an algorithm
identifier or compatibility branch.

This directory is an independent Cargo workspace so it can be moved into its
own repository without inheriting Kēne's dependency graph.

## Quick start

```sh
# Create once and keep this publisher file private.
cargo run -p hakutaku-cli -- identity create publisher.hakutaku-key

# Export the non-signing material used by runtime/read-only tools.
cargo run -p hakutaku-cli -- identity export-runtime \
  publisher.hakutaku-key game.hakutaku-runtime-key

# Build or incrementally update a complete release directory.
cargo run -p hakutaku-cli -- pack \
  --input path/to/assets \
  --output path/to/release \
  --identity publisher.hakutaku-key \
  --deferred-prefix dlc

# Verify every immutable segment, or inspect the file table.
cargo run -p hakutaku-cli -- verify \
  --package path/to/release --keys game.hakutaku-runtime-key
cargo run -p hakutaku-cli -- list \
  --package path/to/release --keys game.hakutaku-runtime-key
cargo run -p hakutaku-cli -- segments \
  --package path/to/release --keys game.hakutaku-runtime-key

# Inspect source changes, build and verify releases, and manage identities.
cargo run -p hakutaku-gui
```

The GUI treats the source directory as the single source of truth. Its
**Resources** view compares every source asset with the authenticated active
release, including same-size edits, and supports safe import, atomic replace,
search, and reveal operations. **Release** previews and builds incremental or
full releases and verifies all immutable segments. **Identity** creates,
inspects, and backs up publisher identities without exposing private key
material. Runtime benchmarks remain in the repository's Criterion bench rather
than the application.

Only `game.haku`, `data/*.taku`, and the runtime's equivalent embedded key
material belong in the shipped game. The `*.hakutaku-key` publisher identity
contains both the content root key and Ed25519 signing private key; never ship
or commit it. `*.hakutaku-runtime-key` contains the content decryption secret
and public verification key but cannot sign a release. It is suitable for
read-only tooling, not for public distribution as a loose file. The packer and
GUI reject either key format as an asset by file magic, regardless of filename.

Incremental packing reuses chunks from the active release. Identical chunks in
the same placement class are also deduplicated during the first release, so
copied assets with the same access policy are encrypted and stored only once.
Scripts, UI files, and small configuration up to 32 KiB are marked `Hot`.
Short voice/SFX audio up to 1 MiB is `Transient`, while BGM/music, longer
audio, and video are `Streaming`; media classification takes precedence over
size. Other assets enter the bounded second-hit `Normal` cache. The reference
packer keeps each availability/access class in its own
bounded segment stream: 64 MiB for Hot, 256 MiB for Normal, 128 MiB for
Transient, and up to the configured limit (512 MiB by default) for Streaming.
These are upper bounds rather than padded target sizes. Deferred content is
also isolated; required and deferred blocks never share a segment.
See [docs/PERFORMANCE.md](docs/PERFORMANCE.md) for the allocation policy and the local
Unity- and Unreal-shaped I/O regression baseline.

## Runtime and update integration

The runtime opens and authenticates `game.haku` first. `Package::list_segments`
then exposes the signed ID, byte length, and `Required`/`Deferred` availability
of every immutable segment. Installers can download only missing content while
the game supplies storage through the dependency-free `SegmentSource` and
`PositionedFile` traits. A missing on-demand segment returns
`Error::SegmentUnavailable(id)`; networking and retry policy stay outside the
format reader.

Sequential cursors reuse their ciphertext and decompression buffers and keep
the current and previous Streaming blocks for short decoder seeks. Engines can
schedule `Asset::prefetch_range` on their existing task pool; its dedicated
cache is bounded by `ResourceBudget::prefetch_cache_bytes` and creates no
Hakutaku-owned threads.

`Asset::read_at` remains the convenient one-shot random-access API. Code that
performs repeated reads should retain an `AssetCursor` for sequential/seekable
decoders or an `AssetReadSession` for offset-based engine APIs; both preserve
the active segment handle and decode buffers across calls.

The signed `release_sequence` prevents undetectable edits but cannot by itself
remember a previously accepted version. A launcher should persist its highest
accepted sequence and pass `OpenPolicy::requiring(sequence)` to
`Package::open_with_policy` or `Package::open_directory_with_policy`. Hakutaku
then rejects a correctly signed older snapshot without owning platform state.

This boundary works with desktop files, Android/iOS asset storage, memory maps,
or a platform download manager without adding an HTTP client or async runtime
to `hakutaku-core`. Snapshot and segment reads are positional and safe to share
between concurrent cursors.

Publisher writes use synchronized temporary files, verified staging, atomic
snapshot replacement, Unix directory synchronization, and post-commit garbage
collection. Interrupted `.part` files are removed only after the exclusive
build lock is acquired. If a process is killed while packing, confirm no packer
is still running before removing a reported stale `.hakutaku.lock`.

For local 20–40 GiB VN iteration, incremental builds may opt into
`--dev-cache`. The release-local cache records source size, modification time,
available file identity, and authenticated chunk metadata so unchanged voices
do not need to be reread merely to rediscover their chunks. It is bound to the
project and active release sequence; corrupt or stale caches are ignored.
`--full` cannot be combined with it, and normal/final builds retain complete
source rereads and byte-for-byte staged verification. After a successful GUI
incremental build, the post-build plan validates the complete path/size/access
inventory without immediately rereading every source body.

See [docs/FORMAT.md](docs/FORMAT.md) for the normative v1 byte layout, parser limits,
nonce/AAD rules, and verification chain.

## Validation

Hakutaku requires Rust 1.97.1. The same version is pinned for local development
and CI in `rust-toolchain.toml`. Local acceptance uses:

```sh
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc -p hakutaku-core -p hakutaku-pack --no-deps
cargo llvm-cov -p hakutaku-core -p hakutaku-pack --fail-under-lines 97.5
cargo llvm-cov -p hakutaku-cli --fail-under-lines 45
cargo bench --workspace
```

The larger VN benchmark is opt-in because it creates 10k/50k/100k-entry
fixtures. A 10k smoke run and the complete matrix are:

```sh
HAKUTAKU_VN_BENCH=1 HAKUTAKU_VN_BENCH_COUNTS=10000 \
  cargo bench -p hakutaku-pack --bench vn_runtime
HAKUTAKU_VN_BENCH=1 cargo bench -p hakutaku-pack --bench vn_runtime
```

Every successful push to `main` creates the next patch tag and publishes the
CLI and GUI builds, together with `SHA256SUMS`, on GitHub Releases. The first
automated tag follows `workspace.package.version`; later releases increment its
patch component. Raise the workspace version explicitly to start a new minor or
major line.

Publisher-key storage, CI trust, and platform signing boundaries are documented
in [docs/SECURITY.md](docs/SECURITY.md).

Both library crates deny missing public API documentation. CI therefore keeps
public rustdoc coverage at 100%. The canonical wire-format parser is held at
100% line coverage; the combined runtime and publisher baseline is 97.5% or
higher, and CLI argument handling has a 45% regression floor. The remaining
lines are operating-system and cryptographic-provider failure returns that
cannot be triggered deterministically without replacing the production
backends.

## License

Licensed under either the [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT), at your option.
