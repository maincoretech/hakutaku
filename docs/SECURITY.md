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

Dependency vulnerability scanning and immutable full-SHA pins for third-party
GitHub Actions should be mandatory CI gates. GitHub artifact attestations can
bind published checksums and binaries to the repository, workflow, and commit;
they complement rather than replace platform code signing.

The current macOS bundle uses ad-hoc signing and Windows binaries are unsigned.
Formal distribution therefore still requires Apple Developer ID signing and
notarization plus Windows Authenticode certificates kept in protected CI
secrets. These platform credentials and policies are release-infrastructure
concerns and do not change Hakutaku's authenticated resource wire format.
