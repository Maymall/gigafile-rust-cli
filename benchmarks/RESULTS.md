# Benchmark Results

These measurements compare the restored interrupted hardening snapshot with the
completed candidate on 2026-07-30. The snapshot is preserved by
`backup/interrupted-hardening-2026-07-30` and the external recovery bundle
described in the project handoff.

## Environment

- Linux 7.1.4, x86_64
- AMD Ryzen 7 7840HS, 8 cores / 16 threads
- Rust and Cargo 1.97.1
- Locked dependencies and the release profile from each revision
- 1 GiB sparse source, 64 MiB chunks, loopback HTTP sink

The upload comparison used two interleaved rounds for every revision and thread
count. Each round had two warmups followed by 15 measured iterations. Table
values are the median of the two round medians.

## Upload hot path

| Metric | Interrupted snapshot | Candidate | Change |
| --- | ---: | ---: | ---: |
| Wall time, 1 thread | 575.64 ms | 353.26 ms | -38.6% |
| Throughput, 1 thread | 1.74 GiB/s | 2.83 GiB/s | +62.9% |
| Wall time, 4 threads | 572.82 ms | 379.82 ms | -33.7% |
| Throughput, 4 threads | 1.75 GiB/s | 2.63 GiB/s | +50.8% |
| Peak RSS, 4 threads | 275,834 KiB | 255,206 KiB | -7.5% |
| Process start to first POST, 4 threads | 123.84 ms | 20.93 ms | -83.1% |

The individual medians were 573.43–577.86 ms versus 352.16–354.36 ms
for one thread, and 565.19–580.45 ms versus 377.36–382.28 ms for four
threads. The ranges stayed well outside the combined median absolute
deviations.

Linux `perf stat -r 5` on the one-thread case independently confirmed less
client-side work:

| Counter | Interrupted snapshot | Candidate | Change |
| --- | ---: | ---: | ---: |
| Elapsed time | 659.75 ms | 365.93 ms | -44.5% |
| Task clock | 897.73 ms | 613.88 ms | -31.6% |
| CPU cycles | 508,484,092 | 262,587,868 | -48.4% |
| Instructions | 336,210,335 | 180,950,122 | -46.2% |
| Cache misses | 10,121,301 | 4,855,266 | -52.0% |

The improvement comes from removing the per-read mutex/seek/allocation path,
using positional reads from the already-open source, keeping prefetched chunks
as `Bytes`, overlapping later prefetch with the active POST, and making hidden
progress accounting lock-free.

## Partial-file discovery and HTML parsing

These are paired medians from the standalone benchmark driver:

| Case | Interrupted snapshot | Candidate | Change |
| --- | ---: | ---: | ---: |
| `parts list`, 1,000 groups | 24.80 ms | 13.75 ms | -44.6% |
| `parts list`, 10,000 groups | 255.28 ms | 132.36 ms | -48.2% |
| Single-file parser | 19.05 us | 8.74 us | -54.1% |
| Matomete parser, 1,000 files | 8.48 ms | 6.11 ms | -27.9% |
| Matomete parser, 10,000 files | 84.93 ms | 62.93 ms | -25.9% |

For four scans of 1,000 partial groups, `strace` recorded:

| Syscall group | Interrupted snapshot | Candidate | Change |
| --- | ---: | ---: | ---: |
| `statx` | 48,000 | 12,000 | -75.0% |
| `openat` | 12,010 | 8,010 | -33.3% |
| `read` | 16,007 | 8,007 | -50.0% |
| Selected calls total | 96,033 | 44,033 | -54.1% |

The candidate samples each path's metadata once, reads each sidecar once, reuses
compiled selectors and regular expressions, builds one DOM for single-file
pages, and avoids per-item selector compilation and temporary strings.

A final candidate-only run after the code freeze measured 13.52 ms for 1,000
partial groups, 151.86 ms for 10,000 groups, 7.91 us for a single page,
5.70 ms for 1,000 matomete entries, and 60.06 ms for 10,000 entries. The
10,000-group filesystem case varied more than the parser cases but remained
well below the interrupted baseline.

These are local CPU, memory, and filesystem measurements. They deliberately do
not claim improvements to GigaFile service latency or public-network
throughput.
