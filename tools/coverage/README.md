# Per-target coverage minimums (`//tools/coverage`)

This package contains a Rust LCOV merger that replaces Bazel's built-in
`@bazel_tools//tools/test:lcov_merger` for this repository, plus Starlark
helpers to declare a **per-target line-coverage minimum** on any test target
that produces coverage.

## How it plugs into Bazel

`.bazelrc` contains:

```
coverage --coverage_output_generator=//tools/coverage:lcov_merger
```

In coverage mode Bazel wraps every test in `collect_coverage.sh`, which
finishes by invoking the configured LCOV merger to combine the raw
per-runner tracefiles (Jacoco emits LCOV for JVM targets, rules_go converts
Go cover profiles to LCOV, rules_rust's llvm-cov toolchain exports LCOV)
into the `coverage.dat` that Bazel publishes for the test. Two properties
fall out of that placement:

1. **Coverage runs only.** Bazel never invokes an LCOV merger for plain
   `bazel test`, so enforcement cannot slow down or fail ordinary test runs.
2. **Per-target `env` is visible.** The merger runs inside the test action,
   so it sees the target's `env` attribute — that is how a target declares
   its minimum, without any global configuration or custom test rules.

One caveat: rules_kotlin hardcodes `kt_jvm_test`'s `_lcov_merger` attribute
to Bazel's built-in merger instead of reading the configuration field that
`--coverage_output_generator` sets, which would silently bypass enforcement
for Kotlin targets. `MODULE.bazel` carries a `single_version_override` patch
([`rules_kotlin_lcov_merger.patch`](rules_kotlin_lcov_merger.patch)) that
makes it use the configuration field, like rules_go/rules_rust/rules_java
already do.

## Declaring a minimum

Wrap any test rule that has the standard `env` attribute (`go_test`,
`rust_test`, `kt_jvm_test`, `java_test`, `py_test`, ...):

```starlark
load("//tools/coverage:defs.bzl", "coverage_enforced_test")
load("@rules_go//go:def.bzl", "go_test")

coverage_enforced_test(
    rule = go_test,
    name = "sample_test",
    srcs = ["sample_test.go"],
    embed = [":sample"],
    coverage_include = ["tools/go/"],
)
```

or splice the env vars into an existing target with `coverage_minimum_env`:

```starlark
load("//tools/coverage:defs.bzl", "coverage_minimum_env")

kt_jvm_test(
    name = "DurationConverterTest",
    ...
    env = coverage_minimum_env(
        coverage_include = ["cli/src/main/kotlin/com/bazel_diff/cli/converter/"],
    ),
)
```

- `min_line_coverage` — minimum overall line coverage (percent, 0–100) of
  the target's merged report. Defaults to **90**. `bazel coverage` fails
  the target below it, with a per-file breakdown in the test log;
  `bazel test` is unaffected.
- `coverage_include` — optional path prefixes scoping which source files
  count. Essential for JVM targets: Jacoco instruments the whole library
  on the test's classpath, so an unscoped percentage would dilute a focused
  unit test's coverage with every other file in the library. Scope each
  target to the code it is responsible for covering.
- `coverage_exclude` — optional path prefixes to drop (e.g. generated code).

Under the hood these become `LCOV_MERGER_MIN_LINE_COVERAGE`,
`LCOV_MERGER_COVERAGE_INCLUDE` and `LCOV_MERGER_COVERAGE_EXCLUDE` in the
target's `env`; only the merger reads them.

## What a failure looks like

```
lcov_merger: line-coverage minimum declared for this target (min 90.00%):

    COV%  LINES (hit/total)  FILE
   66.67%        8 / 12      tools/go/sample/sample.go

Target line coverage: 66.67% (8 / 12 lines)
Required minimum:     90.00%
FAIL: line coverage 66.67% is below the required minimum 90.00%.
```

The test action exits with code 33 (distinct from ordinary failures), the
merged `coverage.dat` is still written, and the combined report
(`--combined_report=lcov`) is built as usual.

## Relationship to the repo-wide gate

`tools/coverage_check.py` still enforces the repo-wide ≥90% floor over the
*combined* report in CI. The per-target minimums here are complementary:
they run per test target, inside the coverage run itself, and catch a
regression in the exact target that introduced it.

## The merger itself

`src/` is a dependency-free Rust crate:

- `args.rs` — the flag contract with `collect_coverage.sh`
  (`--coverage_dir`, `--output_file`, `--filter_sources`,
  `--source_file_manifest`; unknown flags warn instead of failing).
- `lcov.rs` — LCOV parse/merge/emit. Line and function hits are summed
  across tracefiles; `-` branch entries survive only while no run evaluated
  the branch; `LF`/`LH`/`FNF`/`FNH`/`BRF`/`BRH` are recomputed.
- `pattern.rs` — the small full-match regex subset `--filter_sources`
  patterns need (literals, `.`, classes, `*`/`+`/`?`, escapes).
  Unsupported patterns are warned about and skipped.
- `enforce.rs` — reads the env vars above and renders the report.

`bazel test //tools/coverage:lcov_merger_test` runs its unit tests; under
`bazel coverage` the crate enforces a 90% minimum on itself, via itself.
