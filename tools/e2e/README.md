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

Split, each case gets its own **60-second** timeout (Bazel's `short`), its own
cache entry, and its own log.

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

## When 60 seconds is not enough

Declare the case's own timeout with a marker comment directly above it. Both
languages use `//` comments, so the spelling is the same:

```kotlin
// e2e-timeout: moderate
@Test
fun testDownloadsAnAndroidSdkFirst() { ... }
```

```rust
// e2e-timeout: moderate
#[test]
fn downloads_an_android_sdk_first() { ... }
```

The value is a Bazel timeout: `short` (60s, the default), `moderate` (300s),
`long` (900s) or `eternal` (3600s). Anything else fails the regen. Treat a
marker as a note that the case is worth splitting or speeding up, not as the
normal way to add a test.

### Finding the cases that need one

Bazel names them for you. A case over budget fails with

```
//cli:E2ETest_testSomething   TIMEOUT in 60.0s
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
