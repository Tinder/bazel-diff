load("@rules_java//java:defs.bzl", "java_library", "java_test")

java_test(
    name = "bazel-diff-integration-tests",
    runtime_deps = [":bazel-diff-integration-test-lib"],
    test_class = "TestStringGenerator"
)

java_library(
    name = "bazel-diff-integration-test-lib",
    srcs = glob(["*.java"]),
    testonly = True,
    deps = [
        "//integration/src/main/java/com/integration:bazel-diff-integration-lib",
        "@bazel_diff_maven//:junit_junit"
    ]
)
