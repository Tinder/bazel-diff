"""Expands the generated e2e case lists into one Bazel test target per case.

The two e2e suites in this repo used to be one target apiece, each running
dozens of cases in a single process. That made a case's runtime invisible (one
timeout covered the lot), its failures expensive (a re-run re-ran everything)
and its result uncacheable independently. These macros give every case its own
target instead, with its own timeout -- `moderate`, i.e. 300 seconds, unless the
case's source declares otherwise. //tools/e2e:split_e2e_tests.py explains why
that default is not the tighter `short`.

The case lists are generated, never hand-written: see
//tools/e2e:split_e2e_tests.py, `make regen-e2e`, and the //tools/e2e:regen_check
test that fails when they drift from the sources.

Each macro also emits an `<name>_all` target: the original un-split suite,
tagged `manual` so no wildcard picks it up. It is there to run the whole suite
in one process locally (`bazel test //cli:E2ETest_all`) and to give the Rust
lint gates in //BUILD a single crate to check instead of one per case.
"""

load("@rules_kotlin//kotlin:jvm.bzl", "kt_jvm_test")
load("@rules_rust//rust:defs.bzl", "rust_test")

# Backslash first: escaping it after the others would double-escape the
# backslashes they just inserted.
_REGEX_METACHARACTERS = [
    "\\",
    ".",
    "^",
    "$",
    "|",
    "?",
    "*",
    "+",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
]

def _quote_regex(text):
    """Escapes *text* so a Java regex matches it literally."""
    for char in _REGEX_METACHARACTERS:
        text = text.replace(char, "\\" + char)
    return text

# How many CPUs one e2e case reserves from Bazel's local resource pool.
#
# Every case in both suites shells out to a nested Bazel, which sizes its own
# `--jobs` from the host's core count and cheerfully tries to use all of it. Left
# untagged, Bazel schedules as many cases at once as the runner has cores, and
# each of those spawns a nested build that thinks it owns the machine. Measured
# on a 28-core Mac, the JUnit suite run that way turns a 17s case into a 142s one
# and pushes two `serve` cases into failing outright.
#
# A `cpu:` reservation is the fix, and it is deliberately *not* `exclusive`:
# serialization also solves the thrash, but it throws away every bit of
# parallelism along with it -- which is why the Rust suite, tagged `exclusive`,
# takes 640s split against 250s in a single process. Reserving 4 CPUs lets a
# 28-core machine run 7 cases at once and a 4-core CI runner fall back to one at
# a time, without either having to name a concurrency limit anywhere.
#
# Bazel clamps a request larger than the pool down to the pool, so this stays
# correct (one case at a time) on a machine with fewer than 4 cores rather than
# deadlocking.
_CASE_CPUS = 4

def _cpu_tag(cpus):
    """The tag that reserves *cpus* CPUs for a test action."""
    return ["cpu:{}".format(cpus)]

def _junit_filter(test_class, method):
    """The `--test_filter` regex that selects exactly one JUnit method.

    Bazel's JUnit4 runner reads TESTBRIDGE_TEST_ONLY as `--test_filter` and
    applies it with `Matcher.find()` against `<class>#<method>` -- a substring
    match, not a full one. Anchoring is therefore load-bearing: an unanchored
    `...E2ETest#testE2E` also selects testE2EWithNoKeepGoing,
    testE2EIncludingTargetType and testE2EWithTargetType, so that one target
    would quietly run four cases inside the budget meant for one. Escaping
    matters for the same reason: an unescaped `.` matches any character.

    A filter that matches nothing is an error in that runner, so a stale method
    name fails its target rather than passing vacuously.
    """
    return "^{}#{}$".format(_quote_regex(test_class), _quote_regex(method))

