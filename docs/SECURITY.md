# Publisher storage and release supply chain

Hakutaku keeps publisher signing authority outside the `.haku`/`.taku` wire
format. The publisher identity contains the content root key and Ed25519 private
key. Runtime key material contains the content root secret and public
verification key, but no signing private key. Only `pack` loads the full
identity; list, extract, verify, planning, and package open use the smaller
material.

Both key file formats are detected by magic and rejected as source assets even
when renamed. A loaded publisher identity may not reside inside the input asset
tree. The output directory may neither equal nor be nested beneath that tree,
so snapshots, segments, temporary files, and local build-cache state cannot be
collected by a later scan.

## Complete release verification

`Package::open` authenticates the snapshot header and catalog; normal reads
authenticate pages and blocks lazily. CLI/GUI Verify use `Package::verify` to
also check every metadata page, path lookup, contiguous file mapping, segment
digest and decoded content block, including Deferred segments. This is an
explicit whole-release check, not part of normal startup. `verify_segments`
retains its narrower segment-only meaning.

Strict incremental builds verify existing payloads and compare sources even
when the fingerprint is unchanged; missing/corrupt segments cannot yield a
successful unchanged result. Development-cache unchanged builds intentionally
retain their faster local-trust policy. The pack library, as well as its CLI,
rejects development cache with full reconstruction.

## At-rest boundary

On Unix, publisher identities, runtime-key exports, GUI backups, and their
temporary files are created with mode `0600`. On Windows, Rust's portable file
API inherits the destination directory's DACL; it does not expose stable APIs
for replacing that DACL with a per-user ACL while this workspace also forbids
unsafe code. Publisher identities should therefore live in a user-private
profile or secrets directory whose ACL has already been restricted. CI signing
keys should come from the CI secret store and should never be checked out.

Password-derived wrapping is intentionally not added to release files. If
publisher identity encryption is introduced later, it belongs solely to the
identity-storage layer and must use a reviewed platform keystore or a versioned
memory-hard KDF envelope.

## Build and release trust

The [current workflow](../.github/workflows/ci.yml) runs a RustSec dependency audit,
pins third-party Actions to full commit SHAs, and requires the audit plus quality,
stress, coverage and target-build jobs before a main-branch release. The release
job attests its assets and checksums. Attestations bind build provenance; they do
not replace platform code signing or a downstream engine's license policy.

The current macOS bundle uses ad-hoc signing and Windows binaries are unsigned.
Formal distribution therefore still requires Apple Developer ID signing and
notarization plus Windows Authenticode certificates kept in protected CI
secrets. These platform credentials and policies are release-infrastructure
concerns and do not change Hakutaku's authenticated resource wire format.

## Parser and replacement bounds

Private-file reads are bounded before parsing: runtime key files are 156 bytes;
publisher identity files are limited to 4,224 bytes, including at most 4 KiB of
PKCS#8 material. Generated identities are much smaller. Loaded private material
is held in zeroizing buffers, including failed PKCS#8 validation. These limits
do not change identity/key serialization; package v2 is versioned separately.

The optional development cache is capped at 256 MiB on the opened handle and
on the actual read. Entry/chunk counts must fit the remaining encoded payload
before containers reserve memory. Content-defined source reads also stop after
the collected file length plus one byte, detecting growth without reading an
unbounded changing file. Strict builds still reread and verify sources.

Snapshot, cache and GUI replacement use same-directory temporary files followed
by standard `std::fs::rename`, whose supported platforms replace an existing
file. The original destination stays in place if the final rename fails;
legacy snapshot-backup recovery remains for older interrupted builds. Cache
files remain disposable hints, not a durability or authenticity boundary.
See [Rust's rename contract](https://doc.rust-lang.org/std/fs/fn.rename.html).

## Dependency review boundary

The full locked, all-features workspace graph passes RustSec advisory, source
and dependency-ban checks. Core/Pack additionally pass all license rules from
Kēne's existing `deny.toml`, without changing that policy.

Hakutaku has no repository license allowlist. Applying Kēne's policy to the GUI
reports the same four license families before and after this upgrade: BSL-1.0
(`clipboard-win`, `error-code`), OFL-1.1 and Ubuntu-font-1.0
(`epaint_default_fonts`), and Apache-2.0 WITH LLVM-exception (`target-lexicon`,
Linux build tooling). This comparison is not a full GUI license-policy pass.
No allowlist was widened and no crate was hidden to suppress these results.
Distribution must retain the corresponding upstream notices, including font
notices, in addition to this project's own MIT/Apache licenses.

The updated zstd Rust wrapper declares BSD-3-Clause, already permitted by that
policy; the native zstd source remains 1.5.7. The reviewed primary metadata and
license files are available in [zstd 0.14.2](https://docs.rs/crate/zstd/0.14.2/source/),
[egui fonts](https://docs.rs/crate/epaint_default_fonts/0.36.2/source/),
[clipboard-win](https://docs.rs/crate/clipboard-win/5.4.1/source/), and
[target-lexicon](https://docs.rs/crate/target-lexicon/0.12.16/source/).

## Release ordering during reconstruction

Full and incremental builds share the authenticated monotonic release sequence.
Disabling reuse must not make a full rebuild appear older than the installed
release. Full reconstruction reads the existing snapshot, but does not require
old payload segments; corrupt/untrusted snapshot metadata is an error. The
v1-to-v2 rebuild uses a fresh output directory and an explicit publisher-owned
`release_sequence_floor` above the last published release. The v2 packer never
parses a v1 snapshot to infer that value. The same identity remains usable,
while key derivation, signature and AEAD domain labels are all separated as v2.
Compact path fingerprints still require full path equality; compact reuse IDs
are joined to the authenticated block map with identical record totals.

Automatic consolidation counts unique live plaintext, including deduplicated
references, against a 64 MiB rewrite budget. It reuses the verified staging and
commit protocol; no old segment is collected before replacement commits.
This bounds selected rewrite volume, not total source scanning, memory or
wall-clock time. Explicit full rebuilds remain an operator-controlled operation.
