load("@rules_shell//shell:sh_test.bzl", "sh_test")

# A leaf test package that depends on //keep:keep. The e2e test deletes this
# whole package between the two revisions it diffs: nothing depends on it, so
# its labels must never be reported as impacted (there is nothing left at the
# "to" revision to build or test).
sh_test(
    name = "keep_tests",
    srcs = ["keep_tests.sh"],
    deps = ["//keep"],
)
