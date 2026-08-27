# E2E test splitting

Both e2e suites in this repo used to be a single Bazel target apiece:

| suite | was | now |
| --- | --- | --- |
| JUnit, `cli/src/test/kotlin/com/bazel_diff/e2e/` | one `kt_jvm_test`, `timeout = "eternal"` (~1150s) | `//cli:E2ETest`, a `test_suite` over one target per `@Test` method |
| libtest, `tests/e2e/` | one `rust_test`, `size = "enormous"` | `//tests:e2e_test`, a `test_suite` over one target per `#[test]` fn |

One target for a whole suite means one timeout, one cache entry and one log for
everything in it. No individual case has a runtime bound, so a case that creeps
from 5s to 5 minutes is invisible until the suite as a whole blows its ceiling;
a single failure re-runs every case; and a passing case is re-run on every
invocation because the suite's result is cached as a unit.

Split, each case gets its own **300-second** timeout (Bazel's `moderate`), its
own cache entry, and its own log.

### Why 300 and not 60

Because 60 is what a case costs on a fast dev machine, and that is not the
machine CI runs on. Measured on a 28-core Apple Silicon Mac with warm caches,
the JUnit suite runs its 40 cases in ~460s in one process; the same work takes
~1150s on CI's `macos-latest` × Bazel 8.x cell, so treat CI as **~2.5× slower**
than a laptop. On top of that, a case run as its own target pays ~7s of fixed
setup that one process used to amortise across the whole class (`testE2E` is
10.5s in-suite, 17.3s alone), and it runs alongside other cases that are each
driving a nested Bazel.

The slowest case in each suite lands at 130.6s and 126.4s on that Mac. And
`testFineGrainedHashBzlModCquery` needs 75s warm, in-process and uncontended, so
it could never have fit 60s.

## How many cases run at once

Every case shells out to a nested Bazel, which sizes its own `--jobs` from the
host's core count and tries to use all of it. Untagged, Bazel starts as many
cases as the runner has cores and each one spawns a build that thinks it owns
the machine. On this 28-core Mac that turned a 17s case into a 142s one and made
the two `serve` cases fail outright — they pick a port with `ServerSocket(0)`,
close it, and hand the number to the server, so enough concurrency and two of
them race onto the same port.

So each case reserves **4 CPUs** (`_CASE_CPUS` in `defs.bzl`), which is a
per-case tag rather than a global `--local_test_jobs` so it travels with the
targets. A 28-core machine runs 7 at a time; a 4-core CI runner runs one; Bazel
clamps the request on anything smaller rather than deadlocking.

What it is deliberately *not* is `exclusive`. Serializing the suite also fixes
the thrash, but it throws away the parallelism that pays for the per-target
setup — the Rust suite spent 640s that way against 250s for the same 38 cases in
one process. With the reservation instead:

| suite | one process | split, `exclusive` | split, `cpu:4` |
| --- | --- | --- | --- |
| `//cli:E2ETest` | 460s | — | **279s** |
| `//tests:e2e_test` | 250s | 640s | **213s** |

Only the `_all` targets stay `exclusive`: they *are* the whole suite, so there
is nothing to overlap them with.

## Adding an e2e test

Write it wherever it belongs, then:

```
make regen-e2e
```

That runs `//tools/e2e:regen`, which reads the test sources and rewrites
`kotlin_e2e_cases.bzl` and `rust_e2e_cases.bzl`. `//tools/e2e:defs.bzl` expands
those lists into targets, so no BUILD file has to be edited by hand — a new
`@Test` method, a whole new `*E2ETest` class, or a new `#[test]` fn all get
their targets from the regen alone.

Commit the regenerated files. The `E2E split regen` CI job runs
`//tools/e2e:regen_check`, which re-derives the lists and fails if they differ
from what is checked in.

One thing the regen cannot do for you: a *new Rust module* still needs its
`mod` line in `tests/e2e.rs`, because that is what makes rustc compile it at
all. The generator fails with that message rather than silently skipping the
file.

## When the default is the wrong budget

Declare the case's own timeout with a marker comment directly above it. Both
languages use `//` comments, so the spelling is the same:

```kotlin
// e2e-timeout: long
@Test
fun testDownloadsAnAndroidSdkFirst() { ... }
```

```rust
// e2e-timeout: long
#[test]
fn downloads_an_android_sdk_first() { ... }
```

The value is a Bazel timeout: `short` (60s), `moderate` (300s, the default),
`long` (900s) or `eternal` (3600s). Anything else fails the regen. A marker
reaching *up* is a note that the case is worth splitting or speeding up, not the
normal way to add a test; a marker reaching *down* to `short` is how a case that
really is quick gets held to a bound worth having.

### Finding the cases that need one

Bazel names them for you. A case over budget fails with

```
//cli:E2ETest_testSomething   TIMEOUT in 300.0s
```

To see where every case actually lands before it becomes a failure, run the
suite and read the durations Bazel records per target:

```
bazel test //cli:E2ETest //tests:e2e_test
grep -h 'time=' bazel-testlogs/**/test.xml | sort -t'"' -k2 -gr | head
```

## Running the whole suite in one process

Each macro also emits an `_all` target — the original, un-split suite. It is
tagged `manual`, so no wildcard picks it up, and its timeout is `eternal`:

```
bazel test //cli:E2ETest_all
bazel test //tests:e2e_test_all
```

`//tests:e2e_test_all` is also what `//:rust_clippy_check` and
`//:rust_format_check` pin as their root, so the crate is linted once rather
than once per case (the per-case targets carry `no-clippy`/`no-rustfmt`).

## How a case is selected

The two runners filter differently, and the difference matters:

- **JUnit.** The target sets `TESTBRIDGE_TEST_ONLY`, which Bazel's JUnit4 runner
  reads as `--test_filter` and applies with `Matcher.find()` against
  `<class>#<method>` — a *substring* match. The generated filter is therefore
  anchored (`^…$`) and its metacharacters escaped; unanchored,
  `…E2ETest#testE2E` would also select `testE2EWithNoKeepGoing`,
  `testE2EIncludingTargetType` and `testE2EWithTargetType`. A filter that
  matches nothing is an error in that runner, so a stale method name fails.
- **libtest.** The target passes `--exact <case>`. libtest exits 0 when a filter
  matches nothing, so a stale case name would leave a target that passes
  *without running anything*. `//tools/e2e:regen_check` is what rules that out:
  the generated names cannot drift from the sources and still merge.

## Files

| file | |
| --- | --- |
| `split_e2e_tests.py` | the generator; `--check` is the CI mode |
| `split_e2e_tests_test.py` | unit tests for its parsers, renderers and error cases |
| `regen_check.py` | `py_test` wrapper around `--check` |
| `defs.bzl` | the macros that expand the case lists into targets |
| `kotlin_e2e_cases.bzl` | **generated** |
| `rust_e2e_cases.bzl` | **generated** |
