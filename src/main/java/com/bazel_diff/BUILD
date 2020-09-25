load("@rules_java//java:defs.bzl", "java_binary", "java_library", "java_proto_library")
load("@rules_proto//proto:defs.bzl", "proto_library")

BAZEL_DIFF_VERSION = "1.0.0" # Update this when updating version!

java_binary(
    name = "bazel-diff",
    main_class = "BazelDiff",
    runtime_deps = [":java-bazel-diff-lib"],
    jvm_flags = ["-DBAZEL_DIFF_VERSION=%s" % BAZEL_DIFF_VERSION],
    visibility = ["//visibility:public"]
)

java_library(
    name = "java-bazel-diff-lib",
    srcs = glob(["*.java"]),
    deps = [
        ":build_java_proto",
        "@com_google_protobuf//:protobuf_java",
        "@bazel_diff_maven//:info_picocli_picocli",
        "@bazel_diff_maven//:com_google_code_gson_gson",
        "@bazel_diff_maven//:com_google_guava_guava"
    ],
    visibility = ["//src/test/java/com/bazel_diff:__pkg__"]
)

java_proto_library(
    name = "build_java_proto",
    deps = [":build_proto"],
)

proto_library(
    name = "build_proto",
    srcs = [":build_proto_gen"]
)

genrule(
    name = "build_proto_gen",
    srcs = ["@build_proto_message//file"],
    outs = ["build.proto"],
    cmd = "cp $< $@"
)
