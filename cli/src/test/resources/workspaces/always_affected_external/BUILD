# Reproducer workspace for https://github.com/Tinder/bazel-diff/issues/401
#
# The issue: a target that reads files it does NOT declare as inputs (a
# non-hermetic / "undeclared workspace read") gets a stable hash from
# bazel-diff, because hashing is derived purely from the declared graph
# (srcs / deps / attrs). The canonical real-world example is a
# `buildifier_test` that scans every `.bzl` in the repo without declaring
# any of them: edit only a `.bzl` file and the target's hash is unchanged,
# so `get-impacted-targets` skips it -- even though the test would fail if
# actually run.
#
# `scanner` stands in for that buildifier-style target. It is tagged
# `external` (the tag the feature request proposes keying "always-affected"
# behaviour off of) and its action reads `scanned_data.txt`, but that file
# is NOT in `srcs` -- it is an undeclared read.

genrule(
    name = "scanner",
    srcs = [],
    outs = ["scanner_result.txt"],
    # Reads scanned_data.txt at action time without declaring it as an input.
    # This mirrors a linter/buildifier target scanning the workspace.
    cmd = "cat $$(dirname $(location scanner_result.txt))/../../../scanned_data.txt > $@ 2>/dev/null || echo scanned > $@",
    tags = ["external"],
    visibility = ["//visibility:public"],
)

# A normal, hermetic target for contrast: it declares its source, so editing
# `declared_src.txt` DOES change its hash and it IS reported as impacted.
genrule(
    name = "hermetic",
    srcs = ["declared_src.txt"],
    outs = ["hermetic_result.txt"],
    cmd = "cp $< $@",
    visibility = ["//visibility:public"],
)
