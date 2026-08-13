# Kotlin vs Rust benchmark

This benchmark compares the retained Kotlin implementation (`//cli:bazel-diff_deploy.jar`) with
the Rust candidate (`//:bazel-diff-rust`) on a real, pinned checkout of Bazel.

## Workload

- Repository: `https://github.com/bazelbuild/bazel.git`
- Commit: `0c2f428c45ffd9139f5f97a2407cde591b2357e7`
- Bazel: `9.2.0`
- Output: normalized Kotlin and Rust target/hash maps were exactly equal
- Host: 16 Intel Xeon Platinum 8336C cores, 31 GiB RAM, Linux x86_64

## Pure bazel-diff processing

This is the primary implementation benchmark and the default mode of `tools/benchmark.py`. A 22 MiB
`streamed_proto` query result is captured once from the pinned repository before Hyperfine starts.
A replay shim answers Bazel metadata calls and copies that fixed protobuf to the requested output
path. No Bazel server runs during measurement. Pass `--streamed-proto` (or `STREAMED_PROTO` through
Make) to reuse a previously captured fixture.

`hyperfine 1.20.0` ran 5 warmups and 20 measured invocations:

| Implementation | Mean wall time | Range | Median peak RSS |
| --- | ---: | ---: | ---: |
| Kotlin | 2.732 ± 0.128 s | 2.467–2.921 s | 889.15 MiB |
| Rust candidate | 0.461 ± 0.023 s | 0.429–0.508 s | 140.66 MiB |

The Rust candidate was **5.92x faster** and used **84.2% less peak RSS**. Both implementations
produced 24,147 normalized hashes with canonical SHA-256:
`f35f73dcb65b44d745a8308850b9124fb792a19d61ad515b9922bae3c7da1689`.

Peak RSS was sampled separately from hyperfine over 10 runs after 3 warmups. It includes the Kotlin
JVM or Rust worker threads, but no Bazel process.

## End-to-end generate-hashes

This secondary benchmark includes Bazel startup/query time as well as bazel-diff processing. Each
implementation receives its own Bazel output base and server. `tools/benchmark.py` invokes
`hyperfine --export-json` for four named commands: Kotlin/Rust crossed with cold/warm. Hyperfine
owns all wall-time runs and statistics. Select this mode with `--include-bazel` or
`INCLUDE_BAZEL=1`.

- Before every cold timing, Hyperfine's prepare hook shuts down that implementation's Bazel server.
- Before every warm timing, the prepare hook shuts down the server and runs one unmeasured
  `generate-hashes` to prime it.
- Every measured iteration writes a distinct output using `$HYPERFINE_ITERATION`; the runner
  compares Kotlin/Rust and cold/warm output maps before accepting the result.
- Both implementations pass `--excludeExternalTargets`.

Peak process-tree RSS is sampled separately from the timed Hyperfine runs so Python can include the
bazel-diff process and attached children such as the Kotlin JVM and Bazel client. Bazel's detached
long-lived server daemon is excluded. Cold means a cold Bazel server, not a cold operating-system
page cache or a redownloaded Bazel binary.

## Results

| Phase | Implementation | Median wall time | Median peak RSS | Rust improvement |
| --- | --- | ---: | ---: | ---: |
| Cold | Kotlin | 7.895 s | 725.5 MiB | |
| Cold | Rust | 5.489 s | 152.2 MiB | 1.44x faster, 79.0% less RSS |
| Warm | Kotlin | 3.999 s | 816.2 MiB | |
| Warm | Rust | 1.489 s | 152.1 MiB | 2.69x faster, 81.4% less RSS |

Raw wall-time samples:

- Kotlin cold: 7.895 s, 7.850 s, 8.015 s, 7.589 s, 8.067 s
- Rust cold: 5.430 s, 5.774 s, 5.489 s, 5.638 s, 5.473 s
- Kotlin warm: 3.999 s, 4.031 s, 3.753 s, 4.076 s, 3.847 s
- Rust warm: 1.489 s, 1.570 s, 1.434 s, 1.481 s, 1.505 s

These results used Hyperfine 1.20.0 with 2 warmup runs and 5 measured runs per named command.
Peak RSS is the median of 3 separate samples per implementation and phase.

The end-to-end output maps contained 24,243 targets and had the same canonical SHA-256:
`36c3b0fa492845bf4a2bbd3f9f22add66df1ef5ab7a0c2a830771c92734a87f3`.

## Reproduce

Fetch the exact workload commit:

```bash
git init /tmp/bazel
git -C /tmp/bazel remote add origin https://github.com/bazelbuild/bazel.git
git -C /tmp/bazel fetch --depth 1 origin 0c2f428c45ffd9139f5f97a2407cde591b2357e7
git -C /tmp/bazel checkout --detach FETCH_HEAD
```

Then run:

```bash
make benchmark \
  WORKSPACE=/tmp/bazel \
  BAZEL=/path/to/bazelisk \
  HYPERFINE=/path/to/hyperfine \
  ITERATIONS=10 \
  WARMUP=3 \
  RSS_RUNS=5 \
  JSON=/tmp/bazel-diff-benchmark.json
```

The outer `bazel run -c opt` builds both implementations once and injects the Kotlin launcher and
Rust binary through runfiles. The benchmark executes those runfiles directly; it does not run a
nested Bazel build. It records the Hyperfine version and raw wall-time samples plus separate
process-tree RSS samples and environment metadata in JSON, and exits non-zero if any normalized hash
maps differ.

To reproduce the secondary Bazel-inclusive table instead, add `INCLUDE_BAZEL=1`.
