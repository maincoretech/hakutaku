# Hakutaku v1 wire format

This document is normative for format major `1`, minor `0`. Integers are
little-endian. Every reserved byte must be zero. Readers must validate all
counts, lengths, offsets, additions, and multiplications before allocation or
I/O. Unknown versions and non-canonical layouts are errors.

Hakutaku v1 has exactly one cryptographic suite:

- AES-256-GCM with a 96-bit nonce and 128-bit tag;
- Ed25519 snapshot signatures;
- BLAKE3 key derivation, page/block commitments, fingerprints, and SegmentId;
- zstd independent frames, or RAW when compression is not useful.

There is no cipher identifier, algorithm negotiation, password KDF, or
compatibility branch in the wire format.

## Release layout

```text
release/
├── game.haku
└── data/
    └── <64 lowercase hex characters>.taku
```

`SegmentId = BLAKE3(complete .taku file)`. Segment files are immutable. A new
snapshot may reuse blocks from old segments and add new segments.

## SnapshotHeaderV1

`game.haku` begins with one 4096-byte header.

| offset | size | field |
|---:|---:|---|
| 0 | 8 | `HAKU0001` |
| 8 | 2 | major = 1 |
| 10 | 2 | minor = 0 |
| 12 | 4 | header size = 4096 |
| 16 | 16 | project ID |
| 32 | 8 | monotonically increasing release sequence |
| 40 | 8 | catalog offset = 4096 |
| 48 | 8 | catalog ciphertext + tag length |
| 56 | 8 | catalog decompressed length |
| 64 | 8 | page region offset = 4096 + catalog stored length |
| 72 | 4 | total page count |
| 76 | 4 | zero |
| 80 | 16 | snapshot KDF salt |
| 96 | 8 | snapshot nonce prefix |
| 104 | 16 | `BLAKE3(Ed25519 public key)[0..16]` |
| 120 | 32 | source fingerprint |
| 152 | 64 | Ed25519 signature |
| 216 | 3880 | zero |

The catalog immediately follows the header. It is always one zstd frame,
encrypted in place with AES-256-GCM; the 16-byte tag is appended. Pages follow
the catalog in directory order with no gaps.

The signature slot is zeroed when constructing the signed message:

```text
"Hakutaku snapshot signature v1" ||
BLAKE3(zero-signature SnapshotHeaderV1 || catalog ciphertext || catalog tag)
```

The signature is verified before catalog decryption.

## Key schedule and nonce construction

The game provides a random 32-byte content root key. It is never derived from
a human password.

```text
project_master = BLAKE3 derive_key(
  "Hakutaku project master v1",
  content_root_key || project_id
)

derive(domain, fields...) = BLAKE3 keyed_hash(
  project_master,
  domain || (u64_le(field.len) || field)...
)

snapshot_key = derive("snapshot", snapshot_salt)
segment_key  = derive("segment", segment_uid, segment_salt)
path_key     = derive("path")
```

Snapshot catalog nonce ordinal is zero. Page nonce ordinals are their
one-based directory index. Segment block nonce ordinals are their zero-based
block index.

```text
nonce = nonce_prefix[8] || u32_le(ordinal)
```

The format forbids ordinal reuse with one key. Page and block counts are
bounded below `u32::MAX`.

## CatalogV1

After decryption and zstd decoding, the catalog begins with 64 bytes:

| offset | size | field |
|---:|---:|---|
| 0 | 8 | `HAKCAT01` |
| 8 | 2 | major = 1 |
| 10 | 2 | header size = 64 |
| 12 | 4 | segment count |
| 16 | 4 | file count |
| 20 | 4 | total block count |
| 24 | 4 | path pool bytes |
| 28 | 4 | path slot count, non-zero power of two |
| 32 | 4 | total page count |
| 36 | 4 | BlockMapPage count |
| 40 | 4 | ReusePage count |
| 44 | 4 | segment table offset |
| 48 | 4 | file table offset |
| 52 | 4 | path slot table offset |
| 56 | 4 | path byte pool offset |
| 60 | 4 | page directory offset |

