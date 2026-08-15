# Kotlin vs Rust performance gate

The Rust implementation exists to be faster than the Kotlin one. This gate turns that
sentence into a check that can fail: `tools/perf_gate.py` runs both binaries over
generated workloads and exits non-zero unless Rust wins.

It is not the same tool as [`docs/kotlin-vs-rust-benchmark.md`](kotlin-vs-rust-benchmark.md):

| | `tools/benchmark.py` | `tools/perf_gate.py` |
| --- | --- | --- |
| Question | how much faster is Rust on *this* repository? | is Rust still faster than Kotlin at all? |
| Inputs | a real checkout, Bazel, Hyperfine | none -- fixtures are generated |
| Runs in CI | no (manual, needs a workspace) | yes, on every pull request |
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
| `get-impacted-targets` | diffing 150,000 hashed targets |
| `get-impacted-targets-distances` | the same diff plus build-graph distance metrics over dependency edges |

`--scale` multiplies the sizes; `--workload` restricts the run (the `startup` baseline is
always measured, because the other workloads' adjusted numbers derive from it).

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
