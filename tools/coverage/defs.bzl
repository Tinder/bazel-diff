"""Per-target line-coverage enforcement for any Bazel test target.

How it works: `.bazelrc` points `--coverage_output_generator` at
`//tools/coverage:lcov_merger`, a Rust drop-in replacement for Bazel's
built-in LCOV merger. Bazel runs that merger inside every test action —
but only under `bazel coverage`, never `bazel test` — to merge the raw
per-runner LCOV tracefiles into the test's `coverage.dat`. Because the
merger runs inside the test action, it sees the target's `env` attribute,
which is the channel these helpers use to declare a minimum:

    load("//tools/coverage:defs.bzl", "coverage_enforced_test")
    load("@rules_go//go:def.bzl", "go_test")

    coverage_enforced_test(
        rule = go_test,
        name = "sample_test",
        srcs = ["sample_test.go"],
        embed = [":sample"],
        min_line_coverage = 90,
        coverage_include = ["tools/go/"],
    )

Any rule with the standard `env` attribute works (`go_test`, `rust_test`,
`kt_jvm_test`, `java_test`, `py_test`, ...). A target whose merged report
falls below its minimum fails the coverage run with the merger's per-file
breakdown in the test log; plain `bazel test` runs are untouched.
"""

MIN_LINE_COVERAGE_ENV = "LCOV_MERGER_MIN_LINE_COVERAGE"
COVERAGE_INCLUDE_ENV = "LCOV_MERGER_COVERAGE_INCLUDE"
COVERAGE_EXCLUDE_ENV = "LCOV_MERGER_COVERAGE_EXCLUDE"

def coverage_minimum_env(min_line_coverage, coverage_include = [], coverage_exclude = []):
    """Returns the `env` entries declaring a line-coverage minimum.

    Use this directly when you cannot (or prefer not to) route a target
    through `coverage_enforced_test`, e.g. to splice into an existing
    `env` dict:

        kt_jvm_test(
            name = "FooTest",
            ...
            env = coverage_minimum_env(85, ["cli/src/main/kotlin/foo/"]),
        )

    Args:
      min_line_coverage: minimum overall line-coverage percentage (0-100)
        for the target's merged LCOV report during `bazel coverage`.
      coverage_include: optional path prefixes; when non-empty, only source
        files starting with one of them count toward the minimum. Use this
        to scope the check to the code the target is responsible for.
      coverage_exclude: optional path prefixes removed from the computation
        (applied after includes), e.g. generated code.

    Returns:
      A dict suitable for (merging into) a test rule's `env` attribute.
    """
    if min_line_coverage < 0 or min_line_coverage > 100:
        fail("min_line_coverage must be within 0-100, got %s" % min_line_coverage)
    env = {MIN_LINE_COVERAGE_ENV: str(min_line_coverage)}
    if coverage_include:
        env[COVERAGE_INCLUDE_ENV] = ",".join(coverage_include)
    if coverage_exclude:
        env[COVERAGE_EXCLUDE_ENV] = ",".join(coverage_exclude)
    return env

def coverage_enforced_test(
        rule,
        name,
        min_line_coverage,
        coverage_include = [],
        coverage_exclude = [],
        **kwargs):
    """Declares `rule(name = name, ...)` with a coverage minimum attached.

    Args:
      rule: any test rule with the standard `env` attribute
        (`go_test`, `rust_test`, `kt_jvm_test`, `py_test`, ...).
      name: forwarded to the rule.
      min_line_coverage: see `coverage_minimum_env`.
      coverage_include: see `coverage_minimum_env`.
      coverage_exclude: see `coverage_minimum_env`.
      **kwargs: every other attribute, forwarded untouched (an existing
        `env` is preserved and augmented).
    """
    env = dict(kwargs.pop("env", {}))
    env.update(coverage_minimum_env(min_line_coverage, coverage_include, coverage_exclude))
    rule(name = name, env = env, **kwargs)
