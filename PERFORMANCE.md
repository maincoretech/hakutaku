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
| Transient | caller-declared | 128 MiB |

The publisher reports retained segment bytes, unique referenced encrypted block
bytes, and stranded payload after every build. A full rebuild is the single
compaction mechanism; the runtime never performs garbage collection.

## Runtime allocation and latency controls

- Sequential and complete reads recycle ciphertext and decompression buffers
  instead of allocating them again at every block boundary.
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

`Package::trim` releases page, plaintext, prefetch, probation, and idle-handle
caches together.

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
