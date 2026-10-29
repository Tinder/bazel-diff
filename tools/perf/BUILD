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

# cache_probe_bench.py (the cache-probe EXPERIMENT behind cache-probe-bench.yml) is not a py_binary
# for the same reason. It imports tango_compare for its shared helpers.
py_library(
    name = "cache_probe_bench_lib",
    srcs = ["cache_probe_bench.py"],
    imports = ["."],
    visibility = ["//visibility:private"],
    deps = [":tango_compare_lib"],
)

py_test(
    name = "cache_probe_bench_test",
    srcs = ["cache_probe_bench_test.py"],
    python_version = "PY3",
    deps = [":cache_probe_bench_lib"],
)

# partial_graph_bench.py (the nightly proof behind partial-graph-bench.yml that `serve
# --partialGraph` is faster and exactly as accurate as the default mode) is not a py_binary for the
# same reason. It imports tango_compare for its shared helpers.
py_library(
    name = "partial_graph_bench_lib",
    srcs = ["partial_graph_bench.py"],
    imports = ["."],
    visibility = ["//visibility:private"],
    deps = [":tango_compare_lib"],
)

py_test(
    name = "partial_graph_bench_test",
    srcs = ["partial_graph_bench_test.py"],
    python_version = "PY3",
    deps = [":partial_graph_bench_lib"],
)
