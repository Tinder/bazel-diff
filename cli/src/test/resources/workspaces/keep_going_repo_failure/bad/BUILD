# This package loads a .bzl file from @failing_dep. Resolving that load forces Bazel
# to fetch the @failing_dep repo, whose repository rule always fails (see //:ext.bzl).
# Loading this package therefore errors out. With `--keep_going` bazel query reports
# the error but returns partial results (exit code 3), which bazel-diff currently
# treats as success -- so //bad silently disappears from the hashes. With
# `--no-keep_going`, bazel query fails outright and bazel-diff surfaces the error.
load("@failing_dep//:defs.bzl", "noop")

noop(name = "bad")
