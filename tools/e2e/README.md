# E2E test splitting

The e2e suite in this repo used to be a single Bazel target:

| suite | was | now |
| --- | --- | --- |
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
machine CI runs on. Treat CI as **~2.5× slower** than a laptop. On top of that, a
case run as its own target pays several seconds of fixed setup that one process
used to amortise across the whole crate, and it runs alongside other cases that
are each driving a nested Bazel.

The slowest case lands at 126.4s on a 28-core Apple Silicon Mac with warm
caches, and `external::fine_grained_bzlmod_repo_cquery` needs well over 60s
warm, in-process and uncontended, so it could never have fit `short`.

## How many cases run at once

Every case shells out to a nested Bazel, which sizes its own `--jobs` from the
host's core count and tries to use all of it. Untagged, Bazel starts as many
cases as the runner has cores and each one spawns a build that thinks it owns
the machine. On a 28-core Mac that turned a 17s case into a 142s one and made
the two `serve` cases fail outright — they pick a free port, close it, and hand
the number to the server, so enough concurrency and two of them race onto the
same port.

So each case reserves **4 CPUs** (`_CASE_CPUS` in `defs.bzl`), which is a
per-case tag rather than a global `--local_test_jobs` so it travels with the
targets. A 28-core machine runs 7 at a time; a 4-core CI runner runs one; Bazel
clamps the request on anything smaller rather than deadlocking.

What it is deliberately *not* is `exclusive`. Serializing the suite also fixes
the thrash, but it throws away the parallelism that pays for the per-target
setup — the suite spent 640s that way against 250s for the same 38 cases in one
process. With the reservation instead:

| suite | one process | split, `exclusive` | split, `cpu:4` |
| --- | --- | --- | --- |
| `//tests:e2e_test` | 250s | 640s | **213s** |

Only the `_all` target stays `exclusive`: it *is* the whole suite, so there is
nothing to overlap it with.

## Adding an e2e test

Write it wherever it belongs under `tests/e2e/`, then:

```
make regen-e2e
```

That runs `//tools/e2e:regen`, which reads the test sources and rewrites
`rust_e2e_cases.bzl`. `//tools/e2e:defs.bzl` expands that list into targets, so
no BUILD file has to be edited by hand — a new `#[test]` fn gets its target from
the regen alone.

Commit the regenerated file. The `E2E split regen` CI job runs
`//tools/e2e:regen_check`, which re-derives the list and fails if it differs
from what is checked in.

One thing the regen cannot do for you: a *new module* still needs its `mod`
line in `tests/e2e.rs`, because that is what makes rustc compile it at all. The
generator fails with that message rather than silently skipping the file.

## When the default is the wrong budget

Declare the case's own timeout with a marker comment directly above it:

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
//tests:e2e_test_core_something   TIMEOUT in 300.0s
```

To see where every case actually lands before it becomes a failure, run the
suite and read the durations Bazel records per target:

```
bazel test //tests:e2e_test
grep -h 'time=' bazel-testlogs/**/test.xml | sort -t'"' -k2 -gr | head
```

## Running the whole suite in one process

The macro also emits an `_all` target — the original, un-split suite. It is
tagged `manual`, so no wildcard picks it up, and its timeout is `eternal`:

```
bazel test //tests:e2e_test_all
```

`//tests:e2e_test_all` is also what `//:rust_clippy_check` and
`//:rust_format_check` pin as their root, so the crate is linted once rather
than once per case (the per-case targets carry `no-clippy`/`no-rustfmt`).

## How a case is selected

The target passes `--exact <case>` to libtest. libtest exits 0 when a filter
matches nothing, so a stale case name would leave a target that passes *without
running anything*. `//tools/e2e:regen_check` is what rules that out: the
generated names cannot drift from the sources and still merge.

## Files

| file | |
| --- | --- |
| `split_e2e_tests.py` | the generator; `--check` is the CI mode |
| `split_e2e_tests_test.py` | unit tests for its parsers, renderers and error cases |
| `regen_check.py` | `py_test` wrapper around `--check` |
| `defs.bzl` | the macro that expands the case list into targets |
| `rust_e2e_cases.bzl` | **generated** |
