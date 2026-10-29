# bazel-diff vs Tango performance harness

[`tango_compare.py`](tango_compare.py) benchmarks bazel-diff against
[Uber's Tango](https://github.com/uber/tango) (Target Analyzer in Go) on the same workspaces and
commit pairs, and cross-checks that both tools report the same impacted targets. The
[`perf-tango-compare` workflow](../../.github/workflows/perf-tango-compare.yml) runs it nightly
and publishes the comparison as a step summary plus a 90-day metrics artifact.

## What is compared

Both tools compute impacted targets the same way: check out each revision, `bazel query` the whole
graph (`deps(//...:all-targets)`, streamed proto), hash every target, diff the two hash maps. The
harness drives each tool through its own natural end-to-end flow on one workspace and one
`base..head` pair:

| | bazel-diff | Tango |
|---|---|---|
| Flow | `git checkout base` → `generate-hashes` → `git checkout head` → `generate-hashes --depEdgesFile` → `get-impacted-targets` (the README recipe) | one `get-changed-targets` RPC to Tango's example server, which leases a worker clone, checks out and queries each revision, hashes, and compares |
| Timed as | wall time of the five stages (each also reported) | wall time seen by Tango's example client; the server's own `total_duration` is pulled from its log alongside |
| Includes | its two checkouts | its two checkouts |

Each flow's one-off setup (Tango cloning its origin and worker, both Bazel servers fetching
`bazel_tools` & co.) happens in an untimed warm-up. Then, per iteration:

- **cold** -- `bazel shutdown` for both tools' servers, then run each. Fresh server start plus full
  graph load: what a CI job pays per PR today.
- **warm** -- run each again with the servers alive (incremental re-analysis after the checkouts).
  Tango runs with `-bypass-cache` so it recomputes rather than serving its cached result.
- **diff only** -- no Bazel on either side: Tango with its result cache enabled (a treehash lookup)
  next to bazel-diff's `get-impacted-targets` over the hash files already on disk. Informational.

Tool order alternates every iteration so neither tool consistently runs into a page cache the other
just warmed. Medians are reported; the ratio is Tango / bazel-diff, so above `1.00x` means
bazel-diff was faster.

The impacted sets from the last iteration are compared label by label. The tools' semantics are
close enough that the synthetic workloads agree exactly; on real repositories differences (external
repository handling, generated files) are reported, not failed, unless `--require-match` is given.

## Workloads

Synthetic workspaces come from [`tools/firecracker/bench/gen_project.py`](../firecracker/bench/gen_project.py):
layered genrule DAGs with real source files and no external toolchains, so query time is graph
load, not fetching. Presets (`--workload`):

| Preset | Packages | genrules / package | Rule targets |
|---|---:|---:|---:|
| `tiny` | 60 | 3 | 240 (smoke tests only) |
| `small` | 400 | 4 | 2,000 |
| `medium` | 1,500 | 4 | 7,500 |
| `large` | 4,000 | 4 | 20,000 |
| `xlarge` | 10,000 | 4 | 50,000 |

"Rule targets" counts genrules plus one filegroup per package; the query also sees
`sources_per_package` (3) source files and the genrule outputs per package, plus `bazel_tools`,
so `xlarge` is a ~130k-target `bazel query`. With no workload flags the harness runs the four
nightly presets.
Custom sizes: `--synthetic name:packages:targets_per_package[:layers]`. A real repository:
`--repo-url URL --repo-from HEAD~1 --repo-to HEAD [--repo-clone-args --depth=50]`.

## Running locally

```bash
# Build bazel-diff, clone+build Tango at the pinned commit, run the four presets x 3 iterations.
python3 tools/perf/tango_compare.py --bazel ~/go/bin/bazelisk

# Smoke profile: tiny workload, one iteration, reusing an existing binary and Tango checkout.
python3 tools/perf/tango_compare.py --quick --skip-build --bazel-diff bazel-bin/src/bazel-diff \
    --tango-dir ~/src/tango

# Keep everything on disk and write the report files the workflow publishes.
python3 tools/perf/tango_compare.py --workload medium --iterations 5 \
    --workdir /tmp/tc --keep-workdir --metrics-out metrics.json --summary-out summary.md
```

