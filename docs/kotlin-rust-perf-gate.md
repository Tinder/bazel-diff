# Kotlin vs Rust performance gate

The Rust implementation exists to be faster than the Kotlin one. This gate turns that
sentence into a check that can fail: `tools/perf_gate.py` runs both binaries over
generated workloads and exits non-zero unless Rust wins.

It is not the same tool as [`docs/kotlin-vs-rust-benchmark.md`](kotlin-vs-rust-benchmark.md):

| | `tools/benchmark.py` | `tools/perf_gate.py` |
| --- | --- | --- |
| Question | how much faster is Rust on *this* repository? | is Rust still faster than Kotlin at all? |
| Inputs | a real checkout, Bazel, Hyperfine | none -- fixtures are generated |
| Runs in CI | no (manual, needs a workspace) | nightly cron (and `workflow_dispatch`) |
| Output | a report | a report **and an exit code** |

## Running it

```terminal
make perf-gate                                  # default workloads, 5 measured rounds
make perf-gate SCALE=4 ROUNDS=9                 # bigger graphs, more rounds
make perf-gate JSON=/tmp/perf-gate.json         # absolute path: bazel run changes directory
bazel run -c opt //tools:perf-gate -- --list-workloads
```

Against binaries you built yourself (this is what CI does, and it avoids Bazel competing
for cores while timings are taken):

```terminal
bazel build -c opt //cli:bazel-diff //src:bazel-diff
python3 tools/perf_gate.py \
  --kotlin-binary bazel-bin/cli/bazel-diff \
  --rust-binary bazel-bin/src/bazel-diff
```

Always measure `-c opt` builds of both. Comparing a debug build against an optimized one
measures the build flags.

Exit codes: `0` both fast enough, `1` a workload missed a threshold, `2` the run itself
failed (a binary crashed, the two implementations disagreed about the answer, or the
arguments were wrong).

## Workloads

Every workload is a pure function of its spec, so both implementations parse identical
bytes and any difference in timing is a difference in code.

| Workload | What it exercises |
| --- | --- |
| `startup` | `--version`: the fixed cost of launching each CLI (JVM boot vs `exec`) |
| `generate-hashes-small` | 4,800 targets: proto decode, rule/source hashing, transitive digests |
| `generate-hashes-large` | 24,000 targets: the same path where per-target cost dominates |
| `generate-hashes-dense` | 192,000 targets, fan-in 12: sustained concurrent allocation in the parallel proto decode -- stresses allocator scalability (see below) |
| `get-impacted-targets` | diffing 150,000 hashed targets |
| `get-impacted-targets-distances` | the same diff plus build-graph distance metrics over dependency edges |
| `serve-small` | `serve`: time-to-ready plus `/impacted_targets` request latency against a small workspace |
| `serve-large` | the same, against a larger workspace with more requests fired |

`--scale` multiplies the sizes; `--workload` restricts the run (the `startup` baseline is
always measured, because the other workloads' adjusted numbers derive from it). Pass
`--skip-serve` to leave out `serve-small`/`serve-large` -- starting two HTTP servers per
round is much slower than the hermetic process workloads, which matters most for fast
local iteration. `--serve-requests`/`--serve-concurrency` override the request volume and
concurrency the serve workloads run with.

### The `serve` workloads

`serve` does not fit the "run once, time it, diff the output" shape every other workload
uses: it is a long-running HTTP query server, so the measurement is "start it, wait for
`/health`, fire many requests, then kill it" instead. `tools/perf_serve.py` is the parallel
pipeline for that: it builds a small real git repository (via
`perf_workload.write_git_workspace` -- unlike the replay-`bazel`-shim workloads, `serve`
actually clones and checks out real revisions), starts both servers against it one at a
time (alternating which one starts first each round, for the same anti-drift reason as the
other workloads), fires a batch of `/impacted_targets` requests at each with a thread pool,
and diffs the JSON responses for parity before comparing latency.

Its report reuses the request latency's median for the primary speedup and win-rate
columns, and repurposes the "logic" (startup-adjusted) column to carry each server's
**time-to-ready** (first `/health` 200) instead -- server boot (git operations, port bind,
HTTP server init) is a different cost from request latency, not the same "subtract process
start-up" adjustment the process workloads make, so read that column as "who starts up
faster," not "who processes faster once warm."

Rust's `serve` has no `--portFile` flag (Kotlin's exists for OS-assigned ephemeral ports).
Both implementations are therefore driven through a pre-picked free port
(`bind(("127.0.0.1", 0))`, read the port, close, reuse the number) -- the same small
bind/release race already accepted by the Rust end-to-end test harness.

