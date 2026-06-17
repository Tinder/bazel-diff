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