Requirements: `git`, `go` 1.25+ (or any Go with `GOTOOLCHAIN=auto`, the default the harness sets,
so the right toolchain is fetched), a `bazel`/`bazelisk` binary, Python 3.9+. No third-party
Python. Tango's example server hard-codes `127.0.0.1:8081`; the port must be free.

Tango is pinned to `TANGO_PINNED_REF` in the script so a Tango change never silently moves the
baseline. Compare against another Tango with `--tango-ref <sha|branch|tag>` (the workflow exposes
this as a dispatch input). `--tango-pool-size 2` lets Tango analyse the two revisions in parallel on
two workers -- an architectural advantage it has over bazel-diff's sequential flow -- at the cost of
twice the Bazel servers; the default of 1 keeps the comparison sequential-vs-sequential.

## Output

`--summary-out` is GitHub-flavored markdown (the workflow appends it to `$GITHUB_STEP_SUMMARY`):
the per-workload timing table, bazel-diff's cold stage breakdown, the impacted-set agreement table,
and any failures. `--metrics-out` is JSON with the environment (versions, SHAs of both tools, CPU and
memory), per-phase medians/min/max, every raw sample (bazel-diff stages, Tango server-side
durations), and the full label diff when the tools disagree.

Exit status is non-zero when any run fails, and with `--require-match` also when the tools disagree.

## Unit tests

```bash
bazel test //tools/perf:tango_compare_test
```

The harness is not a `py_binary` (it builds bazel-diff with `bazel build`, which would deadlock under
`bazel run`); the test covers the pure helpers between raw tool output and the summary.

## Cache probe experiment

[`cache_probe_bench.py`](cache_probe_bench.py) is an **experiment, not a feature**. bazel-diff has
no cache-probe mode and the top-level README does not mention one. The
[`cache-probe-bench` workflow](../../.github/workflows/cache-probe-bench.yml) runs this harness
nightly so the decision to add one (or not) rests on trend data rather than on a blog post. The
rule: the mode is not advertised until the cron shows it is **sound** (never misses a test that
ground truth ran) and a **win** on precision or wall time against the hash-based flow.

### The technique under test

Aspect's [`aspect cache diff`](https://aspect.build/blog/selective-testing-with-cache-diff) asks the
remote cache which actions it already holds instead of hashing targets. One `bazel test //...`
under `--experimental_remote_require_cached` executes nothing: every action is either a cache hit
or an `EXECUTION_DENIED` miss, and `--remote_grpc_log` records a `GetActionResult` per probed
action labelled with its target and mnemonic. A test is unaffected when its own `TestRunner`
action hits, because that action's key folds in the digests of its runfiles. Bazel enforces
require-cached only on the remote-execution path, so the probe points `--remote_executor` at
[`cache_probe/dummy_executor`](cache_probe/dummy_executor/main.go), a Go REAPI server that
advertises execution and never receives an `Execute` call (the harness counts them; any call
fails the run).

Two Bazel details the harness had to learn the hard way, both reproduced in its docstring:

- On a warm server Bazel memoizes action nodes and never re-queries the cache for them. The probe
  therefore runs two passes: pass 1 flips `--remote_default_exec_properties`, which changes the
  action-execution salt and drops every action node without re-analysis; pass 2 runs with the
  baseline flags plus `--nouse_action_cache` so each action's lookup uses the baseline's key.
- Seeding must start from `bazel clean`: on a warm output base `bazel test` executes nothing and
  uploads nothing. The harness verifies every seed with a clean-tree probe that must report zero
  misses before it measures anything.

### What is measured

Per synthetic workload (the Tango presets plus two `check_test` targets per package, see
`gen_project.py --tests-per-package`) and per change scenario:

| Scenario | Edit | Why |
|---|---|---|
| `leaf_edit` | one source file ~75% deep in the graph | a PR-sized change |
| `identical_output` | an input whose genrule ignores its content | the action key moves, the output and everything downstream stay byte-identical: bazel-diff must flag the closure, the cache knows better |
| `wide_edit` | a layer-0 source with the most dependents | a shared-library change; stresses the miss frontier and rdeps attribution |
| `partial_leaf_edit`, `partial_identical_output` | the same two changes, selectors run against a cache seeded from only the lower half of the packages | what the probe sees when main's CI does not build everything: a miss then means "changed, or never run on main", and the probe cannot tell which |

