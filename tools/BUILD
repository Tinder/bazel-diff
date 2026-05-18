load("@rules_python//python:defs.bzl", "py_binary", "py_library", "py_test")

genrule(
    name = "cli_help_output",
    outs = [
        "help_root.txt",
        "help_generate_hashes.txt",
        "help_get_impacted_targets.txt",
    ],
    cmd = """
        BINARY=$(location //cli:bazel-diff)
        $$BINARY --help > $(location help_root.txt) 2>&1 || true
        $$BINARY generate-hashes --help > $(location help_generate_hashes.txt) 2>&1 || true
        $$BINARY get-impacted-targets --help > $(location help_get_impacted_targets.txt) 2>&1 || true
    """,
    tools = ["//cli:bazel-diff"],
)

py_binary(
    name = "generate-readme",
    srcs = ["generate_readme.py"],
    main = "generate_readme.py",
    data = [
        ":cli_help_output",
        "readme_template.md",
    ],
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
