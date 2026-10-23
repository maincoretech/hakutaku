# Hakutaku performance baseline

This guide owns I/O/cache policy and measured performance. The normative byte
layout is [FORMAT.md](FORMAT.md); public API contracts live in rustdoc.
Numbers are regression markers rather than storage-device specifications.

The I/O and allocation sections describe the current v2 implementation. Earlier
benchmark/cache tables record v1 development; the [design prototypes](#v2-design-prototypes)
were measured before integration. Use [integrated v2 results](#integrated-v2-results)
for the paired format comparison. Historical commands require the recorded source,
inputs and binaries; a current checkout alone does not reproduce an old baseline.

## Publisher validation and inventory reuse

Measured on 2026-10-23, Apple M5 Pro / 24 GB / macOS 27.0.1, Rust 1.99.0. Baseline is
`f1aeff8`; "checked" adds release verification, catalog bounds and GUI guards;
"optimized" additionally reuses storage statistics and the checked source
inventory, releases build-only indexes early, and encodes map pages without a
second whole-release block-reference vector. Wire v2 and crypto are unchanged.

Values are milliseconds, medians of three process runs. Original/optimized
runs alternate; the checked-only runs were collected separately beforehand.
Filesystem caches are warm; no compilation from this task overlapped timing,
but other desktop activity was not controlled. The existing synthetic
VN fixture uses 64-byte voice catalog entries plus media samples, not a full
voice library or decoded playback. GUI timing covers the cached build and
inventory update, not rendering or the final release-summary lookup.

| Workload | Assets | Original | Checked | Optimized |
|---|---:|---:|---:|---:|
| Initial strict pack | 10k | 535.597 | 545.653 | 535.422 |
| Initial strict pack | 100k | 6,483.138 | 6,774.655 | 7,315.653 |
| Unchanged strict pack | 10k | 309.804 | 471.966 | 485.146 |
| Unchanged strict pack | 100k | 4,233.378 | 6,430.582 | 6,725.947 |
| Cached development pack | 10k | 161.221 | 158.763 | 198.587 |
| Cached development pack | 100k | 2,229.038 | 2,155.630 | 2,251.737 |
| GUI cached build + inventory | 10k | — | 313.456 | 202.063 |
| GUI cached build + inventory | 100k | — | 4,548.862 | 2,183.091 |
| Package open | 100k | 9.806 | 9.838 | 9.925 |
| 1,000 path lookups | 100k | 0.466 | 0.479 | 0.477 |

The demonstrated speed benefit is the GUI inventory path: about 35.5% less time
at 10k and 52.0% at 100k versus the checked/rescanning implementation. Standalone
CLI cached builds did not demonstrate a speed gain (10k was slower in this
sample); the removed metadata traversal is not the dominant end-to-end cost.
Initial strict packing at 100k was also slower; these results do not support a
general publisher-throughput improvement claim. Strict unchanged builds now
verify metadata, segments and source bytes instead of returning early: this
necessary additional work must not be counted as an optimization regression
against equivalent validation, nor hidden from users.

A separate single 100k process per variant, measured outside the sandbox with
`/usr/bin/time -l`, recorded checked -> optimized maximum RSS of 195,018,752 ->
189,022,208 bytes (about -3.1%) and macOS peak memory footprint of 229,671,536 ->
204,522,072 bytes (about -11.0%). These include fixture creation, publisher and
runtime probes; they are not isolated packer peaks or runtime cache budgets.

Reproduce with:

```sh
HAKUTAKU_VN_BENCH=1 HAKUTAKU_VN_BENCH_COUNTS=10000,100000 \
  cargo bench --locked -p hakutaku-pack --bench vn_runtime
```

Local `target/publisher-review/` retains raw logs, baseline/checked source,
executable hashes and exact commands. No dependency or public API was removed.
`pack_directory_with_plan` is additive; its returned inventory describes the
successful build, not subsequent external source edits. Explicit Scan still
performs the current source/release comparison.

### Real resource reads after the publisher fixes

On the same machine/date, compared the original and optimized binaries above
against `keine/projects/tday`: 83 Opus/WebP/shou files, 80,907,088 logical bytes
(77.16 MiB). Three alternating original/optimized process pairs each ran five
passes; values below are medians of the three per-process medians. Each workload
starts with `Package::trim()` and new read sessions. Filesystem caches remain
warm. Timings include authenticated reads and byte comparisons, but exclude
packing, package open, media decoding and playback.

| Read workload (whole batch) | Original ms | Optimized ms | Change |
|---|---:|---:|---:|
| First 4 KiB from each of 83 files | 3.886 | 3.836 | -1.3% |
| All 77.16 MiB, sequential 64 KiB requests | 48.003 | 48.474 | +1.0% |
| Opus: 4,480 random 4 KiB reads | 126.629 | 127.279 | +0.5% |
| WebP: 4,480 random 4 KiB reads | 9.711 | 9.783 | +0.7% |

These small differences do not demonstrate a runtime speed gain or regression.
Backend requested bytes / logical bytes were effectively unchanged: 18.20x
for file heads, 1.00x for sequential reads, 12.48x for audio seeks and 0.85x for
image seeks. Small reads still authenticate whole blocks; repeated image reads
can benefit from the bounded cache within a workload. These counters measure
backend requests, not physical SSD traffic; this is not a cold-storage test.

Exact local commands (run each version in order for runs 1 through 3):

```sh
for run in 1 2 3; do
  for version in baseline after; do
    HAKUTAKU_DESIGN_BENCH=real \
    HAKUTAKU_REAL_ASSETS=/Users/shiftz/dev/keine/projects/tday \
      target/publisher-review/${version}-vn \
      > target/publisher-review/real-${version}-${run}.log
  done
done
```

The six `real-*.log` files retain every pass and byte counter. Source assets are
copied into benchmark-owned temporary fixtures and are not modified.

## I/O model

Hakutaku uses buffered positional I/O on Windows, macOS, Linux, and mobile. It
does not infer NAND geometry or require direct I/O. v2 uses a 128-byte segment header; encrypted blocks remain dense. Buffered
I/O does not require padding every small segment to a 4 KiB boundary.

Unity LZ4 AssetBundles independently compress 128 KiB chunks. Unreal recommends
a 256 KiB Oodle compression block and exposes asynchronous ranged IoStore reads.
Hakutaku uses those workload shapes without depending on either engine:

- <https://docs.unity3d.com/Manual/assetbundles-compression-format.html>
- <https://dev.epicgames.com/documentation/unreal-engine/oodle-data>
- <https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/Core/FIoStoreReader>

## Semantic segment allocation

Separate writers are maintained for every `Availability x AccessClass` pair.
Targets are upper bounds; tail segments are never padded.

| Access class | Block policy | Maximum segment payload |
|---|---|---:|
| Hot | one block up to 32 KiB | 64 MiB |
| Normal, up to 64 MiB | FastCDC, 32–512 KiB, 128 KiB average | 256 MiB |
| Normal, larger | fixed 1 MiB | 256 MiB |
| Streaming | audio fixed 64 KiB; video fixed 256 KiB | configured limit, default 512 MiB |
| Transient | short voice/SFX, fixed 64 KiB | 128 MiB |

Known pre-compressed media is encrypted as `Codec::Raw` without first invoking
zstd. This covers WebP/PNG/JPEG/AVIF, Opus/MP3/Ogg/FLAC, and supported video
containers. WAV and general data retain the normal compress-if-useful policy.

The publisher reports retained segment bytes, unique referenced encrypted block
bytes, and stranded payload after every build. The publisher
performs bounded consolidation of small/sparse segments (64 MiB of unique live
plaintext per build); explicit full rebuilds remain available. The runtime
never performs garbage collection.

## Runtime allocation and latency controls

- Sequential and complete reads recycle ciphertext and decompression buffers
  instead of allocating them again at every block boundary.
- Catalog, page, and block authentication data is encoded into fixed-size stack
  arrays. Runtime block authentication and packing no longer allocate a small
  heap buffer per block.
- A reader retains the handle for its active immutable segment, avoiding the
  shared idle-handle cache lock and lookup at every block boundary. This state
  lives behind `PositionedFile`; Core still has no platform-specific I/O path.
- Cached plaintext retains its decoded `Vec` allocation directly; cache
  admission does not copy it into a second byte allocation.
- A Streaming cursor keeps its current and immediately previous block. Decoder
  patterns such as `A -> B -> A` therefore perform two authenticated segment
  reads rather than three.
- `Asset::prefetch_range` authenticates and decodes a range synchronously into a
  dedicated clock cache. The engine schedules that call on its existing task
  pool, so Core owns neither threads nor an async runtime. Default and
  memory-constrained prefetch budgets are 2 MiB and 512 KiB respectively.
- Until explicit prefetch first populates that cache, ordinary reads bypass its
  mutex through an atomic population flag. `Package::trim` clears the cache and
  returns to the same fast path; enabling prefetch restores normal lookups.
- Hot and admitted Normal content retain their separate cache policy; streaming
  prefetch cannot evict those entries.
- Repeated offset-based reads can retain an `AssetReadSession`; the one-shot
  `Asset::read_at` deliberately remains allocation-simple, while cursors and
  sessions reuse their active segment, buffers, and Streaming block window.

`Package::trim` releases page, plaintext, prefetch, probation, and idle-handle
caches together. Active cursors/read sessions retain their own active segment
handle, reusable decode buffers and current/previous Streaming block window.
Trim does not invalidate those readers or reclaim caller-owned buffers, the
catalog, thread-local codec state, or OS file cache. Cache budgets are not a
whole-process memory limit.

## Historical v1 benchmark protocol

The `stream-32m-v1` fixture contains deterministic incompressible MP4-shaped
data. It reports pack and open latency, 128/256 KiB sequential throughput,
10,000 uniformly random 4 KiB reads, and 10,000 alternating 4 KiB reads across
two adjacent Streaming blocks. Filesystem cache is warm because it cannot be
flushed portably.

```sh
cargo bench -p hakutaku-pack --bench runtime
```

Apple Silicon macOS, 2026-08-12. The after row is the median of three runs; the
before row is the immediately preceding `306ec56` run on the same machine.

| Metric | Before | Buffered cursor/cache | Change |
|---|---:|---:|---:|
| Full pack and staged verification | 115.551 ms | 117.705 ms | noise |
| Signed snapshot open | 0.056 ms | 0.068 ms | +0.012 ms |
| Sequential 128 KiB | 1,612.7 MiB/s | 1,636.3 MiB/s | +1.5% |
| Sequential 256 KiB | 1,629.8 MiB/s | 1,659.3 MiB/s | +1.8% |
| Uniform random 4 KiB | 6,689 IOPS | 6,784 IOPS | +1.4% |
| Two-block short seek 4 KiB | not recorded | 24,325,929 IOPS | structural marker |

The short-seek number measures memory copying after two authenticated warm-up
reads, not storage IOPS. Its durable regression condition is covered by a test:
returning from the second block to the first must not issue another segment
read. Values around one percent remain normal variance on this short APFS
fixture.

The dedup fixture remains four identical 4 MiB files: 16 MiB logical content
produces 26 new blocks, 78 reused references, and 4,198,816 segment bytes.

### Cursor-local crypto and segment state

The following A/B used a fresh archive of `0438e78` as the before build and the
current worktree as the after build. Both were measured in the same session;
each value is the median of three runs. Filesystem cache remains warm.

| Metric | `0438e78` | Fixed AAD + active handle | Change |
|---|---:|---:|---:|
| Full pack and staged verification | 124.473 ms | 117.054 ms | -6.0% |
| Signed snapshot open | 0.070 ms | 0.069 ms | noise |
| Sequential 128 KiB | 1,552.7 MiB/s | 1,588.8 MiB/s | +2.3% |
| Sequential 256 KiB | 1,534.4 MiB/s | 1,587.8 MiB/s | +3.5% |
| Uniform random 4 KiB | 6,339 IOPS | 6,521 IOPS | +2.9% |
| Two-block short seek 4 KiB | 21,652,849 IOPS | 21,851,953 IOPS | noise |

An attempted cursor-local block-map page window was rejected: its same-session
A/B changed sequential throughput by only +0.1%/+0.9% and random reads by
-0.4%, while increasing retained state. This prevents an unproven optimization
from becoming part of the runtime architecture.

### Inactive-prefetch fast path

Kēne currently relies on AssetServer timeline prediction rather than calling
`Asset::prefetch_range`, so the dedicated prefetch cache is normally empty.
Every uncached Streaming/Transient block formerly locked that empty cache
anyway. A population flag now removes the lock from this common path without
changing format, authentication, decoded-buffer ownership, or cache budgets.

The before values are the immediately preceding `01a6434` run; after values are
the median of three runs in the same session. Filesystem cache remains warm.

| Metric | Before | After | Change |
|---|---:|---:|---:|
| Sequential 128 KiB | 1,688.8 MiB/s | 1,761.3 MiB/s | +4.3% |
| Sequential 256 KiB | 1,681.1 MiB/s | 1,771.6 MiB/s | +5.4% |
| Uniform random 4 KiB | 7,077 IOPS | 7,392 IOPS | +4.5% |
| Two-block short seek 4 KiB | 24,564,955 IOPS | 24,615,385 IOPS | noise |

The short-seek fixture remains memory-copy dominated and is included only as a
no-regression marker. Explicit prefetch and trim transitions are covered by a
state test so the atomic hint cannot make populated entries invisible.

## Visual-novel workload matrix

`vn_runtime` builds 10k, 50k, and 100k-asset catalogs dominated by small voice
entries plus representative script, background, character, voice, BGM, and
video files. It measures first and repeated package open, 1,000 random path
lookups, first reads by VN asset class, 1,000 sequential voices, BGM/video
sequential reads with short seeks, and interleaved voice/image/script cursors.
A counting `SegmentSource` reports requested backend bytes, logical requested
bytes, and read amplification. It also compares an unchanged strict incremental
pack with the same work after seeding and reusing the local development cache.

```sh
HAKUTAKU_VN_BENCH=1 cargo bench -p hakutaku-pack --bench vn_runtime
```

Set `HAKUTAKU_VN_BENCH_COUNTS=10000` for a quick smoke fixture. First-open and
second-open results distinguish a colder process-level pass from an immediately
warm pass, but Hakutaku does not claim portable cold-storage numbers: obtaining
those requires a fresh process plus an OS-specific cache reset or reboot on the
actual target device.

Apple Silicon macOS, 2026-08-14, 10k smoke fixture (single run, warm filesystem
cache):

| Metric | Result |
|---|---:|
| Initial strict pack | 502.831 ms |
| Unchanged strict incremental | 277.719 ms |
| Development-cache seed | 278.581 ms |
| Development-cache hit | 136.550 ms |
| Signed package open, first / immediate repeat | 1.623 / 1.638 ms |
| 1,000 random path lookups | 0.241 ms, 0 backend bytes |
| Background / character read amplification | 1.022x / 1.022x |
| Representative voice / 1,000 voices | 1.304x / 1.759x |
| BGM / video sequential plus short seek | 1.126x / 1.063x |

The cached unchanged build was 50.8% faster than strict incremental on this
metadata-heavy fixture. This is a smoke marker, not a claim about 20–40 GiB
projects; the full count matrix and target storage should be measured before a
release decision.

### Pre-compressed media bypass

Apple Silicon macOS, 2026-08-21, single same-session runs with a warm filesystem
cache. The before build is `557ea18`; the after build skips zstd entirely for
known pre-compressed media. Runtime read amplification is unchanged because the
wire codec was already commonly RAW; the gain is publisher-side work avoided.

| Fixture / metric | Before | Raw media policy | Change |
|---|---:|---:|---:|
| 10k initial strict pack | 547.471 ms | 500.604 ms | -8.6% |
| 10k unchanged strict incremental | 307.560 ms | 286.086 ms | -7.0% |
| 10k development-cache seed | 308.603 ms | 290.955 ms | -5.7% |
| 10k development-cache hit | 152.763 ms | 141.342 ms | -7.5% |
| 100k initial strict pack | 6,006.161 ms | 5,730.845 ms | -4.6% |
| 100k unchanged strict incremental | 3,893.427 ms | 3,730.331 ms | -4.2% |
| 100k development-cache seed | 3,915.249 ms | 3,751.604 ms | -4.2% |
| 100k development-cache hit | 1,946.918 ms | 1,880.343 ms | -3.4% |

## Bounded cache population

The clock cache now reuses an explicit list of vacant slots. Initial population
no longer scans every occupied slot for each inserted block; all slots released
by a multi-entry eviction remain reusable. Byte budgets, second-chance eviction
and Hot/Normal/prefetch separation are unchanged. Cursor and offset-session
reads also share one authenticated block-loading implementation.

The opt-in Hot fixture packs unique 128-byte assets, resolves their handles
before timing, then trims and fills the plaintext cache three times per process.
It measures cold plaintext-cache population on a warm filesystem, not file
creation, path lookup, repeated hot reads or cold-device I/O.

```sh
HAKUTAKU_HOT_BENCH=1 cargo +1.99.0 bench --locked -p hakutaku-pack --bench runtime
HAKUTAKU_VN_BENCH=1 HAKUTAKU_VN_BENCH_COUNTS=10000 \
  cargo +1.99.0 bench --locked -p hakutaku-pack --bench vn_runtime
```

Measured 2026-10-13 on Apple M5 Pro, 24 GiB, macOS 27.0.1, APFS, Rust 1.99.0
(LLVM 23.1.1). Before is `ce8fe3c` with the same opt-in benchmark added; after
includes the cache/read changes and updated dependencies. Three alternating
before/after process runs use identical toolchains and warmed filesystem state,
with no simultaneous compilation. Runtime columns are medians of three runs;
Hot columns are medians of nine fills (three per process).

| Metric | Before | After | Change |
|---|---:|---:|---:|
| 10k Hot assets, cache population | 20.155 ms | 6.922 ms | -65.7% |
| 50k Hot assets, cache population | 337.596 ms | 36.813 ms | -89.1% |
| Full pack and staged verification | 130.837 ms | 128.096 ms | -2.1% |
| Signed snapshot open | 0.070 ms | 0.069 ms | noise |
| Sequential 128 KiB | 1,621.8 MiB/s | 1,610.2 MiB/s | -0.7% |
| Sequential 256 KiB | 1,652.7 MiB/s | 1,620.4 MiB/s | -2.0% |
| Uniform random 4 KiB | 6,635 IOPS | 6,578 IOPS | -0.9% |
| Two-block short seek 4 KiB | 24,016,812 IOPS | 23,904,402 IOPS | -0.5% |

The small streaming differences do not establish a throughput gain or regression;
short-seek remains a memory-copy marker. A separate same-dependency comparison
isolates the cache change: 50k population goes from 321.699 to 33.784 ms
(median of three fills, -89.5%). A first updated-dependency run overlapped builds
and showed a short-seek outlier; it is retained in the raw logs but excluded
from the paired medians above. The latest tiny publisher short-read cleanup
and GUI-only changes were validated afterwards, not rebenchmarked.

The dedup fixture retains exactly 26 new blocks, 78 reused references and
4,198,816 segment bytes. The updated-dependency 10k VN smoke reports 508.956 ms
initial pack, 282.548 ms unchanged strict incremental, 136.150 ms cached
incremental, and 1.071/1.002 ms first/repeated package open. Those are single-run
markers; the large gain claimed here is specifically Hot cache population,
not total engine startup or every VN workload. The 50k/100k VN matrices and
other operating systems were not remeasured for this cache change; the separate
next-format investigation below measures 100k metadata.

Raw logs, executable checksums, commands and environment are under
`target/quality-review/`: `before/after-runtime.log` for the isolated change,
`paired-{before,final}-{0,1,2}.log` for final medians, `final-vn.log`,
`commands.txt`, and `machine.txt`.

## V2 design prototypes

The following A/B experiments preceded the v2 implementation. They establish
the choice of compact headers, an 8-byte path index and positional reuse IDs;
they do not replace final integrated stress results. The reader/writer now
implements v2 only. See [format policy](FORMAT.md#compatibility-and-upgrade).

Measured 2026-10-13 on Apple M5 Pro, 24 GiB, macOS 27.0.1, Rust 1.99.0. Baseline
is the reviewed worktree based on `ce8fe3c`, before the full-rebuild sequence fix.
Each layout/policy changes one factor and has a separate Cargo target directory
and executable. Paired results use three alternating process runs; each table
reports the median of the three process medians (seven metadata passes or five
read passes per process). Single-run investigations are identified separately.
No compilation ran during the paired measurements. Filesystem caches are warm;
package caches are trimmed before each read workload. Every returned byte is
checked against the source. Backend counters measure requested bytes and read
calls, **not SSD traffic**. Whole-process RSS includes packing and reference
buffers and is not used as a runtime-memory claim.

The scale fixture has 100,000 files, mostly unique **64-byte synthetic voice
entries**. It tests metadata scale, not realistic audio decode. The separate
real-content fixture copies only 35 Opus, 35 WebP and 13 shou files from tday:
83 files, 80,907,088 logical bytes. Its seek pattern is synthetic (128 uniformly
distributed 4 KiB reads per eligible audio/image file), not an Opus decoder trace.
No 20–40 GiB project, cold device, playback, Android or Windows claim follows.

### Path index: retain hashing, reduce each slot

Each lookup column is 100,000 operations with precomputed paths. All prototypes
keep complete path comparison before returning a match; a truncated fingerprint
is never accepted as proof of path identity. The 4-byte form stores just the
file index; the 8-byte form retains a 32-bit keyed fingerprint.

| 100k assets | Decoded catalog bytes | Stored catalog bytes | Open ms | Hit ms | Miss ms |
|---|---:|---:|---:|---:|---:|
| Current 16-byte slots | 10,044,939 | 1,528,671 | 10.312 | 18.307 | 11.195 |
| 4-byte slots | 6,899,211 | 644,546 | 8.435 | 14.327 | 12.097 |
| 8-byte slots | 7,947,787 | 1,070,866 | 9.225 | 14.544 | 10.643 |

Both compact forms reduce resident catalog bytes deterministically: 31.3% and
20.9%. The 4-byte form saves another 1 MiB at this scale but misses cost 8.1%
more in the paired median. Prefer 8 bytes for a balanced v2 default; 4 bytes is
an alternative if target-device memory measurements justify the tradeoff, not
another runtime format option. Baseline hit medians range from 16.065 to
26.436 ms across processes, so the exact latency percentage is not a guarantee.

A sorted-path binary-search prototype removes almost all index storage
(decoded catalog 5,850,651 bytes), but a single process measured 30.786/31.403 ms
for hits/misses versus 20.484/11.125 ms in that initial baseline. Do not replace
the hash index with it. This does not establish results for other trie/index
designs, which were not implemented or measured.

### Seek granularity: smaller blocks have measurable costs and benefits

| Real content, trimmed package caches | 256 KiB audio/video blocks | 64 KiB blocks |
|---|---:|---:|
| First 4 KiB of every file | 7.114 ms | 3.936 ms |
| Sequential reads, 64 KiB requests | 49.119 ms | 49.444 ms |
| Random audio seeks | 298.814 ms | 129.991 ms |
| Audio-seek segment bytes requested | 541,313,900 | 228,899,601 |
| Audio-seek backend calls | 2,175 | 3,600 |
| Total content blocks | 381 | 1,204 |
| Snapshot bytes, initial single-run comparison | 42,044 | 110,895 |

Random-seek time falls 56.5% and requested segment bytes 57.7%; total calls rise
65.5% and snapshot size is 2.64x. The independent 32 MiB synthetic-stream paired
benchmark reports 6,470 -> 24,761 random 4 KiB IOPS. Sequential 128/256 KiB
throughput is 1,531.5/1,563.3 -> 1,617.9/1,611.4 MiB/s; the initial single run
showed the opposite small sequential difference. There is no stable evidence
here for a sequential throughput regression or a universal throughput gain.
Prioritize 64 KiB for seek-heavy content, and validate decoder traces and target
storage before changing every Streaming/Transient asset's default.

Two alternatives did not earn adoption. A 4 KiB block-map page prototype grew
100k-asset metadata from 785 to 1,669 pages and the snapshot from 8,886,382 to
9,061,360 bytes (single runs), with no meaningful real-content read improvement.
Fixed 128 KiB blocks for compressed images measured 10.067 ms image seeking
versus 10.092 ms for existing CDC in the paired comparison; this is noise and
does not justify giving up content-defined reuse. Keep 16 KiB map pages and CDC
until a workload establishes a better tradeoff. Pack-time samples for real
content were too variable to claim a publisher-speed win for either policy.

### Publisher metadata dominates snapshot replacement

A separate single-run 100k-asset fixture directly decodes the authenticated
catalog and totals its page directory:

| Snapshot component | Stored bytes |
|---|---:|
| Header | 4,096 |
| Catalog | 1,528,239 |
| Block maps | 2,041,170 |
| Publisher-only reuse pages | 5,314,469 |
| Total | 8,887,974 |

Changing one 64-byte voice entry produces 4,176 new segment bytes but rewrites
an 8,893,459-byte snapshot; strict packing takes 5,919.763 ms. This measures
whole-file replacement size, not observed network traffic. Reuse pages account
for 59.8% of the original snapshot despite never being needed by ordinary reads.

The lowest-complexity v2 candidate stores only the 32-byte chunk ID at each
global block position and resolves its existing BlockRef through the block map,
instead of duplicating the same 48-byte BlockRef in every 80-byte ReuseRecord.
A compression-size projection for 100,052 records gives 3,207,936 stored bytes
instead of 5,314,469 (-39.6%); it is **not an end-to-end encoder or measured pack
speedup** and excludes changes in the smaller page directory. A positional
representation must validate equal record totals before joining the tables.

If replacement bandwidth is the next bottleneck, consider immutable reusable
metadata objects and a small signed release root. Moving reuse data to a
publisher-only sidecar could remove most of those bytes from distribution, but
introduces another artifact's authentication and lifecycle. Measure that cost
before adding it. Keep publisher commitments and rollback binding: simply
reusing metadata AEAD nonces across changed plaintext, or removing ciphertext
digests because AEAD already exists, is not an optimization.

### Segment lifetime and release ordering

The patch fixture holds only 200 unique 64-byte files (12,800 bytes total).
Changing a different file per release reaches 128 segments after 127 updates;
update 128 fails with `segment count`. The previously committed release remains
readable and passes segment verification. At that boundary, retained segment
bytes are 550,448, including 10,160 stranded payload bytes.

Compact headers (snapshot 4,096 -> 256, segment 4,096 -> 128 bytes) reduce the
same retained segments to 42,544 bytes (-92.3%). On the real 81 MB corpus they
save only 15,872 segment bytes, so this is a small-segment space improvement,
not a runtime-speed claim. Raising the cap to 65,535 permits all 140 updates,
but leaves 141 segments and 604,736 retained bytes: increasing a limit alone
does not solve fragmentation. v2 should use compact headers, explicit bounded
segment capacity, and publisher-side consolidation informed by retained bytes,
dead payload and segment count. No runtime garbage collector or new async
framework is needed. Consolidation policy needs a large-project cost study;
this tiny fixture cannot justify automatically repacking a 40 GiB release.

Full reconstruction compacts the small fixture to one 20,096-byte segment in
32.400 ms after the correctness fix. The experiment exposed that `--full`
previously reset release 128 to 1 and failed the launcher's persisted rollback
floor. The packer now authenticates the existing snapshot to advance its
sequence even when block reuse is disabled; the same fixture becomes release
129 and passes that floor. A regression also verifies reconstruction from source
when old payload segments are missing, and consecutive unchanged full rebuilds.
An existing invalid/untrusted snapshot is now an error on full reconstruction;
full mode does not bypass authentication or silently reset release history.

### Reproduction and status

The measurement modes extend the existing opt-in bench, with no new dependency:

```sh
HAKUTAKU_DESIGN_BENCH=metadata HAKUTAKU_VN_BENCH_COUNTS=100000 \
  cargo +1.99.0 bench --locked -p hakutaku-pack --bench vn_runtime
HAKUTAKU_DESIGN_BENCH=manifest HAKUTAKU_VN_BENCH_COUNTS=100000 \
  cargo +1.99.0 bench --locked -p hakutaku-pack --bench vn_runtime
HAKUTAKU_DESIGN_BENCH=real HAKUTAKU_REAL_ASSETS=/path/to/tday \
  cargo +1.99.0 bench --locked -p hakutaku-pack --bench vn_runtime
HAKUTAKU_DESIGN_BENCH=patches \
  cargo +1.99.0 bench --locked -p hakutaku-pack --bench vn_runtime
```

`target/design-review/` retains the isolated baseline source archive, variant
patches, runner scripts, executable SHA-256s, raw logs, commands and machine
information. `*-paired-*.log` backs paired tables; `baseline-manifest-extended.log`
backs metadata breakdown, and `*-patches*.log` backs update/compaction findings.
An initial shared-target build accidentally reused baseline binaries; those
invalid comparisons are quarantined in `discarded-shared-target/` and excluded.
The rerun uses per-variant targets and prints compiled layout constants.

At the end of the prototype investigation, production changes were the benchmark modes,
the full-rebuild sequence fix and its regression, and the next-format policy.
The format prototypes were still isolated then. Rust 1.99.0 fmt, all-targets/all-features
workspace check and Clippy, all-features workspace tests (76 passed), and docs
with warnings denied pass. Wire-layout fuzzing, a production v2 migration,
downstream engine integration, packaging, GUI reacceptance and remote CI were
not performed for these prototypes.

## Integrated v2 results

Measured on 2026-10-13 UTC, Apple M5 Pro / 24 GiB / macOS 27.0.1, Rust 1.99.0.
The baseline is the reviewed v1 source archived in `target/design-review/`;
the after build is the complete 0.2.0 worktree. Three process runs alternate
v1/v2 order, with no concurrent compilation. Metadata timing is the median of
three seven-pass medians; real-content timing uses three five-pass medians.
The filesystem cache is warm. Read workloads trim Hakutaku caches and compare
returned bytes with their source. Counts are backend requests, not physical
storage traffic. Randomized identities make stored compression sizes vary
slightly; size tables also report process medians.

### 100,000-resource catalog

| Metric | v1 baseline | v2 |
|---|---:|---:|
| Decoded catalog bytes | 10,044,939 | 7,928,907 |
| Stored catalog bytes | 1,528,732 | 1,058,056 |
| Snapshot bytes | 8,888,370 | 6,304,768 |
| Open ms | 10.484 | 9.355 |
| 100k hits ms | 19.898 | 15.696 |
| 100k misses ms | 11.951 | 10.892 |
| Strict unchanged pack ms | 3,848.874 | 3,775.760 |
| Cached development pack ms | 1,898.455 | 1,895.055 |

Decoded catalog bytes fall 21.1% and total snapshot bytes 29.1%. This includes
the 8-byte path slots, smaller page directory and 32-byte positional reuse IDs.
Hit/miss timing improves in this sample; strict/dev-cache packing changes only
slightly, so no broad publisher-speed claim follows.

### Real tday content

The fixture contains 35 Opus, 35 WebP and 13 shou files, totaling 80,907,088
logical bytes. Audio/image seeks are synthetic traces; this is not playback
latency or decoder CPU measurement.

| Metric | v1 baseline | v2 |
|---|---:|---:|
| First 4 KiB per file, ms | 7.126 | 3.977 |
| Sequential 64 KiB reads, ms | 49.418 | 49.693 |
| Random audio 4 KiB reads, ms | 300.538 | 130.815 |
| Audio-seek segment bytes requested | 541,313,900 | 228,891,665 |
| Audio-seek backend calls | 2,175 | 3,600 |
| Random image 4 KiB reads, ms | 10.173 | 10.107 |
| Snapshot bytes | 42,113 | 73,907 |

Random audio-seek time falls 56.5% and requested segment bytes 57.7%; calls
increase 65.5%. The real-content snapshot grows about 75.5% because 64 KiB audio
blocks raise total block count from 381 to 1,204. Sequential time is effectively
unchanged in this sample. Video remains 256 KiB: the independent 32 MiB video
fixture measures 1,644.7 -> 1,628.7 MiB/s for sequential 256 KiB requests and
6,805 -> 6,819 random 4 KiB IOPS. Those small differences do not establish a
video performance change. Image CDC and 16 KiB block-map pages remain unchanged.

A separate 100k-resource manifest run records 3,209,472 stored reuse-page bytes,
2,038,072 block-map bytes and a 6,306,290-byte snapshot. Changing one 64-byte
voice adds a 208-byte segment but still replaces the whole 6,311,789-byte
snapshot. Immutable metadata sharing and a publisher-only sidecar are not
implemented; the smaller format does not turn snapshot replacement into a delta.

### Stress and boundaries

- 500,000 resources: completed full packing, strict unchanged packing, development
  cache seed/reuse, repeated open and hit/miss probes. Decoded catalog
  37,545,547 bytes; snapshot 31,529,982
  bytes; seven-pass open median 45.684 ms. Full pack
  103.547 s, strict unchanged pack 75.598 s, development cache seed 74.747 s
  and cached rebuild 47.891 s. These end-to-end costs grow more than the
  100k fixture; there is no paired 500k v1 baseline or profiling evidence to
  attribute the bottleneck. Whole-process
  maximum RSS 1,326,743,552 bytes includes fixture generation and publisher data and
  is not a runtime-memory budget measurement.
- 1,024 updates to 200 unique 64-byte files: all publish and final byte checks
  pass, 16 automatic consolidations, at most 64 active segments, peak retained
  storage 29,232 bytes. Final explicit reconstruction advances release 1,025
  to 1,026 and passes the previous rollback floor. The earlier v1 fixture failed
  at update 128. This small-fragment test does not model a 40 GiB update.
- Eight concurrent readers perform 65,536 random 8 KiB reads while repeatedly
  trimming package caches. All 512 MiB of returned data matches the 64 MiB
  reference asset; 411.760 ms, 4,861,298,011 requested backend bytes and 76,891
  calls in this single correctness/stress run. Whole-process RSS is 74,498,048
  bytes and includes the reference copy. Counters include metadata requests.
- A regression alternates strict/dev-cache publication for 140 updates, checks
  every asset and verifies segment contents. Budget tests count duplicate
  physical references once and reject compressed-size tricks: an 80 MiB live
  plaintext segment cannot bypass the 64 MiB consolidation budget.
- Actual v1 and v2 CLI binaries reject each other's packages. Reusing the
  existing identity with an explicit floor produces release 42; minimum 42
  accepts and 43 rejects. Attempting to publish v2 into v1 output preserves the
  prior snapshot unchanged. No legacy format fallback is present.

Reproduce stress through the existing bench:

```sh
HAKUTAKU_DESIGN_BENCH=metadata HAKUTAKU_VN_BENCH_COUNTS=500000 \
  cargo bench --locked -p hakutaku-pack --bench vn_runtime
HAKUTAKU_DESIGN_BENCH=patches HAKUTAKU_PATCH_UPDATES=1024 \
  cargo bench --locked -p hakutaku-pack --bench vn_runtime
HAKUTAKU_DESIGN_BENCH=stress \
  cargo bench --locked -p hakutaku-pack --bench vn_runtime
```

CI adds 100k-catalog, 512-update and concurrent-reader jobs with log artifacts;
release depends on their success. Timing has no host-dependent pass threshold.
Local evidence is retained in `target/v2-review/`: `commands.json`, `run.py`,
`binary-sha256.txt`, paired raw logs, stress logs, `summary.json`, and
`format-isolation.json`. The baseline source archive and variant history stay
in `target/design-review/`. Cross-platform runtime/GUI, cold storage, 20–40 GiB
content, decoder traces and remote CI remain separate acceptance gates.