The only canonical order is header, segment records, file records, path slots,
path bytes, and page records. There is no padding between tables and no trailing
data.

### SegmentRecordV1 — 96 bytes

| offset | size | field |
|---:|---:|---|
| 0 | 32 | SegmentId |
| 32 | 16 | segment UID |
| 48 | 16 | segment KDF salt |
| 64 | 8 | block nonce prefix |
| 72 | 8 | complete segment file length |
| 80 | 8 | payload length |
| 88 | 4 | block count |
| 92 | 1 | availability: 0 required, 1 deferred |
| 93 | 3 | zero |

Required and deferred blocks are never mixed in one segment. A launcher must
install every required segment before normal play; deferred segments may be
absent until an asset that references them is requested. The signed catalog is
therefore also the authoritative update/install manifest. Transport and retry
behavior are deliberately outside the wire format.

### FileRecordV1 — 32 bytes

| offset | size | field |
|---:|---:|---|
| 0 | 4 | path pool offset |
| 4 | 2 | path byte length |
| 6 | 1 | layout: 0 fixed, 1 content-defined |
| 7 | 1 | access: 0 hot, 1 normal, 2 streaming, 3 transient |
| 8 | 8 | logical file length |
| 16 | 4 | first global BlockRef index |
| 20 | 4 | BlockRef count |
| 24 | 4 | fixed plaintext block length, otherwise zero |
| 28 | 4 | zero |

Paths are non-empty UTF-8 separated by `/`. Absolute paths, empty components,
`.`/`..`, backslashes, NUL, leading slash, and trailing slash are forbidden.

The reference packer classifies files up to 32 KiB as Hot, known audio/video as
Streaming, and remaining files as Normal. These are cache hints, not security
or compatibility semantics; readers must accept every declared access class.

### PathSlotV1 — 16 bytes

| offset | size | field |
|---:|---:|---|
| 0 | 8 | first 64 bits of `BLAKE3 keyed_hash(path_key, path)` |
| 8 | 4 | file index; `0xffffffff` means empty |
| 12 | 4 | zero |

The table uses linear probing. A hash match must still compare the complete
path bytes.

### PageRecordV1 — 64 bytes

| offset | size | field |
|---:|---:|---|
| 0 | 1 | kind: 1 BlockMap, 2 Reuse |
| 1 | 1 | codec: 0 RAW, 1 zstd |
| 2 | 2 | zero |
| 4 | 4 | nonce ordinal |
| 8 | 8 | offset relative to page region |
| 16 | 4 | ciphertext + tag length |
| 20 | 4 | decoded page length |
| 24 | 32 | BLAKE3 of ciphertext + tag |
| 56 | 8 | zero |

Block-map pages precede reuse pages. Nonce ordinals are directory index + 1;
relative offsets are contiguous. The signed catalog commits the complete page
digest before a page is decrypted.

## Page payloads

Every decoded page starts with 16 bytes:

| offset | size | field |
|---:|---:|---|
| 0 | 8 | `HAKMAP01` or `HAKREU01` |
| 8 | 2 | major = 1 |
| 10 | 2 | record size: 48 or 80 |
| 12 | 4 | first global record index |

A 16 KiB BlockMapPage contains at most 341 BlockRef records. A ReusePage
contains at most 204 ReuseRecord records. The final page is the only short
page. Empty packages have no pages.

### BlockRefV1 — 48 bytes

| offset | size | field |
|---:|---:|---|
| 0 | 8 | logical plaintext offset in file |
| 8 | 2 | segment ordinal |
| 10 | 4 | segment block ordinal |
| 14 | 8 | absolute physical offset in `.taku` |
| 22 | 4 | ciphertext + tag length |
| 26 | 4 | plaintext length |
| 30 | 1 | codec: 0 RAW, 1 zstd |
| 31 | 1 | zero |
| 32 | 16 | `BLAKE3(ciphertext || tag)[0..16]` |

