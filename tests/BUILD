load("@bazel_diff_crates//:defs.bzl", "all_crate_deps")
load("//tools/e2e:defs.bzl", "rust_e2e_tests")
load("//tools/e2e:rust_e2e_cases.bzl", "RUST_E2E_SUITES")

# The Rust behavioral suite, the Bazel equivalent of `cargo test --test e2e`.
# It drives the real `bazel-diff` binary against real Bazel workspaces, so it is
# an integration test in the same sense as //cli:E2ETest: it shells out to a
# nested `bazel` (see `env_inherit` below), copies fixture workspaces into
# $TEST_TMPDIR, and builds them.
#
# One Bazel target per `#[test]` fn rather than one for the crate: the suite used
# to be a single size = "enormous" target, so no individual case had a runtime
# bound of its own and one failure re-ran all of them. Each generated target runs
# its case with libtest's `--exact` under a 60s timeout.
#
# RUST_E2E_SUITES is generated from the sources by `make regen-e2e`, so adding a
# `#[test]` fn under tests/e2e/ creates its target with no edit here
# (a new *module* still needs its `mod` line in e2e.rs, and the generator fails
# with that message if it is missing). //tools/e2e:regen_check fails CI when the
# generated list and the sources drift.
#
# `exclusive` + `local` are inherited from the un-split suite: the cases spawn
# nested Bazel servers, which must not run under the sandbox and are too heavy to
# run several at a time. Splitting is for the per-case timeout, cache entry and
# log, not for concurrency.
rust_e2e_tests(
    srcs = glob([
        "e2e.rs",
        "e2e/**/*.rs",
    ]),
    crate_root = "e2e.rs",
    data = [
        "//cli:e2e_resources",
        "//src:bazel-diff",
    ],
    edition = "2021",
    env = {
        # Runfiles-relative; support::binary() makes it absolute. `rootpath`
        # rather than `execpath` because the test reads it at runtime.
        "BAZEL_DIFF_TEST_BINARY": "$(rootpath //src:bazel-diff)",
    },
    env_inherit = [
        # Which nested Bazel to run, and what it needs to run. Set $BAZEL: the
        # suite picks it up first (see support::bazel()), and without it the
        # choice falls to whichever `bazel` PATH happens to yield -- under
        # `bazel test` that is the outer Bazel's own binary, so the suite would
        # silently test a different version than .bazelversion or
        # USE_BAZEL_VERSION names. Several fixtures are version-sensitive.
        "BAZEL",
        "HOME",
        "JAVA_HOME",
        "PATH",
        "USE_BAZEL_VERSION",
    ],
    suites = RUST_E2E_SUITES,
    # Nested Bazel builds fetch external repos and cannot run under the sandbox.
    tags = [
        "exclusive",
        "external",
        "local",
    ],
    # //:rust_clippy_check and //:rust_format_check name :e2e_test_all, the
    # un-split target this macro also emits, so they check the crate once
    # instead of once per case.
    visibility = ["//:__pkg__"],
    deps = all_crate_deps(
        package_name = "",
        normal = True,
        normal_dev = True,
    ),
)

# The Rust e2e sources //tools/e2e:regen_check re-derives the generated case
# list from -- the crate root included, since the generator checks that every
# module holding a `#[test]` is declared there.
filegroup(
    name = "e2e_test_sources",
    srcs = glob([
        "e2e.rs",
        "e2e/**/*.rs",
    ]),
    visibility = ["//tools/e2e:__pkg__"],
)
