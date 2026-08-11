# I/O performance baseline

Hakutaku optimizes for the buffered filesystem APIs available on Windows,
macOS, Linux, and mobile platforms. It does not infer NAND geometry or require
direct I/O. The 4 KiB segment header keeps the first encrypted block naturally
page-aligned; subsequent compressed blocks remain densely packed to avoid
padding overhead.

## Reference engine workloads

- Unity LZ4 AssetBundles independently compress 128 KiB chunks and can load
  only the chunks needed by an object.
- Unreal recommends a 256 KiB compression block for Oodle-backed Pak/IoStore
  content; IoStore supports ranged, asynchronous, encrypted and compressed
  block reads.

These are workload shapes, not directly comparable engine benchmark numbers:

- <https://docs.unity3d.com/Manual/assetbundles-compression-format.html>
- <https://dev.epicgames.com/documentation/unreal-engine/oodle-data>
- <https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/Core/FIoStoreReader>

The local `runtime` bench therefore reports sequential throughput at both
128 KiB and 256 KiB request sizes, 4 KiB random-read IOPS, snapshot-open time,
pack time, and block-level deduplication. Run it with:

```sh
cargo bench -p hakutaku-pack --bench runtime
```

## Semantic segment allocation

Segment targets are upper bounds; a tail segment is never padded. Separate
writers are maintained for every `Availability x AccessClass` pair:

| Access class | Block policy | Maximum segment payload |
|---|---|---:|
| Hot | one block up to 32 KiB | 64 MiB |
| Normal, up to 64 MiB | FastCDC, 32-512 KiB, 128 KiB average | 256 MiB |
| Normal, larger | fixed 1 MiB | 256 MiB |
| Streaming | fixed 256 KiB | configured limit, default 512 MiB |
| Transient | caller-declared | 128 MiB |

The publisher reports retained segment bytes, unique referenced encrypted
block bytes, and stranded payload bytes after each build. A full rebuild is
the single compaction mechanism; the runtime never performs garbage
collection.

## Baseline captured on 2026-08-11 and 2026-08-12

Apple Silicon macOS, 32 MiB deterministic incompressible media fixture:

| Revision | Pack | Open | Sequential 128 KiB | Sequential 256 KiB | Random 4 KiB |
|---|---:|---:|---:|---:|---:|
| `3cb7a04` before semantic allocation | 120.050 ms | 0.062 ms | not recorded | 1659.2 MiB/s | 6701 IOPS |
| semantic allocation | 119.952 ms | 0.078 ms | 1669.0 MiB/s | 1674.5 MiB/s | 6599 IOPS |

Results are local regression markers, not storage-device specifications. The
OS page cache cannot be flushed portably, so runtime throughput is a warm-cache
CPU/decryption/read-path measurement.
