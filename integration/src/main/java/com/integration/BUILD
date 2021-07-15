load("@rules_java//java:defs.bzl", "java_library")

java_library(
    name = "bazel-diff-integration-lib",
    srcs = glob(["*.java"]),
    deps = [
        "//integration/src/main/java/com/integration/submodule:Submodule"
    ],
    visibility = ["//visibility:public"]
)
