"""Macro under test for https://github.com/Tinder/bazel-diff/issues/259 / #227.

`miniature` is a thin wrapper around `native.genrule`. Issue #259's user reported that
on Bazel 7+, editing this macro (e.g. adding a `print()` call) no longer caused targets
that call the macro to be reported by `bazel-diff get-impacted-targets`. Same shape as
issue #227 where editing a shared .bzl file does not propagate to BUILD files that
`load()` it.
"""

def miniature(name, src, **kwargs):
    native.genrule(
        name = name,
        srcs = [src],
        outs = ["small_" + src],
        # Placeholder for `convert ... -resize`: just copy the file. The cmd string is
        # what the issue reporter mutated when adding a `print()` to the macro body.
        cmd = "cp $< $@",
        **kwargs
    )
