load("@aspect_rules_lint//format:defs.bzl", "format_multirun")

# The fixer for the rustfmt gate. //:rust_format_check only reports (it fails
# the build, it does not rewrite files), so `bazel run //tools/format:rustfmt`
# is how you satisfy it. The daily format cron runs it; buildifier stays on a
# separate cron so Starlark rewrites never land in the rustfmt PR.
#
# `upstream_wrapper:rustfmt` is plain rustfmt from the registered toolchain --
# not //tools/rustfmt, which is a Bazel-target-driven wrapper that does not take
# the file list aspect_rules_lint passes it.
format_multirun(
    name = "rustfmt",
    rust = "@rules_rust//tools/upstream_wrapper:rustfmt",
    visibility = ["//visibility:public"],
)

# buildifier-only formatter for Starlark (BUILD, BUILD.bazel, *.bzl, MODULE.bazel,
# WORKSPACE) files, driven by its own daily cron.
format_multirun(
    name = "buildifier",
    starlark = "@buildifier_prebuilt//:buildifier",
    visibility = ["//visibility:public"],
)
