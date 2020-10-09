load("@rules_java//java:defs.bzl", "java_library", "java_test")

java_test(
    name = "java-bazel-diff-tests",
    runtime_deps = [":java-bazel-diff-test-lib"],
    test_class = "com.bazel_diff.TargetHashingClientImplTests"
)

java_library(
    name = "java-bazel-diff-test-lib",
    srcs = glob(["*.java"]),
    deps = [
        "//src/main/java/com/bazel_diff:java-bazel-diff-lib",
        "@bazel_diff_maven//:org_mockito_mockito_core",
        "@bazel_diff_maven//:junit_junit"
    ]
)
