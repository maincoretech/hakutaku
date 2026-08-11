# Hakutaku

Hakutaku is an authenticated, random-access resource package designed for
offline games. A release consists of one signed `game.haku` snapshot and a
small set of immutable, content-addressed `.hks` segments.

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
  --identity publisher.hakutaku-key

# Verify every immutable segment, or inspect the file table.
cargo run -p hakutaku-cli -- verify \
  --package path/to/release --identity publisher.hakutaku-key
cargo run -p hakutaku-cli -- list \
  --package path/to/release --identity publisher.hakutaku-key

# Developer GUI with Pack, Browse, and Bench tabs.
cargo run -p hakutaku-gui
```

Only `game.haku` and `data/*.hks` belong in the shipped game. The
`*.hakutaku-key` identity contains both the content root key and publisher
signing key; never ship or commit it.

See [FORMAT.md](FORMAT.md) for the normative v1 byte layout, parser limits,
nonce/AAD rules, and verification chain.

## Validation

```sh
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo test --workspace
cargo bench --workspace
```
