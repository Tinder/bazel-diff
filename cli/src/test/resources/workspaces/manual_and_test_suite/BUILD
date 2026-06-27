load("@rules_shell//shell:sh_library.bzl", "sh_library")
load("@rules_shell//shell:sh_test.bzl", "sh_test")

sh_library(
    name = "lib",
    srcs = ["lib.sh"],
)

# A normal test that depends on the shared library.
sh_test(
    name = "regular_test",
    srcs = ["test.sh"],
    deps = [":lib"],
)

# A test tagged "manual". Issue #392 asks for a way to exclude these from the
# impacted-targets output; today bazel-diff has no such filter, so it shows up
# whenever its dependency changes.
sh_test(
    name = "manual_test",
    srcs = ["test.sh"],
    tags = ["manual"],
    deps = [":lib"],
)

# A test_suite. Issue #392 also reports that changing a single test in a suite
# surfaces the wrapping test_suite target as impacted, with no way to exclude it.
test_suite(
    name = "all_tests",
    tests = [":regular_test"],
)
