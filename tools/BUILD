load("@rules_python//python:defs.bzl", "py_binary", "py_library", "py_test")

genrule(
    name = "cli_help_output",
    outs = [
        "help_root.txt",
        "help_generate_hashes.txt",
        "help_get_impacted_targets.txt",
        "help_serve.txt",
    ],
    cmd = """
        BINARY=$(location //cli:bazel-diff)
        $$BINARY --help > $(location help_root.txt) 2>&1 || true
        $$BINARY generate-hashes --help > $(location help_generate_hashes.txt) 2>&1 || true
        $$BINARY get-impacted-targets --help > $(location help_get_impacted_targets.txt) 2>&1 || true
        $$BINARY serve --help > $(location help_serve.txt) 2>&1 || true
    """,
    tools = ["//cli:bazel-diff"],
)

py_binary(
    name = "generate-readme",
    srcs = ["generate_readme.py"],
    data = [
        "readme_template.md",
        ":cli_help_output",
    ],
    main = "generate_readme.py",
    python_version = "PY3",
)

# Coverage threshold enforcement: a py_library so both the runnable binary and the
# unit-test target can share the same source. Running the unit tests via
# `bazel coverage` instruments the library's source and contributes its coverage to
# the combined LCOV report, so the enforcement code itself participates in the gate.
py_library(
    name = "coverage_check_lib",
    srcs = ["coverage_check.py"],
    imports = ["."],
    visibility = ["//visibility:private"],
)

py_binary(
    name = "coverage-check",
    srcs = ["coverage_check.py"],
    main = "coverage_check.py",
    python_version = "PY3",
    visibility = ["//visibility:public"],
)

py_test(
    name = "coverage_check_test",
    srcs = ["coverage_check_test.py"],
    python_version = "PY3",
    deps = [":coverage_check_lib"],
)

# The benchmark shells out to Bazel, so only its pure parsing/statistics helpers run under
# `bazel test`. Invoke the real benchmark directly to avoid nesting Bazel under Bazel.
py_library(
    name = "benchmark_lib",
    srcs = ["benchmark.py"],
    imports = ["."],
    visibility = ["//visibility:private"],
)

py_test(
    name = "benchmark_test",
    srcs = ["benchmark_test.py"],
    python_version = "PY3",
    deps = [":benchmark_lib"],
)

# Note: the serve integration harnesses (serve_harness.py, serve_stress.py, serve_consistency.py)
# are intentionally NOT wrapped in py_binary targets. They shell out to
# `bazel build //cli:bazel-diff`, which would deadlock on the output-base lock if they were
# themselves launched via `bazel run` (nested bazel, same output base). Run them directly:
# `python3 tools/serve_harness.py` / `python3 tools/serve_stress.py` /
# `python3 tools/serve_consistency.py` (see their module docstrings). Python stdlib only.
#
# A py_test over their *pure* helpers is fine, though -- it imports the module and calls functions
# that never shell out, so no nested bazel and no deadlock. serve_stress_test.py covers the CLI
# parsing that the serve-stress-real cron depends on (a flag-shaped --real-clone-args value, which
# argparse rejects unless folded into the `=` spelling; that broke the cron's first two runs).
py_library(
    name = "serve_stress_lib",
    srcs = [
        "serve_harness.py",  # imported by serve_stress as `base`
        "serve_stress.py",
    ],
    imports = ["."],
    visibility = ["//visibility:private"],
)

py_test(
    name = "serve_stress_test",
    srcs = ["serve_stress_test.py"],
    python_version = "PY3",
    deps = [":serve_stress_lib"],
)

# mock_s3.py is the one harness-adjacent module that never invokes bazel or git, so its own test
# can drive the live HTTP server as well as the pure helpers.
py_library(
    name = "mock_s3_lib",
    srcs = ["mock_s3.py"],
    imports = ["."],
    visibility = ["//visibility:private"],
)

py_test(
    name = "mock_s3_test",
    srcs = ["mock_s3_test.py"],
    python_version = "PY3",
    deps = [":mock_s3_lib"],
)

# serve_consistency_test.py covers the payload comparator that decides whether a fleet's hashes
# agree; a false negative there would report consistency that was never actually checked.
py_library(
    name = "serve_consistency_lib",
    srcs = [
        "mock_s3.py",
        "serve_consistency.py",
        "serve_harness.py",  # imported by serve_consistency as `base`
    ],
    imports = ["."],
    visibility = ["//visibility:private"],
)

py_test(
    name = "serve_consistency_test",
    srcs = ["serve_consistency_test.py"],
    python_version = "PY3",
    deps = [":serve_consistency_lib"],
)
