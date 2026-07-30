# Benchmarks

Recorded baseline and candidate results are in [RESULTS.md](RESULTS.md).

## Upload loopback benchmark

The upload benchmark uses a loopback HTTP sink and the public library API. It
does not weaken the production CLI's host validation, contact GigaFile, or keep
the generated source file.

Build the driver with the same locked release profile for every revision:

```sh
cargo build --locked --release --example upload_loopback_benchmark
```

Run a 1 GiB upload with 64 MiB chunks, two warmups, and 15 measured samples:

```sh
python3 benchmarks/run_upload_benchmark.py \
  --binary target/release/examples/upload_loopback_benchmark \
  --threads 1
```

Repeat with `--threads 4` for bounded read-ahead. The runner reports median
wall time, median absolute deviation, throughput, peak resident memory on
Linux, and process-start-to-first-POST latency.

For CPU counters on Linux, start `benchmarks/upload_sink.py`, then use:

```sh
perf stat -r 5 \
  -e task-clock,cycles,instructions,cache-misses,context-switches,page-faults \
  target/release/examples/upload_loopback_benchmark \
  http://127.0.0.1:PORT/ SOURCE_FILE 67108864 1 1
```

Compare revisions with the same Rust toolchain, lockfile, source file, CPU
affinity, and server process. Interleave revision order when the difference is
near the noise floor. Hash or byte-count validation belongs outside the timed
region.

For a release comparison, run two interleaved rounds per thread count. Treat a
candidate as a performance regression when its median is more than 5% slower
and the change is larger than two combined median absolute deviations. Treat a
peak-RSS increase above 10% as a separate regression even when wall time is
unchanged.

## Partial-file and parser microbenchmarks

Build the dependency-free benchmark driver:

```sh
benchmarks/run_parts_parser.sh build
```

Create a fixture directory and benchmark `parts list` discovery:

```sh
fixture_dir="$(mktemp -d)"
target/release/examples/parts_parser_benchmark \
  prepare-parts "$fixture_dir" 10000
target/release/examples/parts_parser_benchmark \
  parts "$fixture_dir" 15 1
```

Benchmark single-file and matomete HTML parsing:

```sh
target/release/examples/parts_parser_benchmark parser-single 25 2000
target/release/examples/parts_parser_benchmark parser-matomete 1000 15 10
target/release/examples/parts_parser_benchmark parser-matomete 10000 15 1
```

The driver prints nanosecond medians, median absolute deviations, minima, and
maxima. Put fixtures on the same filesystem, warm both revisions equally, and
interleave revision order. Remove the temporary fixture directory after the
run.