`tools/perf_workload.py` builds the fixtures:

* a **synthetic `streamed_proto` fixture** -- a length-delimited stream of
  `blaze_query.Target` messages encoded directly in Python, shaped like the output of
  `bazel query //...:all-targets --output=streamed_proto --proto:instantiation_stack`.
  The graph is layered (package *i* depends only on the layer below it), so it has real
  depth and fan-in without degenerating into one long chain;
* the **workspace those targets name** -- source files and `.bzl` files on disk, so
  source hashing and `.bzl`-seed hashing read real bytes;
* a **replay `bazel` shim** answering `version`, `mod`, `info` and `query` from that
  fixture. No Bazel server starts, so the measured wall time is bazel-diff's own work;
* **hash-file pairs** for the diff commands, where the second revision changes, adds and
  removes targets.

### Allocator scalability (`generate-hashes-dense`)

`generate-hashes` decodes the `streamed_proto` stream in parallel: each batch of messages
is decoded across the Rayon pool, and every proto field (`rule_class`, each attribute, and
one `rule_input` per dependency) becomes a short-lived heap allocation. With enough targets
and fan-in, all worker threads allocate at once, so the run's speed becomes a function of
how well the global allocator scales under concurrency.

An allocator with per-thread arenas (glibc's default) absorbs this and the Rust build stays
several times faster than Kotlin. An allocator with a single global lock serializes every
worker on that lock; system time explodes and throughput scales *negatively* with core
count. The published Rust release is a **static musl** binary, and musl's malloc uses a
single arena -- so on a many-core host this workload would make the release binary as slow
as, or slower than, Kotlin, while a glibc build of the same source passed comfortably. The
release therefore links **mimalloc** as its `#[global_allocator]` (see `src/main.rs`), whose
per-thread heaps restore scaling; this workload exists to keep it that way.

To reproduce the underlying problem, drop the `#[global_allocator]` and point `--rust-binary`
at the static-musl release on a many-core machine, or approximate it with a glibc build under
`MALLOC_ARENA_MAX=1`. The smaller graphs do not reach a high enough concurrent allocation
rate to surface it, which is why the dense graph exists as a separate load.

The nightly CI run builds the host **glibc** binary, which is stable on any runner and never
storms -- so it would not catch an allocator regression by itself. The gate therefore runs as
two legs (see `.github/workflows/perf-gate.yaml`): the glibc leg is the "Rust is faster"
check, and a second leg builds the published `--config=release-musl` binary and runs
the dense load against it. That leg needs a **many-core runner** -- the storm does not appear
below ~8 cores, so a 2-vCPU runner would pass it regardless of the allocator. Locally,
`make perf-gate-musl` reproduces it on a many-core host.

## Protocol

The gate is built to avoid the three ways a naive A/B benchmark lies:

1. **Different work.** Before timing anything, both binaries run the workload once and
   their outputs are normalized and compared (hash maps, sorted label lists, JSON
   metrics). If they disagree, the run fails with a parity error instead of reporting a
   speedup over work that was never equivalent.
2. **Drift.** Rounds are interleaved -- both binaries run every round, alternating which
   one goes first. A runner that slows down halfway through penalizes both. The first
   `--warmup-rounds` rounds (default 1) are discarded to pay for cold page cache and a
   cold JIT.
3. **Winning on start-up alone.** Rust would beat a JVM on `--version` no matter how slow
   its hashing became. So each workload also reports a **startup-adjusted** time: the
   median wall time minus that implementation's own `startup` median. That is the number
   that reflects the hashing and diffing logic, and it is gated separately. When a
   workload is too short for the subtraction to mean anything, the report says so rather
   than inventing a ratio.

## Thresholds

| Flag | Default | Meaning |
| --- | --- | --- |
| `--min-speedup` | `1.0` | required Kotlin/Rust ratio of median wall times |
| `--min-win-rate` | `1.0` | fraction of paired rounds Rust must win -- `1.0` means *every* round |
| `--min-logic-speedup` | `1.0` | the same bar for startup-adjusted medians |
| `--max-rss-ratio` | unset | with `--rss-runs N`, fail if Rust's median peak RSS exceeds this multiple of Kotlin's |

The defaults say what the project means by "Rust is faster": faster on the median, faster
on the logic once start-up is discounted, and faster in every single paired round.

## Reference numbers

A run on a 4-core Linux x86_64 container (`--rounds 3 --warmup-rounds 1`), Kotlin
`bazel-diff` 40.0.2 against the Rust binary from the same tree:

| Workload | Kotlin median | Rust median | Speedup | Startup-adjusted | Rounds won |
| --- | ---: | ---: | ---: | ---: | ---: |
| `startup` | 0.246 s | 0.002 s | 139.0x | n/a | 100% |
| `generate-hashes-small` (4,800 targets) | 0.814 s | 0.041 s | 19.8x | 14.4x | 100% |
| `generate-hashes-large` (24,000 targets) | 1.263 s | 0.146 s | 8.6x | 7.0x | 100% |
| `get-impacted-targets` (150,000 targets) | 1.182 s | 0.379 s | 3.1x | 2.5x | 100% |
| `get-impacted-targets-distances` (40,000 targets) | 0.708 s | 0.164 s | 4.3x | 2.9x | 100% |

Every workload also passed its parity check, so the two implementations agreed on the
answer before any of these timings were compared. Treat the absolute numbers as
machine-specific -- what the gate enforces is the direction, not these values. The
startup-adjusted column is the one to watch: it is what remains after the JVM's ~0.24 s
boot is discounted, and it is still 2.5x-14x in Rust's favour.

## When the gate fails

Read the failing line first -- it names which threshold missed and by how much.

* **`median speedup ...x is below the required ...`** on one workload only: a regression
  in that code path. Reproduce locally with
  `make perf-gate WORKLOAD=<name> SCALE=4` and profile the Rust side.
* **`startup-adjusted speedup ... is below ...` while the plain speedup passes**: Rust is
  only ahead because the JVM takes time to boot. The logic itself regressed.
* **`rust won N% of paired rounds`** with a healthy median: the two are close enough that
  noise decides rounds. That is itself a finding -- Rust is supposed to win comfortably --
  but confirm on a quiet machine with `--rounds 15` before treating it as a regression.
* **`implementations disagree`** (exit 2): not a performance problem. The two
  implementations computed different results for identical input; fix the correctness bug
  before the timings mean anything.
* **`command failed`** (exit 2): one binary exited non-zero. The message includes its
  stderr tail.

Do not "fix" a failure by lowering a threshold in CI. The flags exist for local
experimentation on noisy hardware.

## Testing the gate

`//tools:perf_gate_test` drives the runner with stub binaries whose relative speed the
test controls, which is the only way to assert that the gate *fails* when Rust is not
faster -- a run against the real binaries can only ever show that it passes. It also
checks the fixture encoder against an independent protobuf decoder and runs the replay
shim. It is hermetic and fast, and runs in the standard CI matrix alongside
`//tools:benchmark_test`.

`//tools:perf_serve_test` covers the serve pipeline the same way, but the stand-in is a
stub HTTP server (Python's `http.server`) rather than a stub script that exits -- `serve`
has no output file to diff, so parity is checked over request/response bodies instead.
