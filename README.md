# Hakutaku

Hakutaku is an authenticated, random-access resource package designed for
offline games. A release consists of one signed `game.haku` snapshot and a
small set of immutable, content-addressed `.taku` segments.

The repository is intentionally split by responsibility:

- `hakutaku-core`: the only crate linked by a game runtime;
- `hakutaku-pack`: publisher identity, full/incremental bundle construction;
- `hakutaku`: command-line pack/list/extract/verify tool;
- `hakutaku-gui`: optional Pack/Browse/Bench developer application.

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

# Build or incrementally update a complete release directory.
cargo run -p hakutaku-cli -- pack \
  --input path/to/assets \
  --output path/to/release \
  --identity publisher.hakutaku-key \
  --deferred-prefix dlc

# Verify every immutable segment, or inspect the file table.
cargo run -p hakutaku-cli -- verify \
  --package path/to/release --identity publisher.hakutaku-key
cargo run -p hakutaku-cli -- list \
  --package path/to/release --identity publisher.hakutaku-key
cargo run -p hakutaku-cli -- segments \
  --package path/to/release --identity publisher.hakutaku-key

# Developer GUI with Pack, Browse, and Bench tabs.
cargo run -p hakutaku-gui
```

Only `game.haku` and `data/*.taku` belong in the shipped game. The
`*.hakutaku-key` identity contains both the content root key and publisher
signing key; never ship or commit it.

Incremental packing reuses chunks from the active release. Identical chunks are
also deduplicated during the first release, so copied assets are encrypted and
stored only once. Files up to 32 KiB are marked `Hot`, media uses fixed
`Streaming` blocks, and other assets enter the bounded second-hit `Normal`
cache. A deferred prefix is isolated into separately signed segment records;
required and deferred blocks never share a segment.

## Runtime and update integration

The runtime opens and authenticates `game.haku` first. `Package::list_segments`
then exposes the signed ID, byte length, and `Required`/`Deferred` availability
of every immutable segment. Installers can download only missing content while
the game supplies storage through the dependency-free `SegmentSource` and
`PositionedFile` traits. A missing on-demand segment returns
`Error::SegmentUnavailable(id)`; networking and retry policy stay outside the
format reader.

This boundary works with desktop files, Android/iOS asset storage, memory maps,
or a platform download manager without adding an HTTP client or async runtime
to `hakutaku-core`. Snapshot and segment reads are positional and safe to share
between concurrent cursors.

Publisher writes use synchronized temporary files, verified staging, atomic
snapshot replacement, Unix directory synchronization, and post-commit garbage
collection. Interrupted `.part` files are removed only after the exclusive
build lock is acquired. If a process is killed while packing, confirm no packer
is still running before removing a reported stale `.hakutaku.lock`.

See [FORMAT.md](FORMAT.md) for the normative v1 byte layout, parser limits,
nonce/AAD rules, and verification chain.
See [PERFORMANCE.md](PERFORMANCE.md) for the committed benchmark protocol and
the current read/dedup baseline.

## Validation

Hakutaku requires Rust 1.97.1. The same version is pinned for local development
and CI in `rust-toolchain.toml`. Local acceptance uses:

```sh
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo bench --workspace
```