def kt_jvm_e2e_tests(suites, env = {}, tags = [], case_cpus = _CASE_CPUS, **kwargs):
    """One kt_jvm_test per `@Test` method, plus a test_suite named after the class.

    Args:
      suites: KOTLIN_E2E_SUITES from //tools/e2e:kotlin_e2e_cases.bzl.
      env: environment for every generated target; TESTBRIDGE_TEST_ONLY is added
        per case on top of it.
      tags: tags for every generated target. The per-case targets also get a
        `cpu:` reservation; the `_all` target gets `manual` and `exclusive`.
      case_cpus: CPUs each case reserves. See _CASE_CPUS.
      **kwargs: forwarded to every kt_jvm_test (data, runtime_deps, jvm_flags,
        ...). `name`, `timeout`, `test_class` and `env` are set by this macro.
    """
    for suite in suites:
        case_targets = []
        for case in suite["cases"]:
            target = "{}_{}".format(suite["name"], case["name"])
            case_targets.append(target)

            case_env = dict(env)
            case_env["TESTBRIDGE_TEST_ONLY"] = _junit_filter(suite["test_class"], case["name"])

            kt_jvm_test(
                name = target,
                timeout = case["timeout"],
                env = case_env,
                tags = tags + _cpu_tag(case_cpus),
                test_class = suite["test_class"],
                **kwargs
            )

        kt_jvm_test(
            name = "{}_all".format(suite["name"]),
            # The point of this target is to run every case in one process, so
            # the per-case cap cannot apply to it. `exclusive` rather than a
            # `cpu:` reservation for the same reason: it *is* the whole suite,
            # so there is nothing to overlap it with.
            timeout = "eternal",
            env = env,
            tags = tags + ["manual", "exclusive"],
            test_class = suite["test_class"],
            **kwargs
        )

        native.test_suite(
            name = suite["name"],
            tests = case_targets,
        )

def rust_e2e_tests(suites, args = [], tags = [], case_cpus = _CASE_CPUS, **kwargs):
    """One rust_test per `#[test]` fn, plus a test_suite named after the suite.

    Each generated target runs its case with libtest's `--exact`, so the binary
    is the same crate the un-split target builds and only the filter differs.

    Unlike the JUnit runner, libtest exits 0 when a filter matches nothing --
    a renamed `#[test]` would leave a target that passes without running
    anything. //tools/e2e:regen_check is what rules that out: it fails the build
    the moment a case name in the generated list stops matching the sources.

    Args:
      suites: RUST_E2E_SUITES from //tools/e2e:rust_e2e_cases.bzl.
      args: libtest arguments for every generated target; `--exact <case>` is
        appended per case.
      tags: tags for every generated target. The per-case targets also get
        `no-clippy`/`no-rustfmt` (the `_all` target compiles the identical
        sources and is what //BUILD pins as the lint root, so linting each case
        target would be the same check run once per case) and a `cpu:`
        reservation. The `_all` target gets `manual` and `exclusive` instead.
      case_cpus: CPUs each case reserves. See _CASE_CPUS.
      **kwargs: forwarded to every rust_test (srcs, deps, data, env, ...).
        `name`, `timeout` and `args` are set by this macro.
    """
    for suite in suites:
        case_targets = []
        for case in suite["cases"]:
            target = "{}_{}".format(suite["name"], case["name"].replace("::", "_"))
            case_targets.append(target)

            rust_test(
                name = target,
                timeout = case["timeout"],
                args = args + ["--exact", case["name"]],
                tags = tags + ["no-clippy", "no-rustfmt"] + _cpu_tag(case_cpus),
                **kwargs
            )

        rust_test(
            name = "{}_all".format(suite["name"]),
            # See the kt_jvm_e2e_tests counterpart: whole-suite target, so the
            # per-case cap cannot apply and `exclusive` is the right reservation.
            timeout = "eternal",
            args = args,
            tags = tags + ["manual", "exclusive"],
            **kwargs
        )

        native.test_suite(
            name = suite["name"],
            tests = case_targets,
        )