The truncated digest is inside a page committed by the publisher signature.
It is checked before AEAD opening. A party that extracts the embedded content
key can decrypt data and recompute a GCM tag, but cannot replace this signed
digest without the publisher signing key.

### ReuseRecordV1 — 80 bytes

| offset | size | field |
|---:|---:|---|
| 0 | 32 | `BLAKE3(plaintext chunk)` |
| 32 | 48 | complete BlockRefV1 |

Reuse pages are packer-only cold data. Runtime lookup and asset reads never
load them.

## SegmentHeaderV1

Every `.taku` begins with one 4096-byte header followed by tightly packed
ciphertext blocks.

| offset | size | field |
|---:|---:|---|
| 0 | 8 | `HAKSEG01` |
| 8 | 2 | major = 1 |
| 10 | 2 | minor = 0 |
| 12 | 4 | header size = 4096 |
| 16 | 16 | project ID |
| 32 | 16 | segment UID |
| 48 | 16 | segment KDF salt |
| 64 | 8 | block nonce prefix |
| 72 | 4 | block count |
| 76 | 4 | zero |
| 80 | 8 | payload length |
| 88 | 8 | complete file length = 4096 + payload length |
| 96 | 4000 | zero |

The catalog's SegmentRecord and the segment header must match exactly before
reading payload bytes.

## AEAD associated data

Length prefixes below are literal fixed fields, not a generic serialization.

```text
catalog AAD =
  "Hakutaku catalog aad v1" || project_id || release_sequence:u64 ||
  catalog_stored_len:u64 || catalog_plain_len:u64 || page_count:u32

page AAD =
  "Hakutaku page aad v1" || project_id || release_sequence:u64 ||
  kind:u8 || codec:u8 || nonce_ordinal:u32 || stored_len:u32 || plain_len:u32

block AAD =
  "Hakutaku block aad v1" || project_id || segment_uid || block_ordinal:u32 ||
  codec:u8 || stored_len:u32 || plain_len:u32
```

## Parser limits

The v1 core currently rejects values above:

- 128 referenced segments;
- 1,000,000 files;
- 10,000,000 blocks;
- 100,000 pages;
- 65 MiB + 16 bytes stored catalog;
- 64 MiB decoded catalog;
- 32 MiB path pool;
- 1 MiB decoded individual page;
- 1 MiB decoded individual block.

For RAW records, `stored_len = plain_len + 16`. For zstd records,
`16 <= stored_len < plain_len + 16`; a non-beneficial compressed form is not
canonical. These relations bound ciphertext allocation before AEAD and keep a
single v1 block within the fixed reader budget.

These are reader safety limits, not recommended package targets.

## Verification order

```text
Ed25519 public key
  -> SnapshotHeader + catalog ciphertext signature
  -> catalog AES-256-GCM
  -> signed full page digest
  -> page AES-256-GCM
  -> signed 128-bit block ciphertext digest
  -> block AES-256-GCM
  -> RAW length check or exact-length zstd decode
```

Cache hits do not repeat digest, AEAD, or zstd work. Prepared AES keys are
retained only by the active snapshot and the bounded segment-handle cache.

## Publisher transaction

Segment files are finalized and synchronized before their names are committed.
The complete staged release is then reopened, all segment IDs are verified, and
every logical asset is compared with its source. Only after successful
verification is the synchronized snapshot atomically renamed to `game.haku`.
Unreferenced content-addressed segments are deleted after that commit, never
before it. On Unix the affected directories are synchronized after segment
publication, snapshot replacement, recovery, and garbage collection.

The packer may reuse a signed block from the previous snapshot or an identical
block already written during the current build. Reuse is permitted only when
the segment availability matches, preserving the required/deferred install
boundary.
