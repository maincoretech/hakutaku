# Hakutaku performance baseline

This is the single source of truth for Hakutaku layout and runtime performance.
Numbers are regression markers rather than storage-device specifications.

## I/O model

Hakutaku uses buffered positional I/O on Windows, macOS, Linux, and mobile. It
does not infer NAND geometry or require direct I/O. The 4 KiB segment header
page-aligns the first encrypted block; subsequent compressed blocks remain
dense so padding does not consume storage or bandwidth.

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
| Streaming | fixed 256 KiB | configured limit, default 512 MiB |
| Transient | short voice/SFX, fixed 256 KiB | 128 MiB |

Known pre-compressed media is encrypted as `Codec::Raw` without first invoking
zstd. This covers WebP/PNG/JPEG/AVIF, Opus/MP3/Ogg/FLAC, and supported video
containers. WAV and general data retain the normal compress-if-useful policy.

The publisher reports retained segment bytes, unique referenced encrypted block
bytes, and stranded payload after every build. A full rebuild is the single
compaction mechanism; the runtime never performs garbage collection.

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
- Hot and admitted Normal content retain their separate cache policy; streaming
  prefetch cannot evict those entries.
- Repeated offset-based reads can retain an `AssetReadSession`; the one-shot
  `Asset::read_at` deliberately remains allocation-simple, while cursors and
  sessions reuse their active segment, buffers, and Streaming block window.

`Package::trim` releases page, plaintext, prefetch, probation, and idle-handle
caches together. Active readers keep only their current segment handle until
the reader is dropped or moves to another segment.

## Benchmark protocol

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

## Visual-novel workload matrix

`vn_runtime` builds 10k, 50k, and 100k-asset catalogs dominated by small voice
entries plus representative script, background, character, voice, BGM, and
video files. It measures first and repeated package open, 1,000 random path
lookups, first reads by VN asset class, 1,000 sequential voices, BGM/video
sequential reads with short seeks, and interleaved voice/image/script cursors.
A counting `SegmentSource` reports physical backend bytes, logical requested
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