Four selectors are scored against **ground truth**, the tests a real `bazel test //...` against
the fully seeded cache actually executes (what CI pays with no selector at all). The `partial_*`
scenarios keep that change-based truth, so recall still means "never misses a changed test" and
precision shows how much each selector overreports under a thinner cache; they also record how
many tests a full run against the partial cache executes (the change's tests plus everything main
never ran). They run one iteration of the warm selectors only, since their point is accuracy.

| Selector | Flow | Timed as |
|---|---|---|
| bazel-diff | checkout base, `generate-hashes`, checkout head, `generate-hashes --depEdgesFile`, `get-impacted-targets`, filtered to test targets | the five stages, cold and warm |
| probe overreport | pass 1 + pass 2, decode the gRPC log, one `tests(rdeps(//..., set(<frontier misses>)))` query | both passes, the decode and the query, cold and warm |
| probe precise | `bazel build //...` with upload (executes the missed non-test actions), then the probe; affected = tests whose own action missed | the build plus the probe |
| probe hybrid | bazel-diff's impacted tests as candidates (recall), then `bazel build` of just those with upload and a probe scoped to just those (precision); affected = candidates whose own action missed | bazel-diff warm plus the build plus the probe, what a serve-backed hybrid would pay |

Recall below 100% is a soundness failure (`--require-sound` makes it fail the run). Precision is
ground truth / flagged. The cache directory is snapshotted after the seed and restored before every
iteration so precise mode's uploads never leak into the next measurement. Medians are reported.

### Running locally

```bash
# Build the fixtures once (bazel-remote at the pinned version, the dummy executor) ...
python3 tools/perf/cache_probe_bench.py --install-fixtures-only --fixture-dir ~/cache-probe-bin

# ... then the smoke profile against an existing bazel-diff binary.
python3 tools/perf/cache_probe_bench.py --quick --skip-build --bazel-diff bazel-bin/src/bazel-diff \
    --fixture-dir ~/cache-probe-bin --bazel ~/go/bin/bazelisk

# The nightly legs, keeping the scratch directory and writing the report files the workflow publishes.
python3 tools/perf/cache_probe_bench.py --workload small --workload medium --iterations 3 \
    --fixture-dir ~/cache-probe-bin --workdir /tmp/cpb --keep-workdir \
    --metrics-out metrics.json --summary-out summary.md --require-sound
```

Requirements: `git`, `go` 1.25+, a `bazel`/`bazelisk` binary, Python 3.9+, stdlib only. All Bazel
commands run with `--nohome_rc` (bazel-diff included, through `--bazelStartupOptions`) so a
developer's own rc never changes action keys or streams to a BES. Unit tests for the pure helpers
(the proto-runtime-free gRPC log decoder, verdicts, result-line classification, accuracy math,
rendering):

```bash
bazel test //tools/perf:cache_probe_bench_test
```

### Known limits of the technique (what the trend has to answer)

- gRPC caches only: `--remote_grpc_log` is a gRPC client interceptor, so an HTTP remote cache or a
  disk cache yields no verdict at all.
- A hit is sound; a miss is ambiguous (changed, never uploaded, or evicted). A sparse cache turns
  into overreporting, never into a missed test.
- Flag parity: the run that fills the cache and the probe must agree on every flag that enters an
  action key.
- Actions tagged `local`, `no-remote` or `no-cache` bypass the gate and execute during the probe.
- The probe's analysis phase is paid in full every time (the salt flip throws away incremental
  execution state); whether that beats two `generate-hashes` runs is exactly what the cold/warm
  columns measure.
- The probe answers "is this result cached", not "did this change touch it". With a fully seeded
  cache the two coincide; the `partial_*` scenarios measure how far they drift apart when they do
  not, and whether bounding the probe by bazel-diff's candidates (hybrid) keeps the drift in check.
