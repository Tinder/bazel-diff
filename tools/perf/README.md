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
