load("@bazel_diff_crates//:defs.bzl", "all_crate_deps")
load("@rules_rust//rust:defs.bzl", "rust_test")

# The Rust behavioral suite, the Bazel equivalent of `cargo test --test e2e`.
# It drives the real `bazel-diff` binary against real Bazel workspaces, so it is
# an integration test in the same sense as //cli:E2ETest: it shells out to a
# nested `bazel` (see `env_inherit` below), copies fixture workspaces into
# $TEST_TMPDIR, and builds them.
#
# `exclusive` + `local` keep it from running alongside other tests; the
# `--test-threads=1` below is the same serialization cargo ran it with, so
# nested Bazel servers don't fight over one output base.
rust_test(
    name = "e2e_test",
    size = "enormous",
    srcs = [
        "e2e.rs",
        "e2e/core.rs",
        "e2e/external.rs",
        "e2e/regressions.rs",
        "e2e/support/mod.rs",
    ],
    args = ["--test-threads=1"],
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
    # Nested Bazel builds fetch external repos and cannot run under the sandbox.
    tags = [
        "exclusive",
        "external",
        "local",
    ],
    # //:rust_clippy_check and //:rust_format_check name this target directly.
    visibility = ["//:__pkg__"],
    deps = all_crate_deps(
        package_name = "",
        normal = True,
        normal_dev = True,
    ),
)
