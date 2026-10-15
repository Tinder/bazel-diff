load("@rules_python//python:defs.bzl", "py_library", "py_test")

# tango_compare.py is intentionally NOT a py_binary, for the same reason as the serve harnesses in
# //tools: by default it shells out to `bazel build //src:bazel-diff`, which would deadlock on the
# output-base lock if the harness were itself launched via `bazel run`. Run it directly:
# `python3 tools/perf/tango_compare.py` (see its module docstring and README.md). Python stdlib only.
#
# Its pure helpers (output decoding, timing math, summary rendering, CLI parsing) never shell out, so
# a py_test over them is safe and runs in CI with the rest of //tools/....
py_library(
    name = "tango_compare_lib",
    srcs = ["tango_compare.py"],
    imports = ["."],
    visibility = ["//visibility:private"],
)

py_test(
    name = "tango_compare_test",
    srcs = ["tango_compare_test.py"],
    python_version = "PY3",
    deps = [":tango_compare_lib"],
)
