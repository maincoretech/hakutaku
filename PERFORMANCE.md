# Hakutaku performance baseline

The checked-in benchmark uses a deterministic 32 MiB incompressible MP4-shaped
asset, 256 KiB runtime reads, and 10,000 random 4 KiB reads. It measures the
authenticated `Package`/`AssetCursor` path with warm filesystem cache; it is not
a cold-device or decoder benchmark.

```sh
cargo bench -p hakutaku-pack --bench runtime
```

## 2026-08-11 transaction and first-release dedup pass

The before row is a warm run at `3dd87ba`. The after row is the median of three
runs after intra-release deduplication, availability isolation, and durable
publisher directory commits. Values inside roughly one percent are treated as
noise on this short APFS fixture.

| Metric | Before | After | Change |
| --- | ---: | ---: | ---: |
| Full pack and staged verification | 120.760 ms | 114.979 ms | -4.8% |
| Signed snapshot open | 0.060 ms | 0.065 ms | +0.005 ms |
| Sequential authenticated read | 1,618.4 MiB/s | 1,611.8 MiB/s | -0.4% |
| Random authenticated 4 KiB read | 6,540 IOPS | 6,503 IOPS | -0.6% |

Runtime throughput is unchanged within noise. The structural improvement is in
publisher output: four identical 4 MiB files (16 MiB logical) produce 26 new
blocks and 78 reused references, with 4,198,816 segment bytes. That is about a
75% reduction compared with storing every copy independently, without weakening
per-block AES-GCM authentication or the signed ciphertext commitment.

The benchmark deliberately prints both logical bytes and physical segment bytes
so future changes cannot claim deduplication from timing alone.
