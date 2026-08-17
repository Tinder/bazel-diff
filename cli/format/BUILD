load("@aspect_rules_lint//format:defs.bzl", "format_multirun")

format_multirun(
    name = "format",
    kotlin = "//cli:ktfmt",
    visibility = ["//visibility:public"],
)

# buildifier-only formatter for Starlark (BUILD, BUILD.bazel, *.bzl, MODULE.bazel,
# WORKSPACE) files, driven by its own daily cron. Kept separate from the kotlin
# `format` target so the two crons don't reformat each other's files.
format_multirun(
    name = "buildifier",
    starlark = "@buildifier_prebuilt//:buildifier",
    visibility = ["//visibility:public"],
)

# The fixer for the rustfmt gate. //:rust_format_check only reports (it fails
# the build, it does not rewrite files), so `bazel run //cli/format:rustfmt` is
# how you satisfy it. Kept separate from `format` for the same reason
# `buildifier` is: one language per target, so a cron on one never rewrites
# another's files.
#
# `upstream_wrapper:rustfmt` is plain rustfmt from the registered toolchain --
# not //tools/rustfmt, which is a Bazel-target-driven wrapper that does not take
# the file list aspect_rules_lint passes it.
format_multirun(
    name = "rustfmt",
    rust = "@rules_rust//tools/upstream_wrapper:rustfmt",
    visibility = ["//visibility:public"],
)
